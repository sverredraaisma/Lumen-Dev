# M2 — two nodes, one timebase

**Under test:** `lumen_device::node::Node` — the election and sync state machines
the simulator exercises — on an ESP32-C3 and on a desktop peer running the
identical code over the identical wire format.

The desktop peer is not a stand-in for a second ESP32 and does not pretend to
be. It is the *useful* second node precisely because it shares nothing with the
first: no clock hardware, no radio, not even a CPU architecture. Two copies of
one implementation agreeing proves very little; x86-64 and RISC-V agreeing
proves the protocol.

## Result

**Election, sync and failover all work on real hardware.**

```
== now Follower in epoch 1     the stronger peer wins
== disciplined by 4565361 us   the join offset, applied at once
== show clock acquired
== show clock lost             the leader is killed
== now Leader in epoch 2       the survivor takes over
== show clock acquired         and is now the timebase itself
```

Steady-state corrections once synchronised: **−255 µs, −348 µs, −2.3 ms**.

Clock agreement, measured per five-second window as "receipt time minus the
sender's show time":

| window | mean | worst |
|---|---|---|
| 1 (device booting) | −121 s | 121 s |
| 2 (join applied) | 2.75 s | 4.59 s |
| 3 | **4.2 ms** | 16 ms |
| 4 | **4.0 ms** | 26 ms |

**That 4 ms is an upper bound, not the clock offset.** The measurement includes
one-way network delay, which on this WiFi is itself a few milliseconds, and
nothing here separates the two. The corrections the device applies are the
honest figure for how far apart the clocks are, and those are sub-millisecond to
low-millisecond — consistent with S1, which measured p50 225–350 µs between two
ESP32s and needed a round trip to do it.

Measuring the residual properly needs the same round-trip treatment S1 used, and
is not done.

## Three bugs, none of which a simulator can show

### One lost datagram deadlocked time sync for ever

The probe is one at a time, and the flag saying one was outstanding was cleared
**only** by a matching answer. A lost request, a lost answer, or an answer
arriving after its question had been forgotten left the flag set, and every
later probe was refused. The node reported itself a healthy follower
indefinitely.

The device and the peer exchanged **94 round trips, lost one, and stopped**.
Ticks kept arriving once a second throughout, so every other sign was green.

Fixed in `lumen-device`: the outstanding probe has a deadline, and the timer
accounts for it. A lossless simulated network never produces this, which is how
it survived to reach a strip.

### The slew rate was silently zero

`elapsed * 200 / 1_000_000` is an integer division, and the clock is advanced
thousands of times a second — so `elapsed` is tens of microseconds, every
interval's budget rounded to nothing, and the correction never moved. The
follower reported `pending 17231680 us` unchanged in every report for as long as
anyone watched.

The remainder is carried now rather than discarded.

### Seventeen seconds is not drift

Slewing a join offset at 200 ppm takes a day, and the node reports itself synced
throughout. A correction beyond 100 ms is now applied at once: a device joining a
running show has not drifted, it did not know the time. 100 ms is far above any
real drift — S1 measured p95 offsets of 1.5 ms — and far below a join, so nothing
real lands in between.

## And one on the desktop, worth knowing

The peer's limited broadcast left by a **WSL/Hyper-V virtual adapter**: it heard
its own ticks arrive from 172.25.192.1 while the device on 192.168.1.66 heard
nothing, so both elected themselves.

Mesh traffic now goes to the broadcast address **and** unicast to every peer
already known. That is not only a workaround: Spike S3 measured 4–6% loss on
multicast against 0.00% on unicast over this same access point, so for the
handful of peers a house has, addressing them directly is simply better. The
broadcast remains only to find peers that are not yet known.

## What M2 still owes

The exit criterion is *"two devices, powered on in either order, discover each
other, elect a timebase, and change colour on the same frame."*

Election, discovery and failover are demonstrated. **"The same frame" is not.**
Both nodes agree on show time, and `lumen-vm::digest` proves a host and a device
render identical frames for the same show time — but nothing yet renders the same
effect on two nodes at once and compares. The desktop peer does not render at
all.

Two devices with LEDs would settle it by eye in a second. One ESP32-S3 is on the
desk and was not connected when this ran.

## Running it

```bash
# The device, either chip:
cargo build --release --features esp32c3 --target riscv32imc-unknown-none-elf
rustup run esp cargo build --release --features esp32s3 \
  --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc

# The second node. Capacity decides who leads: a C3 reports 1194, an S3 1714.
cd sender && cargo run --release -- --peer --capacity 5000
```
