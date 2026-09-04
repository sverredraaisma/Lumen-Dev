//! Spike S5: a whole Lumen device, on real hardware, driving real light.
//!
//! Everything measured so far was measured with the output thrown away — the
//! VM's throughput, the sync offsets, the multicast loss, the dual-core split.
//! This is the stage where the pieces are wired to each other and to a strip,
//! and a person can look at it.
//!
//! # What it is
//!
//! WiFi, a UDP socket, and the **real** device core above them: programs
//! arriving as `ProgBegin`/`ProgChunk`/`ProgEnd`, a `SrcPush` putting one on the
//! source stack, `lumen_device::Renderer` rendering it through the real VM and
//! the real zone projection, and the result going out of the RMT peripheral to
//! thirty SK6812 RGBW LEDs on GPIO4.
//!
//! Nothing along that path is a stand-in. If a pixel is wrong, something that
//! ships is wrong — which is the whole reason to build it this way rather than
//! mock the halves that are inconvenient.
//!
//! # Two stages, and stage 1 is still in here
//!
//! `LUMEN_STAGE=strip` runs the self-test from stage 1 and never touches the
//! radio: one pixel walking the strip, then colour blocks, then white, then a
//! gradient. It is how the strip, the pin, the LED count and the colour order
//! were confirmed before anything harder was stacked on top, and it stays
//! because it is the first thing to re-run when the light looks wrong.
//!
//! Anything else runs the device.
//!
//! # It elects a timebase rather than being told one
//!
//! The show clock used to come from a `Tick` the desktop sent, which works for
//! one device and is not a mesh. Devices now run `lumen_device::node::Node` —
//! the same election and sync state machines the simulator exercises — and agree
//! among themselves: whoever has the most capacity leads, ties break on the
//! lower UUID, and everyone else disciplines their clock towards the leader over
//! 32 filtered round trips.
//!
//! Corrections are **slewed, never stepped** (`clock.rs`). Every effect is a
//! function of this clock, so a step renders a frame twice or skips one — a
//! visible stutter on every device, every time it resynchronises.
//!
//! A cold mesh takes about five seconds to have a leader and a few more to be
//! synchronised. Until then a device renders on its own clock rather than
//! waiting: a device is never dark because of software, and unsynchronised
//! light beats none.
//!
//! # It announces itself
//!
//! The device broadcasts a one-byte hello. A sender hears it, learns the
//! address, and sends **unicast** from then on: Spike S3 measured 4-6% loss on
//! multicast over this AP against 0.00% on unicast, and a program transfer that
//! loses a chunk is a program that never renders.
//!
//! It keeps announcing after it has been given something, once every five
//! seconds instead of every one. Stopping was tried first and is wrong: a
//! second sender can then never find a device that is already rendering, which
//! means the only way to change the effect is to reboot the device.

#![no_std]
#![no_main]

extern crate alloc;

mod clock;
mod node;
mod strip;
mod strip_dma;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::rmt::Rmt;
use esp_hal::rng::Rng;
use esp_hal::time::RateExtU32;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;

use esp_wifi::wifi::{ClientConfiguration, Configuration, WifiDevice, WifiStaDevice};
use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::socket::{dhcpv4, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

use clock::ShowClock;
use lumen_device::node::Node as MeshNode;
use lumen_device::{Action, Destination, Event, Identity, Role};
use lumen_proto::Uuid;
use node::{Handled, Node};
use strip::{buffer_words, Format, Strip};
use strip_dma::DmaStrip;

/// Credentials come from the environment at build time, so they are never in
/// the repository. Build with:
///
/// ```text
/// LUMEN_WIFI_SSID='...' LUMEN_WIFI_PASS='...' cargo build --release
/// ```
const SSID: &str = env!("LUMEN_WIFI_SSID");
const PASS: &str = env!("LUMEN_WIFI_PASS");

/// `strip` runs the stage-1 self-test with no radio at all.
const STAGE: &str = env!("LUMEN_STAGE");

/// `rmt` drives the strip from RMT with interrupts held off for the frame;
/// anything else uses SPI with DMA.
///
/// Both are kept because the comparison is the point: RMT needed a critical
/// section to survive WiFi at all, and how much that costs is a number this
/// spike should be able to produce rather than assert.
const DRIVER: &str = env!("LUMEN_DRIVER");

/// LEDs on the strip, and the pin they are on.
const LEDS: usize = 30;

/// SK6812 RGBW: 32 bits per LED, with a dedicated white die.
///
/// The white byte is sent as zero and white is mixed from the colour dies -
/// see the note in `strip.rs`. It costs brightness and buys every device in the
/// mesh agreeing about what a colour is.
const FORMAT: Format = Format::Grbw;

const SCRATCH: usize = buffer_words(LEDS, 4);

/// The port a Lumen device listens on. Same as S3 used, so the two spikes can
/// share a network without confusing each other's captures.
const PORT: u16 = 6354;

/// Which mesh this device belongs to. A device drops anything with a different
/// prefix from the header alone, without decrypting.
const MESH_PREFIX: [u8; 2] = [0x4c, 0x4d];

/// The same mesh, as the UUID the core wants. Its first two bytes are
/// [`MESH_PREFIX`], because that is what goes on the wire.
const MESH_UUID: [u8; 16] = [
    0x4c, 0x4d, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// Peers this device will remember an address for. A house is not a datacentre.
const MAX_PEERS: usize = 8;

/// Two bytes: "a Lumen device is here", and whether it already holds a program.
///
/// The second byte is what makes the test repeatable. A device that reboots -
/// or is reflashed mid-transfer, which is how this was found - comes back with
/// nothing, and a sender that had already sent its program would never send it
/// again. Saying so costs one byte and turns "sometimes the strip stays dark"
/// into a state machine.
///
/// Not a protocol message. Real discovery is mDNS, which is I/O, and this spike
/// is not about discovery.
const HELLO: u8 = 0xA5;
const HELLO_INTERVAL_US: u64 = 1_000_000;

/// Frame period. 30 fps for a first light - the C3 has headroom for 60 at 30
/// LEDs, and a slower frame makes a stutter easier to see by eye.
///
/// The same grid `node::FRAME_US` quantises show time to, so every device in the
/// mesh draws the same frame at the same moment.
use node::FRAME_US;

fn now_us() -> u64 {
    esp_hal::time::now().duration_since_epoch().to_micros()
}

fn set(pixels: &mut [u8], i: usize, r: u8, g: u8, b: u8) {
    pixels[i * 3] = r;
    pixels[i * 3 + 1] = g;
    pixels[i * 3 + 2] = b;
}

/// Stage 1: prove the strip with no network in the way.
fn strip_self_test<C: esp_hal::rmt::TxChannel>(
    led_strip: &mut Strip<C>,
    pixels: &mut [u8],
    scratch: &mut [u32],
    delay: &esp_hal::delay::Delay,
) -> ! {
    loop {
        println!();
        println!("== S5 stage 1: strip self-test, {LEDS} LEDs, {}", FORMAT.name());

        println!("1. one white pixel walking 0 -> {}", LEDS - 1);
        for i in 0..LEDS {
            pixels.fill(0);
            // Dim on purpose: thirty of these at full white is about 1.8 A, and
            // a USB supply answering that with a brownout looks exactly like a
            // driver that cannot hold a frame.
            set(pixels, i, 40, 40, 40);
            let _ = led_strip.write(pixels, scratch);
            delay.delay_millis(120);
        }

        println!("2. ten red, ten green, ten blue - red nearest the controller");
        for i in 0..LEDS {
            match i / 10 {
                0 => set(pixels, i, 60, 0, 0),
                1 => set(pixels, i, 0, 60, 0),
                _ => set(pixels, i, 0, 0, 60),
            }
        }
        let _ = led_strip.write(pixels, scratch);
        delay.delay_millis(4_000);

        println!("3. every LED dim white, mixed from the colour dies");
        for i in 0..LEDS {
            set(pixels, i, 30, 30, 30);
        }
        let _ = led_strip.write(pixels, scratch);
        delay.delay_millis(4_000);

        println!("4. a red-to-blue gradient");
        for i in 0..LEDS {
            let t = (i * 255 / (LEDS - 1)) as u8;
            set(pixels, i, 60 - t / 5, 0, t / 4);
        }
        let _ = led_strip.write(pixels, scratch);
        delay.delay_millis(4_000);

        pixels.fill(0);
        let _ = led_strip.write(pixels, scratch);
        delay.delay_millis(2_000);
    }
}

/// Whatever this device pushes bytes out of.
trait StripOut {
    fn show(&mut self, pixels: &[u8]) -> Result<(), ()>;
    fn name(&self) -> &'static str;
}

impl<C: esp_hal::rmt::TxChannel> StripOut for (Strip<C>, &mut [u32]) {
    fn show(&mut self, pixels: &[u8]) -> Result<(), ()> {
        self.0.write(pixels, self.1).map_err(|_| ())
    }
    fn name(&self) -> &'static str {
        "RMT, interrupts held off for the frame"
    }
}

impl StripOut for (DmaStrip<'_>, &mut [u8]) {
    fn show(&mut self, pixels: &[u8]) -> Result<(), ()> {
        self.0.write(pixels, self.1)
    }
    fn name(&self) -> &'static str {
        "SPI with DMA"
    }
}

#[allow(clippy::too_many_lines)]
fn device_loop(
    mut controller: esp_wifi::wifi::WifiController<'_>,
    device: WifiDevice<'_, WifiStaDevice>,
    mac: [u8; 6],
    led_strip: &mut dyn StripOut,
    pixels: &mut [u8],
) -> ! {
    let mut device = device;

    controller
        .set_configuration(&Configuration::Client(ClientConfiguration {
            ssid: SSID.try_into().expect("ssid fits"),
            password: PASS.try_into().expect("password fits"),
            ..Default::default()
        }))
        .expect("configure");
    controller.start().expect("start wifi");
    // Clear the strip before anything else. An LED holds its last value until
    // something writes a new one, so a device that reboots leaves the previous
    // firmware's final frame lit - which looks exactly like a device that has
    // hung, and is the first thing anybody would go and debug.
    //
    // "A device is never dark because of software" is about not *losing* a show
    // to a fault. Sitting on a frame from before a reboot is not holding a show,
    // it is lying about one.
    pixels.fill(0);
    let _ = led_strip.show(pixels);

    println!("== connecting to {SSID}");

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if let Err(e) = controller.connect() {
            println!("== connect() attempt {attempt}: {e:?}");
        }
        let until = now_us() + 8_000_000;
        while now_us() < until {
            if matches!(controller.is_connected(), Ok(true)) {
                break;
            }
        }
        if matches!(controller.is_connected(), Ok(true)) {
            println!("== associated after {attempt} attempt(s)");
            break;
        }
        println!("== not associated after attempt {attempt}; retrying");
    }

    // Power save off, and **after** association - S1 found it worth 4x on sync
    // and S3 found setting it before `connect` did not survive. A device holding
    // a shared clock cannot sleep between beacons, and that belongs in the power
    // budget rather than being discovered later.
    if let Err(e) = controller.set_power_saving(esp_wifi::config::PowerSaveMode::None) {
        println!("== could not disable power save: {e:?}");
    }

    let config = Config::new(EthernetAddress::from_bytes(&mac).into());
    let mut iface = Interface::new(config, &mut device, Instant::from_micros(now_us() as i64));

    let mut sockets_storage = [SocketStorage::EMPTY; 3];
    let mut sockets = SocketSet::new(&mut sockets_storage[..]);
    let dhcp = sockets.add(dhcpv4::Socket::new());

    let mut rx_meta = [udp::PacketMetadata::EMPTY; 32];
    let mut rx_buf = [0u8; 8192];
    let mut tx_meta = [udp::PacketMetadata::EMPTY; 8];
    let mut tx_buf = [0u8; 1024];
    let mut socket = udp::Socket::new(
        udp::PacketBuffer::new(&mut rx_meta[..], &mut rx_buf[..]),
        udp::PacketBuffer::new(&mut tx_meta[..], &mut tx_buf[..]),
    );
    socket.bind(PORT).expect("bind");
    let udp_handle = sockets.add(socket);

    // The show clock. Everything below reads this rather than the hardware: it
    // is what election and sync discipline, and what every effect is a function
    // of.
    let mut clock = ShowClock::new(now_us());

    // The device's identity. Derived from the MAC so it is stable across
    // reboots and different on every board - a real device would have this
    // provisioned, and two devices sharing a UUID is the kind of thing that
    // only shows up when the second one is plugged in.
    let mut id = [0u8; 16];
    id[..6].copy_from_slice(&mac);
    let mut lumen = Node::new(MESH_PREFIX, Uuid(id), LEDS as u16);

    // The mesh half: election and time sync, from `lumen-device`. Capacity is a
    // static benchmark score - VM instructions per second over a thousand, from
    // Spike S2 - and must never be current load, or the role flaps under it.
    // Both these chips report the same number and the UUID breaks the tie,
    // which is exactly what it is for.
    let capacity = if cfg!(feature = "esp32s3") { 1714 } else { 1194 };
    let mut mesh = MeshNode::new(
        Identity::new(Uuid(id), capacity),
        Uuid(MESH_UUID),
        0,
        clock.now_us(),
    );
    // Prefix to address, learned from whoever talks to us. The core addresses
    // peers by UUID prefix and never sees an IP; this is the shell's half of
    // that bargain.
    let mut peers: [([u8; 4], Ipv4Address); MAX_PEERS] = [([0; 4], Ipv4Address::UNSPECIFIED); MAX_PEERS];
    let mut peer_count = 0usize;
    let mut mesh_deadline_us = 0u64;
    let mut role = Role::Follower;
    let mut synced = false;

    println!("== waiting for a lease");
    let mut have_lease = false;
    let mut next_hello_us = 0u64;
    let mut next_frame_us = 0u64;
    let mut frames: u32 = 0;
    let mut spent_total: u64 = 0;
    let mut next_report_us = 0u64;
    let mut announced = false;
    let mut datagrams: u32 = 0;
    let mut program_bytes = 0usize;
    let mut sources = 0usize;
    let mut draw_ua = 0u32;
    let (mut rx_tick, mut rx_req, mut rx_resp) = (0u32, 0u32, 0u32);
    let mut last_render: Option<node::Rendered> = None;
    let mut render_us = 0u64;
    let mut show_us = 0u64;
    let mut derated = lumen_vm::q16::Q16::ONE;

    loop {
        iface.poll(
            Instant::from_micros(now_us() as i64),
            &mut device,
            &mut sockets,
        );

        if !have_lease {
            if let Some(dhcpv4::Event::Configured(cfg)) =
                sockets.get_mut::<dhcpv4::Socket>(dhcp).poll()
            {
                iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(cfg.address));
                });
                if let Some(router) = cfg.router {
                    let _ = iface.routes_mut().add_default_ipv4_route(router);
                }
                println!("== lease: {}", cfg.address);
                println!(
                "== listening on {PORT}, {LEDS} LEDs, mesh {MESH_PREFIX:02x?}, via {}",
                led_strip.name()
            );
                have_lease = true;
                next_frame_us = now_us();
                next_report_us = now_us() + 5_000_000;
            }
            continue;
        }

        // Say where we are, always. A sender hears this, learns the address,
        // and unicasts from then on - which is what S3 measured at 0.00% loss
        // against multicast's 4-6%.
        //
        // Backing off rather than stopping once something has arrived. Stopping
        // was the first version and it means a device that is already rendering
        // can never be found again, so changing the effect needs a reboot.
        let t = now_us();
        if t >= next_hello_us {
            let socket = sockets.get_mut::<udp::Socket>(udp_handle);
            let to = IpEndpoint {
                addr: IpAddress::Ipv4(Ipv4Address::new(255, 255, 255, 255)),
                port: PORT,
            };
            let _ = socket.send_slice(&[HELLO, u8::from(lumen.program_bytes() > 0)], to);
            next_hello_us = t + if announced {
                HELLO_INTERVAL_US * 5
            } else {
                HELLO_INTERVAL_US
            };
        }

        // The mesh runs on the show clock, and advancing it is what applies any
        // outstanding correction. Once per turn, before anything reads the time.
        let show_now = clock.advance_to(now_us());

        if show_now >= mesh_deadline_us {
            let actions = mesh.on_event(show_now, Event::Tick);
            let socket = sockets.get_mut::<udp::Socket>(udp_handle);
            mesh_deadline_us = apply_mesh(
                &actions,
                show_now,
                socket,
                &peers[..peer_count],
                &mut clock,
                &mut role,
                &mut synced,
            );
        }

        {
            let socket = sockets.get_mut::<udp::Socket>(udp_handle);
            let mut buf = [0u8; 1500];
            while let Ok((n, meta)) = socket.recv_slice(&mut buf) {
                if buf[0] == HELLO && n <= 2 {
                    // Another device announcing itself. Not ours to answer.
                    continue;
                }
                datagrams += 1;

                // Remember where this sender lives, so the core's
                // `Destination::Peer(prefix)` can become an address. The core
                // addresses peers by UUID prefix and never sees an IP; this is
                // the shell's half of that bargain. Bytes 6..10 of the header
                // are the sender prefix.
                if n >= 10 {
                    // What arrives, by type. Counted rather than printed: this
                    // is the receive loop, and a line per datagram would measure
                    // `println!`.
                    match buf[2] {
                        0x10 => rx_tick += 1,
                        0x11 => rx_req += 1,
                        0x12 => rx_resp += 1,
                        _ => {}
                    }
                    // Only IPv4 is configured on this interface, so the address
                    // can only be one thing; destructured rather than matched
                    // to say so.
                    let IpAddress::Ipv4(v4) = meta.endpoint.addr;
                    remember(
                        &mut peers,
                        &mut peer_count,
                        [buf[6], buf[7], buf[8], buf[9]],
                        v4,
                    );
                }

                // Every datagram goes to the mesh as well. It picks out the
                // three it cares about - TICK, SYNC_REQ, SYNC_RESP - drops
                // rubbish silently, filters foreign meshes on the prefix, and
                // ignores its own looped-back multicast, which on an ESP32 is
                // not hypothetical.
                let actions = mesh.on_event(show_now, Event::Datagram { bytes: &buf[..n] });
                if !actions.is_empty() {
                    mesh_deadline_us = apply_mesh(
                        &actions,
                        show_now,
                        socket,
                        &peers[..peer_count],
                        &mut clock,
                        &mut role,
                        &mut synced,
                    );
                }

                let handled = lumen.receive(&buf[..n], now_us());
                match handled {
                    // Logged, because these are the ones worth seeing once. The
                    // per-chunk and per-frame cases are not: printing from the
                    // receive loop at 30 Hz measures `println!`.
                    Handled::ProgramStarted { len } => {
                        println!("== program arriving, {len} bytes");
                        announced = true;
                    }
                    Handled::ProgramComplete {
                        len,
                        budget,
                        channels,
                    } => {
                        println!(
                            "== program complete: {len} bytes, {budget} units/pixel, {channels} channel(s)"
                        );
                    }
                    Handled::ChannelClaimed { id } => println!("== channel {id} claimed"),
                    Handled::ChannelUnknown { id } => {
                        println!("== channel {id} is not one this program reads")
                    }
                    // Not logged: a slider sends these thirty times a second,
                    // and a line each would measure `println!` rather than the
                    // device.
                    Handled::ChannelSet { .. } => {}
                    Handled::ProgramRejected => println!("== program rejected"),
                    Handled::SourcePushed { priority } => {
                        println!("== source pushed at priority {priority}");
                    }
                    Handled::SourceRejected => println!("== source rejected"),
                    Handled::SourcePopped => println!("== source popped"),
                    Handled::Undecodable => println!("== undecodable datagram"),
                    Handled::NotForThisMesh | Handled::Ignored | Handled::ProgramChunk { .. } => {}
                }
            }
        }

        let t = show_now;
        if t >= next_frame_us {
            // Absolute rather than `t + FRAME_US`, so a slow frame does not push
            // every later one late. A show clock that drifts because rendering
            // was briefly slow is a show clock that no longer agrees with the
            // rest of the mesh.
            next_frame_us += FRAME_US;
            if next_frame_us < t {
                next_frame_us = t + FRAME_US;
            }

            let before = now_us();
            lumen.advance(t);
            program_bytes = lumen.program_bytes();
            sources = lumen.source_count();
            if let Some((spent, encoded, rendered)) = lumen.render(t, pixels) {
                spent_total += spent as u64;
                draw_ua = encoded.draw_ua;
                derated = encoded.derated_to;
                last_render = Some(rendered);
                frames += 1;
                let rendered = now_us();
                if led_strip.show(pixels).is_err() {
                    println!("== the strip driver refused a frame");
                }
                // Split, because the two halves scale completely differently:
                // rendering is per pixel and the output is a fixed transfer at
                // 1.25 us a bit. Which one a longer strip runs out of frame on
                // is the question this spike should be able to answer.
                render_us += rendered - before;
                show_us += now_us() - rendered;
            }
        }

        if t >= next_report_us {
            next_report_us = t + 5_000_000;
            // Printed every five seconds unconditionally. Silence is the one
            // output that cannot be diagnosed: a device that has hung and a
            // device with nothing to say look identical down a serial cable.
            if frames > 0 {
                // The draw and the derating are reported because a strip that
                // is quietly at 40% because the supply is too small looks
                // exactly like an effect that is quietly wrong, and the two are
                // debugged in completely different places.
                println!(
                    "== {frames} frames in 5 s ({} fps), {} units/frame, {datagrams} datagrams, {} mA{}, render {} us, out {} us",
                    frames / 5,
                    spent_total / frames as u64,
                    draw_ua / 1000,
                    if derated.0 < lumen_vm::q16::Q16::ONE.0 {
                        " (derated)"
                    } else {
                        ""
                    },
                    render_us / frames as u64,
                    show_us / frames as u64,
                );
            } else {
                println!(
                    "== idle: {} bytes of program, {} source(s), {datagrams} datagrams",
                    program_bytes, sources
                );
            }
            // The show clock itself, so two devices can be compared directly.
            // `pending` is the correction still being slewed off: a device that
            // never converges shows one that does not shrink.
            println!(
                "== rx: {rx_tick} ticks, {rx_req} sync-reqs, {rx_resp} sync-resps, {peer_count} peer(s)"
            );
            println!(
                "== clock: show {} us, {:?}, {}, pending {} us",
                show_now,
                role,
                if synced { "synced" } else { "unsynced" },
                clock.pending_us()
            );
            // The frame fingerprint, so a host or another device can be checked
            // against this one. Printed once per report rather than per frame:
            // it is a spot check, and a line per frame would measure `println!`.
            if let Some(r) = last_render {
                // The frame *index* as well as the time. Two synchronised nodes
                // will never render on the same microsecond, but they land on
                // the same frame of the same grid - and that is what "changing
                // colour on the same frame" actually means.
                println!(
                    "== frame #{} {:016x} at show {} us",
                    r.show_us / FRAME_US,
                    r.digest,
                    r.show_us
                );
            }
            frames = 0;
            spent_total = 0;
            render_us = 0;
            show_us = 0;
        }
    }
}

/// Carry out what the mesh asked for, and return when it wants waking next.
///
/// Every action is handled. A shell that quietly ignored one would produce a
/// mesh that mostly works: `SetTimer` dropped means a node that stops, `Send`
/// dropped means one that never leads, `DisciplineClock` dropped means one that
/// never syncs and says it has.
#[allow(clippy::too_many_arguments)]
fn apply_mesh(
    actions: &[Action],
    now_us: u64,
    socket: &mut udp::Socket<'_>,
    peers: &[([u8; 4], Ipv4Address)],
    clock: &mut ShowClock,
    role: &mut Role,
    synced: &mut bool,
) -> u64 {
    // The *earliest* of several timers, not the last. A core that asks for
    // 10 ms and then a second in one batch wants waking in 10 ms, and taking
    // the last would silently drop the tighter deadline.
    let mut deadline = u64::MAX;

    for action in actions {
        match action {
            Action::SetTimer { in_us } => {
                deadline = deadline.min(now_us.saturating_add(*in_us));
            }
            Action::Send { to, datagram, .. } => match to {
                Destination::Mesh => {
                    // Broadcast *and* unicast to everyone already known.
                    //
                    // Spike S3 measured 4-6% loss on multicast over this access
                    // point against 0.00% on unicast, so for the handful of
                    // peers a house has, addressing them directly is simply
                    // better. The broadcast stays only to find peers that are
                    // not known yet - and a peer whose broadcasts leave by the
                    // wrong interface, which is what a desktop with a WSL
                    // adapter does, is then still reachable.
                    send_to(socket, datagram, Ipv4Address::new(255, 255, 255, 255));
                    for (_, addr) in peers {
                        send_to(socket, datagram, *addr);
                    }
                }
                // A peer never heard from has no address yet, and dropping is
                // right: the core will ask again, by which time it will almost
                // certainly have announced itself.
                Destination::Peer(prefix) => {
                    if let Some((_, addr)) = peers.iter().find(|(p, _)| p == prefix) {
                        send_to(socket, datagram, *addr);
                    }
                }
            },
            Action::DisciplineClock { offset_us } => {
                // Logged because it is rare - once on joining, then every
                // thirty seconds - and because a device that never prints one
                // is a device that thinks it is synced and is not.
                println!("== disciplined by {offset_us} us");
                clock.discipline(*offset_us);
            }
            Action::RoleChanged { role: r, epoch } => {
                *role = *r;
                println!("== now {r:?} in epoch {epoch}");
            }
            Action::SyncAcquired => {
                *synced = true;
                println!("== show clock acquired");
            }
            Action::SyncLost => {
                *synced = false;
                println!("== show clock lost");
            }
        }
    }

    // A core that returned no timer would simply stop, and the shell has no way
    // to know it should have. The floor keeps a zero from spinning a core flat.
    if deadline == u64::MAX {
        now_us.saturating_add(1_000)
    } else {
        deadline.max(now_us.saturating_add(1_000))
    }
}

fn send_to(socket: &mut udp::Socket<'_>, datagram: &[u8], addr: Ipv4Address) {
    let to = IpEndpoint {
        addr: IpAddress::Ipv4(addr),
        port: PORT,
    };
    let _ = socket.send_slice(datagram, to);
}

/// Note where a peer lives, keyed by the prefix the core addresses it with.
///
/// Oldest entry is overwritten once full. A house with more than eight devices
/// in earshot is a house that wants a real table, and losing the least recently
/// added address only costs one retransmission.
fn remember(
    peers: &mut [([u8; 4], Ipv4Address); MAX_PEERS],
    count: &mut usize,
    prefix: [u8; 4],
    addr: Ipv4Address,
) {
    for slot in peers[..*count].iter_mut() {
        if slot.0 == prefix {
            slot.1 = addr;
            return;
        }
    }
    if *count < MAX_PEERS {
        peers[*count] = (prefix, addr);
        *count += 1;
    } else {
        peers[0] = (prefix, addr);
    }
}

#[esp_hal::main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    let delay = esp_hal::delay::Delay::new();

    let mut pixels = [0u8; LEDS * 3];
    let mut scratch = [0u32; SCRATCH];

    // The stage-1 self-test always runs on RMT: it exists to prove the strip
    // with as little as possible between the pixel and the wire, and the radio
    // is off, so the deadline RMT races is one nothing is competing for.
    if STAGE == "strip" {
        let rmt = Rmt::new(peripherals.RMT, strip::CLOCK_MHZ.MHz()).expect("RMT");
        let mut led_strip =
            Strip::new(rmt.channel0, peripherals.GPIO4, FORMAT).expect("a channel on GPIO4");
        strip_self_test(&mut led_strip, &mut pixels, &mut scratch, &delay);
    }

    // The render loop allocates - a membership is a `Vec`, the machines live in
    // a `BTreeMap` - and so does esp-wifi. 96 KiB leaves room for both on a
    // chip with about 400.
    esp_alloc::heap_allocator!(96 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let init = esp_wifi::init(
        timg0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
    )
    .expect("esp-wifi");
    let (wifi_device, controller) =
        esp_wifi::wifi::new_with_mode(&init, peripherals.WIFI, WifiStaDevice).expect("sta");
    let mut mac = [0u8; 6];
    esp_wifi::wifi::sta_mac(&mut mac);

    println!();
    println!("== spike S5: a Lumen device");
    println!("== {LEDS} SK6812 RGBW on GPIO4, {}", FORMAT.name());

    if DRIVER == "rmt" {
        let rmt = Rmt::new(peripherals.RMT, strip::CLOCK_MHZ.MHz()).expect("RMT");
        let led_strip =
            Strip::new(rmt.channel0, peripherals.GPIO4, FORMAT).expect("a channel on GPIO4");
        let mut out = (led_strip, &mut scratch[..]);
        device_loop(controller, wifi_device, mac, &mut out, &mut pixels)
    } else {
        // Static, because the DMA engine reads this buffer while the CPU has
        // moved on and a stack frame is exactly the wrong lifetime for that.
        static mut DMA_SCRATCH: [u8; strip_dma::buffer_bytes(LEDS, 4)] =
            [0; strip_dma::buffer_bytes(LEDS, 4)];
        let (rx, tx) = strip_dma_buffers!(strip_dma::buffer_bytes(LEDS, 4));
        let led_strip = DmaStrip::new(
            peripherals.SPI2,
            peripherals.DMA_CH0,
            peripherals.GPIO4,
            FORMAT,
            rx,
            tx,
        )
        .expect("SPI on GPIO4");
        let scratch = unsafe { &mut *core::ptr::addr_of_mut!(DMA_SCRATCH) };
        let mut out = (led_strip, &mut scratch[..]);
        device_loop(controller, wifi_device, mac, &mut out, &mut pixels)
    }
}
