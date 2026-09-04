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

It is also checked rather than asserted.
`lumen-capi` has `two_halves_render_what_one_whole_does`, which renders a strip
whole and then in two halves on two machines and compares the bytes. If that ever
diverges, a two-core device renders a different show from a one-core device and
the mesh stops agreeing with itself — which matters far more than the speed does,
because the whole architecture rests on every device computing the same frame.

### What the split actually requires

1. Run the **`frame` section once**, on one core. Its results are the hoisted
   values, and they live in the machine's registers.
2. **Copy the machine** to the second core (`lumen_machine_clone`). It needs
   those registers, and it must not share the ones it is about to write.
3. Each core renders its own range (`lumen_render_range`), passing the **whole
   strip's length** as `count` — `u` and `index` are relative to the strip, and
   passing the slice length instead makes each half render as though it were the
   whole strip. That looks like a mirrored effect rather than an error, which is
   the worst way for a bug to present.

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

1. **Take it in the firmware, on dual-core devices, for the pixel loop.** The API
   is there and the equivalence is tested.
2. **Pin the radio to the other core** while doing so. It is nearly free once the
   split exists, and it is what makes a frame boundary land on time.
3. **Do not let it reach the core crates.** A dual-core path in `lumen-device`
   would cost the sans-IO property that every test and every conformance vector
   depends on, to speed up a minority of the family.
4. **Declare the capacity, not a new cost model.** A device that renders on two
   cores reports it can afford more; what an effect costs is unchanged.
