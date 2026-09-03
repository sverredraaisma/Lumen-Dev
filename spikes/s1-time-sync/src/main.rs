//! Spike S1: does time sync hold under ±500 µs across ESP32s on ordinary WiFi?
//!
//! The other assumption the architecture rests on. Every device renders the same
//! show clock independently, with nothing coordinating the pixels — a wave that
//! crosses six strips is six devices each computing where the wave is *now*. If
//! they disagree by more than about a frame, the wave visibly tears, and the
//! whole design of "broadcast the time, not the pixels" falls over.
//!
//! Throwaway, per the plan. It exists to produce one number.
//!
//! # What it measures
//!
//! The real exchange from the wire format: `SYNC_REQ` carries `t1`, `SYNC_RESP`
//! echoes it with `t2` and `t3`, and the requester records `t4`. Offset is
//! `((t2-t1) + (t3-t4)) / 2` and round-trip is `(t4-t1) - (t3-t2)`, with any
//! sample whose RTT exceeds 1.5× the running minimum discarded.
//!
//! Over a real AP, on ordinary 2.4 GHz, with whatever else is on the network.
//! That last part is the point: the risk is not the arithmetic, it is that a
//! consumer AP buffers, retries, and parks clients in power-save, and that its
//! jitter is far larger than the number we need.
//!
//! # What it can and cannot tell you
//!
//! Two boards cannot check each other against a common reference, so this
//! measures the **dispersion of the offset estimate** — how much successive
//! measurements of the same quantity disagree — plus the drift of the follower's
//! clock against the master's. Dispersion bounds the error: a follower cannot
//! track better than its estimate is stable.
//!
//! It does not measure absolute skew. That needs a wire: one board pulsing a
//! GPIO on its show-second and the other timestamping the edge. If the numbers
//! here are near the limit, that is the next measurement, and it needs two
//! jumper leads rather than a better algorithm.
//!
//! # Roles
//!
//! Both boards run this binary. `LUMEN_ROLE=master` is the time source; it
//! answers `SYNC_REQ` and broadcasts a `TICK` every second so a follower can
//! find it without being told an address. Anything else is a follower.

#![no_std]
#![no_main]

mod stats;
mod sync;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;

use esp_wifi::wifi::{ClientConfiguration, Configuration, WifiDevice, WifiStaDevice};
use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::socket::{dhcpv4, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr};

/// Credentials come from the environment at build time, so they are never in the
/// repository. Build with:
///
/// ```text
/// LUMEN_WIFI_SSID='...' LUMEN_WIFI_PASS='...' cargo build --release
/// ```
const SSID: &str = env!("LUMEN_WIFI_SSID");
const PASS: &str = env!("LUMEN_WIFI_PASS");

/// `master` is the time source. Anything else follows.
const ROLE: &str = env!("LUMEN_ROLE");

/// The port both messages use. Arbitrary, and outside the ephemeral range so a
/// DHCP lease renewal cannot land on it.
const PORT: u16 = 5353 + 1000;

/// How often a follower asks. The wire format says 30 s once synced; this spike
/// asks far more often, because it is trying to characterise the distribution
/// rather than to hold a clock, and a 24 h run at 30 s would yield 2 880 samples
/// where this yields one every 200 ms.
const REQUEST_INTERVAL_MS: u64 = 200;

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
    // Power save off, and this is not a detail of the spike — it is a
    // requirement the mesh inherits. A station left in the default mode parks
    // its radio between the AP's beacons and only wakes on DTIM, which turns a
    // sub-millisecond LAN round trip into tens of milliseconds of quantised
    // waiting. The first run of this spike measured a 17 ms minimum round trip
    // on an idle network and read as a catastrophic result; it was the radio
    // asleep, not the network.
    //
    // A device that renders on a shared clock cannot sleep between beacons, so
    // the cost is real and belongs in the power budget rather than being
    // recovered here.
    if let Err(e) = controller.set_power_saving(esp_wifi::config::PowerSaveMode::None) {
        println!("== could not disable power save: {e:?}");
    }
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

    // smoltcp, driven directly. A blocking stack would be less code, but the
    // crate the examples use is not published, and driving it here keeps the
    // timestamps under this file's control — which for a spike about
    // timestamping is the part that must not be someone else's abstraction.
    let config = Config::new(EthernetAddress::from_bytes(&mac).into());
    let mut iface = Interface::new(config, &mut device, Instant::from_micros(now_us() as i64));

    let mut sockets_storage = [SocketStorage::EMPTY; 3];
    let mut sockets = SocketSet::new(&mut sockets_storage[..]);
    let dhcp = sockets.add(dhcpv4::Socket::new());

    let mut rx_meta = [udp::PacketMetadata::EMPTY; 16];
    let mut rx_buf = [0u8; 1536];
    let mut tx_meta = [udp::PacketMetadata::EMPTY; 16];
    let mut tx_buf = [0u8; 1536];
    let mut socket = udp::Socket::new(
        udp::PacketBuffer::new(&mut rx_meta[..], &mut rx_buf[..]),
        udp::PacketBuffer::new(&mut tx_meta[..], &mut tx_buf[..]),
    );
    socket.bind(PORT).expect("bind");
    let udp_handle = sockets.add(socket);

    println!("== waiting for a lease");
    let mut have_lease = false;
    let mut state = sync::State::new(ROLE);
    let mut next_action_us = 0u64;

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
                next_action_us = now_us();
            }
            continue;
        }

        // Receive first, so a reply is timestamped as close to arrival as the
        // stack allows. `t4` taken after `poll` is already later than the wire,
        // and that error goes straight into the offset.
        let socket = sockets.get_mut::<udp::Socket>(udp_handle);
        let mut buf = [0u8; 64];
        while let Ok((n, meta)) = socket.recv_slice(&mut buf) {
            let t = now_us();
            if let Some((to, reply)) = state.on_datagram(t, &buf[..n], meta.endpoint) {
                let _ = socket.send_slice(&reply, to);
            }
        }

        let t = now_us();
        if t >= next_action_us {
            if let Some((to, out)) = state.on_tick(t) {
                let _ = socket.send_slice(&out, to);
            }
            next_action_us = t + state.interval_us();
        }
    }
}

#[esp_hal::main]
fn main() -> ! {
    // Before anything else, so a hang in init is distinguishable from a board
    // that is not running at all.
    println!();
    println!("== spike S1: time sync over ordinary WiFi, esp32c3");
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    // A statement rather than an item: the macro declares the backing array and
    // registers it, so it has to run inside a function.
    esp_alloc::heap_allocator!(HEAP_BYTES);

    println!("== role {ROLE}, asking every {REQUEST_INTERVAL_MS} ms");

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
