//! A second node in the mesh, on a desktop.
//!
//! M2 asks for two devices that discover each other, elect a timebase and change
//! colour on the same frame. This is the second one — the same
//! `lumen_device::node::Node` the firmware runs, on a machine with a keyboard
//! attached so the result can be read rather than inferred from a strip.
//!
//! It is not a substitute for a second ESP32 and does not pretend to be: it
//! shares no clock hardware with the device, no radio, and not even a CPU
//! architecture. That is what makes it a useful peer rather than a convenient
//! one — two implementations of nothing agreeing proves nothing, and this runs
//! the identical state machine over the identical wire format on x86-64 against
//! RISC-V.
//!
//! # What it demonstrates
//!
//! - **Election**: whichever of the two has more capacity leads, and the other
//!   yields. Give this peer a higher capacity and the device should hand over.
//! - **Sync**: the follower probes, filters 32 round trips by RTT, and
//!   disciplines its clock.
//! - **Agreement**: both render the same effect at their own show time and
//!   report a fingerprint. Equal fingerprints for the same moment means two
//!   machines with unrelated clocks produced identical light.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use lumen_device::node::Node as MeshNode;
use lumen_device::{Action, Destination, Event, Identity, Role};
use lumen_proto::Uuid;

/// The frame grid every node in the mesh draws on, matching the firmware's.
const FRAME_US: u64 = 33_333;

/// The mesh, as the core wants it. Its first two bytes are the wire prefix.
const MESH_UUID: [u8; 16] = [
    0x4c, 0x4d, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// A slewing show clock, the same rule the firmware follows: never step.
///
/// Duplicated rather than shared because the firmware's lives in a `no_std`
/// binary and this is forty lines. If it grows a third copy it belongs in
/// `lumen-device`.
struct ShowClock {
    started: Instant,
    /// Show microseconds at the last advance.
    now_us: u64,
    /// Hardware microseconds at the last advance.
    last_raw_us: u64,
    pending_us: i64,
    /// Slew budget below a whole microsecond, carried rather than discarded.
    /// Without it the integer division rounds every short interval's budget to
    /// zero and the correction never arrives.
    slew_numer: i64,
}

impl ShowClock {
    const SLEW_PPM: i64 = 200;
    /// Beyond this a correction is applied at once: a node joining a running
    /// show has not drifted, it did not know the time.
    const STEP_ABOVE_US: i64 = 100_000;

    fn new() -> ShowClock {
        ShowClock {
            started: Instant::now(),
            now_us: 0,
            last_raw_us: 0,
            pending_us: 0,
            slew_numer: 0,
        }
    }

    fn advance(&mut self) -> u64 {
        let raw = self.started.elapsed().as_micros() as u64;
        let elapsed = raw.saturating_sub(self.last_raw_us);
        self.last_raw_us = raw;

        self.slew_numer += (elapsed as i64).saturating_mul(Self::SLEW_PPM);
        let budget = self.slew_numer / 1_000_000;
        self.slew_numer -= budget * 1_000_000;
        let applied = self.pending_us.clamp(-budget, budget);
        self.pending_us -= applied;
        self.now_us = self
            .now_us
            .saturating_add_signed(elapsed as i64 + applied)
            .max(self.now_us);
        self.now_us
    }

    fn discipline(&mut self, offset_us: i64) {
        let total = self.pending_us.saturating_add(offset_us);
        if total.abs() > Self::STEP_ABOVE_US {
            self.now_us = self.now_us.saturating_add_signed(total).max(self.now_us);
            self.pending_us = 0;
            self.slew_numer = 0;
        } else {
            self.pending_us = total;
        }
    }
}

/// Run as a peer until interrupted.
///
/// `capacity` decides who leads. The firmware reports 1194 for a C3 and 1714 for
/// an S3 — pass something higher to take the mesh over, or lower to follow it.
#[allow(clippy::too_many_arguments)]
pub fn run(
    capacity: u32,
    port: u16,
    play: Option<Vec<u8>>,
    leds: u16,
    alert: Option<Vec<u8>>,
    alert_every: Duration,
    alert_lasts_us: u64,
    http_port: Option<u16>,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(2)))?;

    // An identity that is stable for this process and cannot collide with a
    // device's, which is derived from a MAC.
    let mut id = [0u8; 16];
    id[0] = 0xDE;
    id[1] = 0x5C;
    let pid = std::process::id();
    id[2..6].copy_from_slice(&pid.to_le_bytes());

    let mut clock = ShowClock::new();
    let mut mesh = MeshNode::new(
        Identity::new(Uuid(id), capacity),
        Uuid(MESH_UUID),
        0,
        clock.advance(),
    );

    let mut peers: Vec<([u8; 4], Ipv4Addr)> = Vec::new();
    let mut deadline_us = 0u64;
    let mut role = Role::Follower;
    let mut synced = false;
    let mut last_report = Instant::now();
    let mut seen_ticks = 0u32;
    let (mut skew_samples, mut skew_total, mut worst_skew_us) = (0u64, 0i64, 0i64);
    let mut differ = 0u64;
    let (mut agree_w, mut differ_w) = (0u64, 0u64);
    let mut worst_stale_us = 0i64;
    let (mut frame_samples, mut frame_drift_total, mut frame_drift_worst) = (0u64, 0i64, 0i64);

    println!(
        "peer up: capacity {capacity}, uuid {:02x}{:02x}{:02x}{:02x}…, port {port}",
        id[0], id[1], id[2], id[3]
    );
    println!("(a device reports 1194 for a C3, 1714 for an S3)");

    // Rendering the same effect as the device, on the same frame grid, so the
    // two can be compared frame for frame rather than by eye.
    let mut live = play
        .as_ref()
        .and_then(|bytecode| crate::simulate::Live::new(bytecode.clone(), leds));
    let mut last_frame = None;

    // And giving the device that same effect, because two nodes drawing
    // different things prove nothing about a shared timebase. One process, so
    // there is one socket on the port.
    let mut sequence = 0u32;
    // An alert over the show, on a timer, so the source stack can be watched
    // doing what it is for: a higher priority winning every pixel, and the show
    // still underneath when the alert expires by itself.
    let mut next_alert = Instant::now() + alert_every;
    let mut provisioned: Option<SocketAddr> = None;

    // Anything that can `curl` can now light the room. The endpoint only asks;
    // the mesh loop decides, so nothing on that thread touches the socket or
    // the node.
    let (http_tx, http_rx) = std::sync::mpsc::channel::<crate::http::Request>();
    if let Some(port) = http_port {
        crate::http::serve(port, http_tx);
    }

    let mut buf = [0u8; 1500];
    loop {
        let now = clock.advance();

        if now >= deadline_us {
            let actions = mesh.on_event(now, Event::Tick);
            deadline_us = apply(
                &actions,
                now,
                &socket,
                &peers,
                port,
                &mut clock,
                &mut role,
                &mut synced,
            );
        }

        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                // What actually arrives, by type and sender. Five times this
                // session a symptom has pointed at the wrong layer, and every
                // one was settled by instrumenting the boundary rather than the
                // suspect.
                // The definitive comparison: what the sender's clock said when
                // it sent, against what this one says now. Same instant, one
                // number - two logs from different windows cannot answer this.
                if n >= 24 {
                    let prefix = [buf[6], buf[7], buf[8], buf[9]];
                    if prefix != id[..4] {
                        // Bytes 14..22. The header is magic, version, type,
                        // flags, mesh(2), sender(4), sequence(4), then the show
                        // time - reading it two bytes late produced a skew of
                        // 1.2e16 microseconds, which is a number worth
                        // recognising as an offset error rather than a clock.
                        let theirs = u64::from_le_bytes([
                            buf[14], buf[15], buf[16], buf[17], buf[18], buf[19], buf[20], buf[21],
                        ]);
                        let skew = now as i64 - theirs as i64;
                        if skew.abs() > worst_skew_us {
                            worst_skew_us = skew.abs();
                        }
                        skew_samples += 1;
                        skew_total += skew;
                    }
                }
                if n >= 10 {
                    let kind = buf[2];
                    let prefix = [buf[6], buf[7], buf[8], buf[9]];
                    if kind != 0x10 || seen_ticks < 3 {
                        println!(
                            "  rx {n:>3}B type {kind:#04x} from {from} prefix {:02x}{:02x}{:02x}{:02x}",
                            prefix[0], prefix[1], prefix[2], prefix[3]
                        );
                    }
                    if kind == 0x10 {
                        seen_ticks += 1;
                    }
                }
                if n >= 10 {
                    if let SocketAddr::V4(v4) = from {
                        let prefix = [buf[6], buf[7], buf[8], buf[9]];
                        if let Some(slot) = peers.iter_mut().find(|(p, _)| *p == prefix) {
                            slot.1 = *v4.ip();
                        } else {
                            peers.push((prefix, *v4.ip()));
                        }
                    }
                }
                // A device announcing what it drew. Render the same frame index
                // and compare: matching fingerprints mean the two nodes produced
                // the same picture, and the index difference says whether they
                // produced it at the same moment. Neither question can be
                // answered from two logs taken at different instants.
                if n >= 36 && buf[0] == 0xA5 && buf[1] == 1 {
                    if let Some(l) = live.as_mut() {
                        let theirs_index = u64::from_le_bytes(
                            buf[2..10].try_into().expect("eight bytes"),
                        );
                        let theirs_digest = u64::from_le_bytes(
                            buf[10..18].try_into().expect("eight bytes"),
                        );
                        // Their clock when they sent, so staleness and
                        // disagreement can be separated.
                        let theirs_now = u64::from_le_bytes(
                            buf[18..26].try_into().expect("eight bytes"),
                        );
                        if theirs_index > 0 {
                            let mine = l.frame(theirs_index * FRAME_US);
                            let same_picture = mine == theirs_digest;
                            // How far apart the two clocks put the same moment,
                            // in frames - which is the question M2 asks. The
                            // report's own age is excluded: that is this node's
                            // and the network's latency, not the mesh's.
                            let drift =
                                theirs_now as i64 / FRAME_US as i64 - theirs_index as i64;
                            let stale = now as i64 - theirs_now as i64;
                            if stale.abs() > worst_stale_us {
                                worst_stale_us = stale.abs();
                            }
                            if same_picture {
                                agree_w += 1;
                            } else {
                                differ += 1;
                                differ_w += 1;
                            }
                            frame_drift_total += drift;
                            frame_drift_worst = frame_drift_worst.max(drift.abs());
                            frame_samples += 1;
                            if !same_picture && differ <= 3 {
                                println!(
                                    "  frame #{theirs_index}: device {theirs_digest:016x}, peer {mine:016x} - DIFFERENT"
                                );
                            }
                        }
                    }
                }

                // Any announcement tells us where a device is. Learning the
                // address only while *giving* one a program meant a device that
                // already held one could never be sent an alert - which is the
                // case every time this reconnects to a running mesh.
                if n >= 2 && buf[0] == 0xA5 {
                    if let SocketAddr::V4(v4) = from {
                        provisioned = Some(SocketAddr::V4(SocketAddrV4::new(*v4.ip(), port)));
                    }
                }

                // A device announcing that it holds nothing gets the effect.
                if n >= 2 && buf[0] == 0xA5 && buf[1] == 0 {
                    if let (Some(code), SocketAddr::V4(v4)) = (&play, from) {
                        let to = SocketAddr::V4(SocketAddrV4::new(*v4.ip(), port));
                        if crate::provision(&socket, to, code, &mut sequence, now).is_ok() {
                            println!("  gave {} the effect", v4.ip());
                        }
                    }
                    continue;
                }

                let actions = mesh.on_event(now, Event::Datagram { bytes: &buf[..n] });
                if !actions.is_empty() {
                    deadline_us = apply(
                        &actions,
                        now,
                        &socket,
                        &peers,
                        port,
                        &mut clock,
                        &mut role,
                        &mut synced,
                    );
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }

        if let Some(l) = live.as_mut() {
            let index = now / FRAME_US;
            if last_frame.map(|(i, _)| i) != Some(index) {
                last_frame = Some((index, l.frame(index * FRAME_US)));
            }
        }

        // Whatever HTTP asked for, carried out here rather than on its thread.
        while let Ok(request) = http_rx.try_recv() {
            match request {
                crate::http::Request::Alert(seconds) => {
                    if let (Some(code), Some(to)) = (&alert, provisioned) {
                        if crate::push_program(
                            &socket,
                            to,
                            code,
                            &mut sequence,
                            now,
                            1,
                            230,
                            seconds * 1_000_000,
                            [9; 16],
                        )
                        .is_ok()
                        {
                            println!("  HTTP: alert for {seconds} s");
                        }
                    } else {
                        println!("  HTTP: asked for an alert with no device or no --alert effect");
                    }
                }
                crate::http::Request::Off => {
                    if let Some(to) = provisioned {
                        let _ = crate::send(
                            &socket,
                            to,
                            lumen_proto::header::MsgType::SrcPop,
                            &mut sequence,
                            now,
                            |w| {
                                lumen_proto::msg::SrcPop {
                                    source_id: Uuid([7; 16]),
                                    fade_out_ms: 400,
                                }
                                .encode(w)
                            },
                        );
                        println!("  HTTP: off");
                    }
                }
            }
        }

        if let (Some(code), Some(to)) = (&alert, provisioned) {
            if Instant::now() >= next_alert {
                next_alert = Instant::now() + alert_every;
                // Slot 1, priority 230, and an expiry. The expiry is not
                // optional above the ambient floor: it is what stops a
                // controller that walks away from leaving an alert up for ever,
                // and it is what makes this clear itself.
                let secs = alert_lasts_us / 1_000_000;
                if crate::push_program(
                    &socket, to, code, &mut sequence, now, 1, 230, alert_lasts_us, [9; 16],
                )
                .is_ok()
                {
                    println!("  ALERT over the show for {secs} s");
                }
            }
        }

        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            println!(
                "  {role:?}, {}, show {} us, {} peer(s)",
                if synced { "synced" } else { "unsynced" },
                now,
                peers.len()
            );
            // Per window, not cumulative. Spike S3 learned this once already:
            // an average that includes the seconds before a node synchronised
            // never recovers from them, and reports a mesh as broken long after
            // it has converged.
            if let Some((index, digest)) = last_frame {
                println!("  frame #{index} {digest:016x} at show {} us", index * FRAME_US);
            }
            // Per window. A cumulative average never recovers from the
            // seconds before a node synchronised, and this is the third time
            // that has hidden a converged result in this project - S3 learned
            // it, the clock skew relearned it, and here it was again.
            if frame_samples > 0 {
                println!(
                    "  frames this window: {agree_w} identical, {differ_w} different, {} frame(s) behind its own clock (worst {frame_drift_worst}); report age up to {worst_stale_us} us",
                    frame_drift_total / frame_samples as i64
                );
            }
            frame_samples = 0;
            frame_drift_total = 0;
            frame_drift_worst = 0;
            agree_w = 0;
            differ_w = 0;
            worst_stale_us = 0;
            if skew_samples > 0 {
                println!(
                    "  clock skew this window: mean {} us, worst {worst_skew_us} us, over {skew_samples} datagrams",
                    skew_total / skew_samples as i64
                );
            }
            skew_samples = 0;
            skew_total = 0;
            worst_skew_us = 0;
        }
    }
}

/// Carry out what the mesh asked for; return when it wants waking next.
#[allow(clippy::too_many_arguments)]
fn apply(
    actions: &[Action],
    now_us: u64,
    socket: &UdpSocket,
    peers: &[([u8; 4], Ipv4Addr)],
    port: u16,
    clock: &mut ShowClock,
    role: &mut Role,
    synced: &mut bool,
) -> u64 {
    // The earliest of several timers, not the last: a core asking for 10 ms and
    // then a second in one batch wants waking in 10 ms.
    let mut deadline = u64::MAX;

    for action in actions {
        match action {
            Action::SetTimer { in_us } => deadline = deadline.min(now_us.saturating_add(*in_us)),
            Action::Send { to, datagram, .. } => match to {
                Destination::Mesh => {
                    // Broadcast *and* unicast to everyone already known.
                    //
                    // A limited broadcast leaves by whichever interface the OS
                    // picks, and on a desktop with a WSL or Hyper-V adapter that
                    // is reliably the wrong one: this peer heard its own ticks
                    // arrive from 172.25.192.1 while the device on 192.168.1.66
                    // heard nothing at all, so both elected themselves.
                    //
                    // The unicast copies are not a workaround for that alone.
                    // Spike S3 measured 4-6% loss on multicast against 0.00% on
                    // unicast over this same access point, so for the handful of
                    // peers a house has, addressing them directly is simply
                    // better - and the broadcast stays only to find peers that
                    // are not known yet.
                    let _ = socket.send_to(datagram, SocketAddrV4::new(Ipv4Addr::BROADCAST, port));
                    for (_, addr) in peers {
                        let _ = socket.send_to(datagram, SocketAddrV4::new(*addr, port));
                    }
                }
                Destination::Peer(prefix) => {
                    if let Some((_, addr)) = peers.iter().find(|(p, _)| p == prefix) {
                        let _ = socket.send_to(datagram, SocketAddrV4::new(*addr, port));
                    }
                }
            },
            Action::DisciplineClock { offset_us } => {
                clock.discipline(*offset_us);
                println!("  disciplined by {offset_us} us");
            }
            Action::RoleChanged { role: r, epoch } => {
                *role = *r;
                println!("  now {r:?} in epoch {epoch}");
            }
            Action::SyncAcquired => {
                *synced = true;
                println!("  show clock acquired");
            }
            Action::SyncLost => {
                *synced = false;
                println!("  show clock lost");
            }
        }
    }

    if deadline == u64::MAX {
        now_us.saturating_add(1_000)
    } else {
        deadline.max(now_us.saturating_add(1_000))
    }
}
