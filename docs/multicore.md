# Using the second core

Some of the family have two cores — the original ESP32 and the S3 — and most do
not: the C3, C2, C6 and H2 are single-core, and they are the cheap ones a house
fills up with. So this has to be an optimisation a device may take, never
something the design assumes.

## The obvious answer is the smaller one

The instinct is to put networking on one core and rendering on the other, so the
render loop stops being interrupted. That is worth something, and it is not where
the headroom is.

It is worth something because WiFi interrupt work is exactly the jitter a frame
timer feels, and [Spike S3](../spikes/s3-multicast/RESULTS.md) measured arrival
gaps with a p95 of 26 ms against a 16.7 ms frame. Moving the radio's work off the
rendering core makes a frame boundary land where it was asked to.

But it does not buy **capacity**, and capacity is what [Spike
S2](../spikes/s2-vm-throughput/RESULTS.md) found short: a C3 gets about 60 VM
instructions per pixel at 300 LEDs and 60 fps, against a criterion that asked for
a thousand. Networking is not what is eating the frame — the interpreter is,
at 134 cycles per instruction and 80% of that dispatch.

## The larger answer: split the pixels

**A frame's pixels are independent.** Each is a pure function of its own position
and the frame's hoisted values. Nothing in the pixel section reads another
pixel's result — `prev` is *this* pixel's own history — so two cores rendering
halves of a strip produce exactly the bytes one core rendering all of them
produces.

That is close to a straight doubling on a dual-core device, and it applies to the
part that is actually short.

**This has been built and measured.** `lumen_device::render::Shard` is the seam;
[Spike S4](../spikes/s4-dual-core/RESULTS.md) ran it on an ESP32-S3 over 300 LEDs
and the whole shipped corpus and got **2.08–2.20×**, with the output
**byte-identical to a single core**. The worst effect goes from 103% of a 60 fps
frame to 49%: on one core it does not fit, on two it fits with the frame half
empty.

It is checked rather than asserted, in three places.
`lumen-capi` has `two_halves_render_what_one_whole_does` for the C ABI,
`lumen-device` has `shards_render_what_one_whole_does` over several frames so the
per-LED history is covered, and S4 compares every pixel of every frame on real
silicon. If any of them diverges, a two-core device renders a different show from
a one-core device and the mesh stops agreeing with itself — which matters far
more than the speed does, because the whole architecture rests on every device
computing the same frame.

### Why it came out above 2×

Superlinear is normally a measurement error, so it needs an explanation rather
than a cheer. Each core renders half the LEDs, and the per-LED history is a
`BTreeMap` keyed by `(source, led)` with two accesses per pixel. Halving the
entries makes every one of those shallower, so the second core does not only
halve the work — it makes its own half cheaper.

Which also says where the remaining overhead is. That map is now the largest
per-pixel cost outside the VM itself, and dealing with it would help the
single-core chips, where the headroom is actually short.

### What the split actually requires

In Rust, through `lumen-device`:

1. Build one `Shard` per core over the device's LED count.
2. Give each core **its own `Renderer`**. The VM's register file survives from
   `frame` into every pixel of that frame — which is the whole reason hoisting
   pays — so two cores sharing one machine would be two cores writing one
   register file.
3. Split the output with `split_at_mut` and give each core the run its shard
   covers. No sharing, no locking, no copy: the shards own disjoint LEDs, and
   that is what a contiguous split is for.
4. Merge the `FrameReport`s with `FrameReport::merge` once the cores have joined.

Every shard runs the `frame` section for itself rather than receiving hoisted
registers from a neighbour. The section is a pure function of the program and
`t`, so each shard computes the same registers, and a little duplicated
arithmetic is far cheaper than the alternative — handing a live machine between
cores means shared mutable state, a barrier in the middle of a frame, and a crate
that can no longer be tested without threads. The one exception is a probe build,
where `Uniforms::probe` would record once per shard; probe builds render whole.

In C, through `lumen-capi`, the shape is the same but the machine is copied
rather than re-run: `lumen_frame` once, then `lumen_machine_clone`, then
`lumen_render_range` per core — passing the **whole strip's length** as `count`,
because `u` and `index` are relative to the strip and passing the slice length
makes each half render as though it were the whole strip. That looks like a
mirrored effect rather than an error, which is the worst way for a bug to
present.

The `sim` section stays on one core. It is not per-pixel, it writes shared state,
and only the sim master runs it at all.

## Where this belongs

**In the firmware, not in `lumen-device`.**

Everything in `lumen-device` is `on_event(now, ev) -> Vec<Action>` and performs
no I/O — no threads among the things it may not have. That constraint is what
buys deterministic replay, tests with no hardware, and behavioural conformance,
and it should not be spent on a speed-up available to a minority of chips.

The seam is already in the right place. The core says *what* to render; the shell
owns timers, sockets and cores, and how many of them it uses to answer is its
business. A firmware that splits the strip and one that does not are the same
device as far as every vector in `lumen-spec` is concerned.

## What it does not fix

The instruction budget is per pixel, and splitting the work across two cores does
not make an effect cheaper — it makes a device able to afford twice as much of
one. The **cost model does not change**: `OpCode::cost()` is still what the
compiler promises against, and a device that renders on two cores declares a
higher capacity rather than a different price list.

Nor does it help the chips that need it most. A C3 driving 300 LEDs at 60 fps is
the case at 87% of frame, and a C3 has one core. What two cores buy is a *bigger*
device being able to drive more, which is worth having and is not the same thing.

## Recommendation

1. **Take it in the firmware, on dual-core devices, for the pixel loop.** Done:
   `Shard`, `render_cores` in a board definition, and S4's measurement.
2. **Pin the radio to the other core** while doing so. It is nearly free once the
   split exists, and it is what makes a frame boundary land on time.
3. **Do not let it reach the core crates.** A dual-core path in `lumen-device`
   would cost the sans-IO property that every test and every conformance vector
   depends on, to speed up a minority of the family.
4. **Declare the capacity, not a new cost model.** A device that renders on two
   cores reports it can afford more; what an effect costs is unchanged.
