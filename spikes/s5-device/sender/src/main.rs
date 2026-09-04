//! The other end of Spike S5: something that tells a device what to render.
//!
//! Runs on a desktop, speaks the real wire protocol, and exists so the device
//! half can be proven on its own. A dark strip with a phone at the other end has
//! two possible causes and debugging both at once is how a day goes missing.
//!
//! # What it does
//!
//! 1. Listens for the device's one-byte hello broadcast, which is how it learns
//!    the address. Discovery proper is mDNS; this spike is not about discovery.
//! 2. Compiles an effect from source with the real compiler.
//! 3. Sends it as `ProgBegin` / `ProgChunk` × n / `ProgEnd`.
//! 4. Sends a `SrcPush` to put it on the source stack.
//! 5. Sends a `Tick` every second so the device runs on this machine's show
//!    clock rather than its own uptime.
//!
//! Unicast, because S3 measured 4–6% loss on multicast over this AP against
//! 0.00% on unicast, and a program transfer that loses a chunk is a program that
//! never renders.
//!
//! ```text
//! cargo run --release -- effects/rainbow.lfx
//! cargo run --release -- --list          # what effects are to hand
//! ```

mod peer;
mod simulate;

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lumen_proto::header::{Header, MsgType, HEADER_LEN, TAG_LEN};
use lumen_proto::msg::{Chan, ChanClaim, ProgBegin, ProgChunk, ProgEnd, SrcPush, Tick};
use lumen_proto::{Uuid, Writer};

/// The port a device listens on, matching the spike's firmware.
const PORT: u16 = 6354;

/// Which mesh. A device drops anything else from the header alone.
const MESH_PREFIX: [u8; 2] = [0x4c, 0x4d];

/// The device's hello: the marker byte, then whether it already holds a
/// program. See the note in the firmware for why the second byte exists.
const HELLO: u8 = 0xA5;

/// Bytes of bytecode per chunk.
///
/// Small enough that a chunk plus its header clears a 1500-byte MTU with room
/// to spare. Fragmentation at the IP layer would work and would also mean one
/// lost fragment costs the whole datagram, which is the thing chunking exists
/// to avoid.
const CHUNK: usize = 1024;

/// How often a driven channel is published.
///
/// Thirty, matching the device's frame rate. Faster would be spending bandwidth
/// on values no frame will ever read; much slower and the channel's hold time
/// starts deciding what the light does instead of the slider.
const DRIVE_HZ: u32 = 30;

/// How long one sweep of the driven value takes.
const DRIVE_PERIOD_US: u64 = 4_000_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" {
        eprintln!("usage: sender <effect.lfx | program.lfxb> [--priority N] [--seconds N]");
        eprintln!("       sender <effect.lfx> --simulate [--leds N] [--fps N]");
        eprintln!("       sender <effect.lfx> --verify <show_us> [--leds N]");
        eprintln!("       sender --peer [--capacity N]     second node in the mesh");
        eprintln!("       sender --list");
        return Ok(());
    }

    // Before the effect is read: a peer plays nothing, it only elects and
    // syncs, so requiring a file it will not compile would be a poor joke.
    if args.iter().any(|a| a == "--peer") {
        return peer::run(
            arg_value(&args, "--capacity").unwrap_or(2_000) as u32,
            PORT,
        )
        .map_err(Into::into);
    }

    let source_path = &args[0];
    let priority = arg_value(&args, "--priority").unwrap_or(100) as u8;
    let seconds = arg_value(&args, "--seconds").unwrap_or(600);

    // Compile with the real compiler, or take bytecode straight if that is what
    // was handed over. Both paths exist because "does my effect work on real
    // hardware" and "does this exact bytecode work" are different questions.
    let bytecode = if source_path.ends_with(".lfxb") {
        std::fs::read(source_path)?
    } else {
        let text = std::fs::read_to_string(source_path)?;
        match lumen_lang::compile(&text) {
            (Some(compiled), _) => {
                println!(
                    "compiled {}: {} bytes, {} units/pixel, {} registers",
                    source_path,
                    compiled.bytecode.len(),
                    compiled.report.instructions_per_pixel,
                    compiled.report.registers_used
                );
                compiled.bytecode
            }
            (None, diags) => {
                // Print what the compiler said rather than that it failed. A
                // sender that swallows diagnostics turns "your effect has a
                // typo" into "the hardware is broken".
                // `render` produces the compiler's own message with the source
                // line under it. A sender that swallowed diagnostics would turn
                // "your effect has a typo" into "the hardware is broken".
                eprintln!("{source_path} did not compile:");
                eprintln!("{}", diags.render(&text));
                return Ok(());
            }
        }
    };

    if let Some(at) = args.iter().position(|a| a == "--verify") {
        let show_us = args
            .get(at + 1)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        simulate::verify(
            &bytecode,
            arg_value(&args, "--leds").unwrap_or(30) as u16,
            show_us,
        );
        return Ok(());
    }

    if args.iter().any(|a| a == "--simulate") {
        simulate::simulate(
            &bytecode,
            arg_value(&args, "--leds").unwrap_or(30) as u16,
            arg_value(&args, "--fps").unwrap_or(30) as u32,
            arg_value(&args, "--frames").unwrap_or(90) as u32,
        );
        return Ok(());
    }

    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;

    // A show clock both ends agree on, counting from when this show started.
    //
    // **Not** wall time, and that is the whole point. The VM reads `t` as Q16.16
    // seconds, which holds about 32 768 of them before the integer part eats the
    // range; feed it a Unix timestamp and `t * speed` saturates, so an effect
    // stops moving smoothly and starts stepping whenever the arithmetic happens
    // to cross a quantum. On hardware that looks like a 2 fps animation on a
    // device reporting 30 fps, which is a memorable way to spend an evening.
    //
    // Wall time still travels, in the `Tick`'s own `wall_time_us` field, which
    // is where a device that wants a date should look. Show time is an elapsed
    // count from an epoch the mesh agrees on, and it starts small.
    let epoch = Instant::now();
    let show_now = || epoch.elapsed().as_micros() as u64;

    let mut sequence = 0u32;
    let program_id = 1u16;

    println!("listening for a device on port {PORT} ...");
    loop {
        // Any device saying it has no program gets one. That covers the first
        // one to appear, a device that has been rebooted, and a device that was
        // reflashed halfway through a transfer - which is how this loop came to
        // exist, because the fixed version sent the program once to a device
        // that then restarted and stayed dark.
        // Back to a patient timeout while there is nothing to drive: spinning
        // at 500 Hz to wait for a broadcast would burn a core for nothing.
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        let device = wait_for_needy_device(&socket)?;
        println!("
device at {device} has no program; sending one");

        send(&socket, device, MsgType::ProgBegin, &mut sequence, show_now(), |w| {
            ProgBegin {
                program_id,
                slot: 0,
                vm_min_version: 1,
                total_len: bytecode.len() as u32,
                device_class: "strip",
            }
            .encode(w)
        })?;

        for (i, part) in bytecode.chunks(CHUNK).enumerate() {
            send(&socket, device, MsgType::ProgChunk, &mut sequence, show_now(), |w| {
                ProgChunk {
                    program_id,
                    offset: (i * CHUNK) as u32,
                    data: part,
                }
                .encode(w)
            })?;
            // A gap between chunks. The device parses each one inside its
            // receive loop, and a burst arriving faster than that overflows the
            // socket's rx buffer - which looks exactly like network loss from
            // here.
            std::thread::sleep(Duration::from_millis(20));
        }

        // The hash and signature are zero. This spike proves the path, not the
        // trust: verification is `lumen-crypto` behind the `lumen-proto` seam,
        // and wiring it in here would be testing two things at once.
        send(&socket, device, MsgType::ProgEnd, &mut sequence, show_now(), |w| {
            ProgEnd {
                program_id,
                sha256: [0; 32],
                sig: [0; 64],
            }
            .encode(w)
        })?;
        println!("sent {} bytes of bytecode", bytecode.len());

        let expires = show_now() + seconds * 1_000_000;
        send(&socket, device, MsgType::SrcPush, &mut sequence, show_now(), |w| {
            SrcPush {
                source_id: Uuid([7; 16]),
                zone_id: Uuid([50; 16]),
                scene_id: Uuid([7; 16]),
                priority,
                fade_in_ms: 500,
                fade_out_ms: 500,
                // Absolute show time, not a duration: every device shares this
                // clock, so a source expires at the same instant everywhere
                // regardless of when each one heard about it.
                expires_at: Some(expires),
                param_overrides: &[],
            }
            .encode(w)
        })?;
        println!("pushed at priority {priority}, expiring in {seconds} s");

        // Claim the channels this program reads, so this sender is allowed to
        // publish to them. A channel is owned: two producers fighting over one
        // slider is the failure the claim exists to make impossible, and the
        // higher priority wins outright rather than the two interleaving.
        let channels = channel_ids(&bytecode);
        for id in &channels {
            send(&socket, device, MsgType::ChanClaim, &mut sequence, show_now(), |w| {
                ChanClaim {
                    channel_id: *id,
                    priority: 100,
                    // Renewed on the same clock the Ticks go out on. A lease
                    // that outlives the producer would leave a channel nobody
                    // can take over.
                    lease_ms: 5_000,
                }
                .encode(w)
            })?;
        }
        if channels.is_empty() {
            println!("this effect reads no channels; nothing to drive");
        } else {
            println!("claimed channel(s) {channels:?}, driving at {DRIVE_HZ} Hz");
        }

        // Poll quickly from here on.
        //
        // The half-second timeout is right while waiting for a device to appear
        // and badly wrong once driving one: the loop blocks in `recv_from` for
        // up to 500 ms, so it can only publish about twice a second whatever
        // `DRIVE_HZ` says. The device rendered a rock-steady 30 fps of a value
        // that changed twice a second, which looks exactly like a device running
        // at 2 fps and is not.
        socket.set_read_timeout(Some(Duration::from_millis(2)))?;

        // Hold the show clock, and keep listening. A device that comes back
        // needing a program breaks out and gets provisioned again.
        let mut next_tick = Instant::now();
        let mut next_drive = Instant::now();
        let mut producer_seq: u16 = 0;
        loop {
            // Drive the channels. A triangle rather than a sine: the turn at
            // each end is the moment a dropped or stale update shows up as a
            // visible flat spot, and a sine hides exactly that.
            if !channels.is_empty() && Instant::now() >= next_drive {
                next_drive += Duration::from_micros(1_000_000 / DRIVE_HZ as u64);
                let phase = (show_now() % DRIVE_PERIOD_US) as f64 / DRIVE_PERIOD_US as f64;
                let level = if phase < 0.5 { phase * 2.0 } else { (1.0 - phase) * 2.0 };
                let q16 = (level * 65536.0) as i32;
                producer_seq = producer_seq.wrapping_add(1);
                for id in &channels {
                    send(&socket, device, MsgType::Chan, &mut sequence, show_now(), |w| {
                        Chan {
                            channel_id: *id,
                            producer_seq,
                            payload: &q16.to_le_bytes(),
                        }
                        .encode(w)
                    })?;
                }
            }

            if Instant::now() >= next_tick {
                next_tick += Duration::from_secs(1);
                send(&socket, device, MsgType::Tick, &mut sequence, show_now(), |w| {
                    Tick {
                        show_time_us: show_now(),
                        master_uuid: Uuid([7; 16]),
                        master_capacity: 0,
                        election_epoch: 0,
                        // This machine's wall clock is whatever the OS says,
                        // which is NTP-disciplined on any desktop but is not
                        // something this spike verifies. `AppSupplied` is the
                        // honest label: it came from an application.
                        wall_time_us: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_micros() as u64)
                            .unwrap_or(0),
                        wall_quality: lumen_proto::msg::WallQuality::AppSupplied,
                    }
                    .encode(w)
                })?;
                // Renew, so the lease never runs out under a producer that is
                // still here. Cheaper than making the lease long: a long lease
                // is how a channel ends up held by something that has gone.
                for id in &channels {
                    send(&socket, device, MsgType::ChanClaim, &mut sequence, show_now(), |w| {
                        ChanClaim {
                            channel_id: *id,
                            priority: 100,
                            lease_ms: 5_000,
                        }
                        .encode(w)
                    })?;
                }
            }
            let mut buf = [0u8; 64];
            match socket.recv_from(&mut buf) {
                Ok((n, _)) if n >= 2 && buf[0] == HELLO && buf[1] == 0 => break,
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Wait for a device that needs a program.
///
/// Returns where it is. A device that already holds one is left alone - which
/// is what stops this from re-pushing the same effect five times a minute at a
/// device that is perfectly happy.
fn wait_for_needy_device(socket: &UdpSocket) -> std::io::Result<SocketAddr> {
    use std::io::Write;
    let mut buf = [0u8; 64];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if n >= 1 && buf[0] == HELLO => {
                let has_program = n >= 2 && buf[1] == 1;
                if !has_program {
                    return Ok(from);
                }
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                print!(".");
                let _ = std::io::stdout().flush();
            }
            Err(e) => return Err(e),
        }
    }
}

/// Frame a payload as a Lumen datagram and send it.
fn send<F>(
    socket: &UdpSocket,
    to: SocketAddr,
    msg_type: MsgType,
    sequence: &mut u32,
    show_time_us: u64,
    build: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&mut Writer<'_>) -> Result<(), lumen_proto::EncodeError>,
{
    let mut body = [0u8; 1400];
    let written = {
        let mut w = Writer::new(&mut body);
        build(&mut w).map_err(|e| format!("encoding {msg_type:?}: {e:?}"))?;
        w.position()
    };

    *sequence += 1;
    let header = Header::new(msg_type, MESH_PREFIX, [1, 2, 3, 4], *sequence, show_time_us);
    let datagram = lumen_proto::Datagram {
        header,
        payload: &body[..written],
        // Plaintext, so the tag is not meaningful. The header carries the flag
        // that says so, and a device that required a tag here would be
        // requiring one it cannot check.
        tag: &[0u8; TAG_LEN],
    };

    let mut out = vec![0u8; HEADER_LEN + written + TAG_LEN];
    let n = datagram
        .encode(&mut out)
        .map_err(|e| format!("framing {msg_type:?}: {e:?}"))?;
    socket.send_to(&out[..n], to)?;
    Ok(())
}

/// The channels a compiled program reads, from its own header.
///
/// Read from the bytecode rather than from the source, because the header is
/// what the device reads too — if the two disagreed, this would be publishing to
/// a channel nobody is listening on and the strip would simply not respond.
fn channel_ids(bytecode: &[u8]) -> Vec<u16> {
    let Ok(program) = lumen_vm::program::Program::parse(bytecode) else {
        return Vec::new();
    };
    (0..program.channel_count())
        .filter_map(|slot| program.channel_id(slot as u8))
        .collect()
}

fn arg_value(args: &[String], name: &str) -> Option<u64> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1)?.parse().ok()
}
