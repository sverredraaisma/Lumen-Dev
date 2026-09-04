# Spike S4 — splitting the pixel loop across the S3's two cores

**Hardware:** ESP32-S3 (rev v0.2, 4 MB flash), 240 MHz, both cores.
**Under test:** `lumen_device::Renderer::render_shard` — the real render loop,
not a copy of it. 300 LEDs, 30 frames averaged, five effects from the shipped
corpus.

## Result

| effect | budget | 1 core | 2 cores | speed-up | % of a 60 fps frame | identical? |
|---|---|---|---|---|---|---|
| 07-alert | 136 | 9 081 µs | 4 114 µs | 2.20× | 24% | yes |
| 01-breathe | 215 | 11 176 µs | 5 208 µs | 2.14× | 31% | yes |
| 12-panel-plasma | 388 | 15 678 µs | 7 466 µs | 2.09× | 44% | yes |
| 05-beat-strobe | 462 | 17 169 µs | 8 245 µs | 2.08× | 49% | yes |
| 03-drift | 562 | 13 437 µs | 6 362 µs | 2.11× | 38% | yes |

**Passes.** Every effect renders 300 LEDs in under half a 60 fps frame on two
cores, and **every one produces byte-identical output to a single core.**

The worst case moves from 103% of a frame to 49%. On one core, `05-beat-strobe`
at 300 LEDs and 60 fps does not fit; on two it fits with the frame half empty.

## The identity is the result that matters

The speed-up is worth having. The identity is what decides whether the feature
ships.

Every device in a mesh computes the same show from the same clock. That is what
a gradient spanning six strips rests on, and it is why the VM is fixed point
rather than float. A two-core device that rendered even one pixel differently
from a one-core device would break that agreement — and it would not be visible
until someone put two kinds of device in one room, by which time the cause would
be six months old.

So the spike compares bytes, not just microseconds: 300 pixels × 30 frames × 5
effects, one core against two, all equal. `lumen-device` carries the same claim
as a host test (`shards_render_what_one_whole_does`); this is the claim on real
silicon, on two real cores, with two real caches.

## Why more than 2×

Superlinear speed-up is normally a measurement error, so it needs an
explanation rather than a cheer.

Each core renders half the LEDs, and the per-LED history is a `BTreeMap` keyed by
`(source, led)`. Halving the number of entries makes every lookup and insert
shallower — and there are two of them per pixel. So the second core does not only
halve the work, it makes its own half cheaper. The same applies to cache
footprint: half a strip's history fits better than a whole one.

That also says where the remaining per-pixel overhead is. Against S2's raw-VM
figures for the same effects on the same chip, the render loop still costs about
12 µs per pixel beyond running the effect. Most of it is that map. It is the next
thing worth looking at, and it would help single-core devices — which is where
the headroom is actually short.

## Two bugs it found, both fixed

### `07-alert` rendered nothing at all

On the first run it faulted every frame with `BudgetExceeded` and drew nothing,
having cost 24 µs for what should have been 9 000 pixels.

The render loop set the VM's fuel limit from the program header's `budget`, and
that field is the cost of the **pixel** section — the number a device multiplies
by its LED count to decide whether it can afford a source. It was being spent on
the `frame` section too. The frame section is the part deliberately made
expensive, because hoisting work out of the per-pixel path is the VM's entire
performance story; charging it a per-pixel allowance faults exactly the effects
that hoist most, which is to say the well-written ones.

Fixed by giving the frame section its own allowance, computed from the bytecode
with `Program::section_cost` — the same sum the compiler reports, so no wire
change and no re-publishing of anything. `07-alert` now renders in 9 081 µs on
one core.

This had been recorded as an open design question. It was not a question; it was
a corpus effect that could not run.

### A linear scan per pixel

The single-core column was about 7 ms per frame worse than S2's figures for the
same effects on the same chip. S2 measures the VM alone, so some gap is expected
— but not one the size of the render.

`render_source` looked up each LED with `leds.iter().find(...)`, a scan of the
whole strip, once per pixel. Quadratic in the strip: 90 000 comparisons per
source per frame at 300 LEDs. A strip is `0..n` in order, so the index is the
position; trying that first and keeping the search as a fallback took 20–25% off
every frame, single-core and dual-core alike.

Worth noting how it stayed hidden. Every host test uses four to twenty-four
LEDs, where a linear scan is free. It took 300 LEDs on real silicon to make it
visible, which is the argument for spikes in one sentence.

## What this does not answer

**Two cores, one strip, one device.** It says nothing about contention with a
WiFi stack, which is the other thing the second core is for and what S3's jitter
measurement points at. A firmware that renders on both cores has nowhere left to
put the radio; the answer is likely that the *app* core renders while the *pro*
core keeps the network, and the split is between comms and rendering rather than
across the pixels. That needs measuring with a live radio and is not what this
spike did.

**Imbalance.** A contiguous split gives one core the whole of a mask if the mask
covers half the strip, and this corpus does not contain that case — `07-alert`'s
mask is time-based, not positional. The design accepts the imbalance in exchange
for `split_at_mut` handing each core its own memory with no sharing; the worst
case is no speed-up, never a slow-down.

**More than two cores.** `Shard` takes any count and the host tests cover up to
five, but nothing in this family has more than two.

## Running it

```bash
cd lumen-dev/spikes/s4-dual-core
export PATH="$HOME/.rustup/toolchains/esp/xtensa-esp-elf/bin:$PATH"
rustup run esp cargo build --release
espflash flash --port COM7 --monitor target/xtensa-esp32s3-none-elf/release/s4-dual-core
```

It repeats every 10 seconds and prints `SPLIT RENDERS A DIFFERENT FRAME` in the
one case anybody should care about.
