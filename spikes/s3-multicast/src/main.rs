//! Spike S3: does a multicast `CHAN` at 60 Hz reach every device?
//!
//! The third assumption the architecture rests on, and the one that has never
//! been measured. The design broadcasts *values* rather than pixels — a beat, a
//! sensor reading, a slider — and every device turns those into light itself.
//! That only works if a value sent once arrives everywhere, sixty times a
//! second, on whatever access point somebody already owns.
//!
//! Throwaway, per the plan. It exists to produce one number.
//!
//! # What it measures
//!
//! One device sends a `CHAN`-shaped datagram to a multicast group every
//! 16 667 µs, carrying the `producer_seq` the wire format already has. Every
//! other device joins the group and counts gaps in that sequence, which is
//! **loss**, and the spread of arrival intervals, which is **jitter**.
//!
//! Jitter is the number that matters and the one a mean would hide. A channel
//! reliably 40 ms late can be compensated for; one usually 2 ms late and
//! occasionally 60 ms late cannot, because a receiver never knows which kind of
//! packet it is holding.
//!
//! # Why multicast rather than broadcast
//!
//! They are close cousins on WiFi — both flooded, both unacknowledged, both sent
//! at a low basic rate — but a consumer AP treats them differently in one way
//! that matters: it may stop forwarding a multicast group nobody has reported
//! membership in. That IGMP behaviour is part of what is being tested, so the
//! receivers join the group properly rather than sidestepping it with broadcast.
//!
//! # Roles
//!
//! `LUMEN_ROLE=source` sends. Anything else listens and reports.

#![no_std]
#![no_main]

mod chan;
mod stats;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;

use esp_wifi::wifi::{ClientConfiguration, Configuration, WifiDevice, WifiStaDevice};
use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::socket::{dhcpv4, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

/// Credentials come from the environment at build time, so they are never in the
/// repository. Build with:
///
/// ```text
/// LUMEN_WIFI_SSID='...' LUMEN_WIFI_PASS='...' cargo build --release
/// ```
const SSID: &str = env!("LUMEN_WIFI_SSID");
const PASS: &str = env!("LUMEN_WIFI_PASS");

/// `source` sends. Anything else listens and reports.
const ROLE: &str = env!("LUMEN_ROLE");

/// `multicast` or `unicast`.
///
/// The whole point of the second mode. Multicast is what the design wants
/// because one send reaches every device; unicast is the fallback the plan
/// prepared, and the only way to know whether it is needed is to measure both on
/// the same hardware and the same access point within minutes of each other.
const MODE: &str = env!("LUMEN_MODE");

/// A sink the source has heard from, for unicast mode.
const MAX_SINKS: usize = 8;

/// How often a sink announces itself so the source can address it.
///
/// Only used in unicast mode, and cheap: one datagram a second against sixty.
const HELLO_INTERVAL_US: u64 = 1_000_000;

/// A sink saying "I am here". One byte, and not a `CHAN`, so it cannot be
/// mistaken for the traffic being measured.
const HELLO: u8 = 0xA5;

/// The port the flood uses.
const PORT: u16 = 5353 + 1001;

/// The group. `239.x` is the administratively scoped range — the one reserved
/// for exactly this, a private network's own traffic, which will not be
/// forwarded off the local network by anything that respects the scope.
const GROUP: Ipv4Address = Ipv4Address::new(239, 12, 34, 56);

/// How many datagrams a receiver counts between reports. At 60 Hz this is a
/// report every ten seconds, which is often enough to watch and rare enough that
/// printing does not itself disturb the timing being measured.
const REPORT_EVERY: u32 = 600;

/// Heap for the WiFi driver. It allocates internally and there is no way around
/// it; nothing in the spike itself allocates.
const HEAP_BYTES: usize = 72 * 1024;

/// Microseconds since boot. The clock everything here is measured against.
fn now_us() -> u64 {
    esp_hal::time::now().duration_since_epoch().to_micros()
}

fn main_loop(
    mut controller: esp_wifi::wifi::WifiController<'_>,
    device: WifiDevice<'_, WifiStaDevice>,
    mac: [u8; 6],
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
    println!("== connecting to {SSID}");

    // What is in range, printed before trying. An association that never
    // completes looks exactly like a hang, and the first question is always
    // whether the AP is visible at all.
    match controller.scan_n::<16>() {
        Ok((networks, count)) => {
            println!("== {count} networks in range");
            for n in networks.iter() {
                println!("   {:>4} dBm  {}", n.signal_strength, n.ssid);
            }
        }
        Err(e) => println!("== scan failed: {e:?}"),
    }

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match controller.connect() {
            Ok(()) => {}
            Err(e) => println!("== connect() attempt {attempt}: {e:?}"),
        }
        // Association is not instant and `connect` only starts it. Poll for a
        // bounded time, then say so and try again rather than spinning forever
        // with nothing on the wire to show for it.
        let until = now_us() + 8_000_000;
        while now_us() < until {
            if matches!(controller.is_connected(), Ok(true)) {
                println!("== associated after {attempt} attempt(s)");
                break;
            }
        }
        if matches!(controller.is_connected(), Ok(true)) {
            break;
        }
        println!("== not associated after attempt {attempt}; retrying");
    }

    // Power save off, **after association**, and the ordering is the finding.
    //
    // Set before `connect`, it did not reliably survive: two identical C3s on
    // one network behaved completely differently, one receiving multicast on
    // schedule and the other in 110 ms bursts with nine times the loss. No error
    // was reported either time. Applying it once associated made them agree.
    //
    // The cost is real and belongs in the power budget: a device holding a
    // shared clock or reading a 60 Hz channel cannot sleep between beacons.
    if let Err(e) = controller.set_power_saving(esp_wifi::config::PowerSaveMode::None) {
        println!("== could not disable power save: {e:?}");
    } else {
        println!("== power save off");
    }

    // smoltcp, driven directly. A blocking stack would be less code, but the
    // crate the examples use is not published, and driving it here keeps the
    // timestamps under this file's control — which for a spike about
    // timestamping is the part that must not be someone else's abstraction.
    let config = Config::new(EthernetAddress::from_bytes(&mac).into());
    let mut iface = Interface::new(config, &mut device, Instant::from_micros(now_us() as i64));

    let mut sockets_storage = [SocketStorage::EMPTY; 3];
    let mut sockets = SocketSet::new(&mut sockets_storage[..]);
    let dhcp = sockets.add(dhcpv4::Socket::new());

    // Generous, because the first version of this spike was not and it mattered.
    // With sixteen slots the source refused 7% of its own sends when smoltcp's
    // transmit queue backed up behind the radio - and a send that never left the
    // device is not network loss, however much it looks like it from the far
    // end. The measurement is only about the network once the sender stops
    // being the bottleneck.
    let mut rx_meta = [udp::PacketMetadata::EMPTY; 64];
    let mut rx_buf = [0u8; 8192];
    let mut tx_meta = [udp::PacketMetadata::EMPTY; 64];
    let mut tx_buf = [0u8; 8192];
    let mut socket = udp::Socket::new(
        udp::PacketBuffer::new(&mut rx_meta[..], &mut rx_buf[..]),
        udp::PacketBuffer::new(&mut tx_meta[..], &mut tx_buf[..]),
    );
    socket.bind(PORT).expect("bind");
    let udp_handle = sockets.add(socket);

    println!("== waiting for a lease");
    let mut have_lease = false;
    let is_source = ROLE == "source";
    let mut sink = chan::Sink::new();
    let mut seq: u32 = 0;
    let mut next_send_us = 0u64;
    let mut joined = false;
    let unicast = MODE == "unicast";
    let mut sinks = [Ipv4Address::new(0, 0, 0, 0); MAX_SINKS];
    let mut sink_count = 0usize;
    let mut next_hello_us = 0u64;
    let mut windows: u32 = 0;
    // Power save is set once, after association, and that is as reliable as it
    // gets. Setting it before `connect` did not survive association. Re-applying
    // it every second was tried and is *worse*: calling into the radio driver
    // that often disturbs it, and both sinks went from 4-6% loss to 13-17%.
    //
    // What is left is a real limitation of the measurement rather than of the
    // network. On esp-wifi 0.12 a station sometimes sleeps anyway, and one that
    // does shows an unmistakable signature - arrivals clustered 250 us apart in
    // bursts about 110 ms apart, which is the beacon interval. Any window with a
    // gap p50 far below the send interval is a sleeping receiver and says
    // nothing about the network.
    // The source's own failures, which would otherwise be indistinguishable
    // from network loss. `send_slice` fails when smoltcp's transmit buffer is
    // full, and ignoring that would make a spike that drops its own packets
    // report the network as broken.
    let mut send_failed: u32 = 0;
    let mut send_ok: u32 = 0;
    // How late each send actually was against its 16 667 us deadline, which
    // says whether the sender is keeping up at all.
    let mut lateness = stats::Histogram::new();

    loop {
        let timestamp = Instant::from_micros(now_us() as i64);
        iface.poll(timestamp, &mut device, &mut sockets);

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
                println!("== lease: {} as {ROLE}", cfg.address);
                have_lease = true;
                next_send_us = now_us();
            }
            continue;
        }

        // Joining the group is what tells the AP to forward it. An access point
        // that has seen no membership report is entitled to drop the traffic,
        // and whether it does is part of what this measures - so a receiver
        // joins properly rather than sidestepping it with broadcast.
        // A sink announces itself so a unicast source knows where to send. In
        // multicast mode nothing needs it, and it is skipped rather than left
        // running - a stray datagram a second is small, but it is traffic this
        // spike would otherwise be measuring against itself.
        if unicast && !is_source && have_lease {
            let t = now_us();
            if t >= next_hello_us {
                let socket = sockets.get_mut::<udp::Socket>(udp_handle);
                let to = IpEndpoint {
                    addr: IpAddress::Ipv4(Ipv4Address::new(255, 255, 255, 255)),
                    port: PORT,
                };
                let _ = socket.send_slice(&[HELLO], to);
                next_hello_us = t + HELLO_INTERVAL_US;
            }
        }

        if !joined && !is_source && !unicast {
            match iface.join_multicast_group(GROUP) {
                Ok(_) => {
                    println!("== joined {GROUP}");
                    joined = true;
                }
                Err(e) => println!("== could not join {GROUP}: {e:?}"),
            }
        }

        let socket = sockets.get_mut::<udp::Socket>(udp_handle);
        let mut buf = [0u8; 64];
        while let Ok((n, _meta)) = socket.recv_slice(&mut buf) {
            // Timestamped on arrival, before anything else touches it: every
            // microsecond between the wire and here is jitter this spike would
            // otherwise attribute to the network.
            let t = now_us();
            if is_source {
                // A hello from a sink that wants to be sent to.
                if n == 1 && buf[0] == HELLO {
                    // Only IPv4 is configured, so the address can only be one
                    // thing; destructured rather than matched to say so.
                    let IpAddress::Ipv4(addr) = _meta.endpoint.addr;
                    if !sinks[..sink_count].contains(&addr) && sink_count < MAX_SINKS {
                        sinks[sink_count] = addr;
                        sink_count += 1;
                        println!("== sink {sink_count}: {addr}");
                    }
                }
                continue;
            }
            sink.on_datagram(t, &buf[..n]);
            if sink.received() >= REPORT_EVERY {
                windows += 1;
                // The first window is startup - association, the group join,
                // the source's queue draining - and says nothing about the
                // network. Reported anyway, marked, so nobody has to wonder
                // whether it was dropped or simply never happened.
                println!("window {windows}{}", if windows == 1 { " (startup)" } else { "" });
                sink.report();
                sink.begin_window();
            }
        }

        if is_source {
            let t = now_us();
            if t >= next_send_us {
                let out = chan::encode(1, seq, t);
                if unicast {
                    // One datagram per sink. That is the cost the design was
                    // avoiding: airtime grows with the number of devices, where
                    // multicast is one send however many are listening.
                    for i in 0..sink_count {
                        let to = IpEndpoint {
                            addr: IpAddress::Ipv4(sinks[i]),
                            port: PORT,
                        };
                        match socket.send_slice(&out, to) {
                            Ok(()) => send_ok += 1,
                            Err(_) => send_failed += 1,
                        }
                    }
                } else {
                    let to = IpEndpoint {
                        addr: IpAddress::Ipv4(GROUP),
                        port: PORT,
                    };
                    match socket.send_slice(&out, to) {
                        Ok(()) => send_ok += 1,
                        Err(_) => send_failed += 1,
                    }
                }
                lateness.add(t.saturating_sub(next_send_us) as i64);
                seq = seq.wrapping_add(1);
                if seq % REPORT_EVERY == 0 {
                    // Separated on purpose: a send this spike never made is not
                    // loss the network caused, and reporting the two together
                    // would blame the wrong thing.
                    println!(
                        "sent {seq}: {send_ok} ok, {send_failed} refused by the stack"
                    );
                    println!(
                        "   late p50={}us p95={}us p99={}us (deadline {}us)",
                        lateness.percentile(50),
                        lateness.percentile(95),
                        lateness.percentile(99),
                        chan::INTERVAL_US
                    );
                }
                // Advanced from the deadline rather than from now, so a late
                // send does not push every later one back and turn a hiccup
                // into a permanently slow sender.
                next_send_us += chan::INTERVAL_US;
                if next_send_us < t {
                    next_send_us = t + chan::INTERVAL_US;
                }
            }
        }
    }
}

#[esp_hal::main]
fn main() -> ! {
    // Before anything else, so a hang in init is distinguishable from a board
    // that is not running at all.
    println!();
    println!("== spike S3: multicast CHAN at 60 Hz, esp32c3");
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    // A statement rather than an item: the macro declares the backing array and
    // registers it, so it has to run inside a function.
    esp_alloc::heap_allocator!(HEAP_BYTES);

    println!("== role {ROLE}, mode {MODE}, {} us between sends", chan::INTERVAL_US);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let init = esp_wifi::init(timg0.timer0, Rng::new(peripherals.RNG), peripherals.RADIO_CLK)
        .expect("wifi init");

    let (device, controller) =
        esp_wifi::wifi::new_with_mode(&init, peripherals.WIFI, WifiStaDevice).expect("sta mode");

    let mut mac = [0u8; 6];
    esp_wifi::wifi::sta_mac(&mut mac);
    println!(
        "== mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    main_loop(controller, device, mac)
}
