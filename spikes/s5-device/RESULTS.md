# Spike S5 — a whole device, driving real light

**Hardware:** ESP32-C3 devkit, 30 SK6812 **RGBW** on GPIO4, driven from RMT.
**Under test:** the whole chain, with nothing stubbed —

```
effect.lfx -> lumen-lang -> ProgBegin/ProgChunk/ProgEnd over UDP
           -> SourceStack -> lumen-device Renderer -> lumen-vm
           -> RMT -> the strip
```

## Result

**Passes.** A program written on a desktop compiles, crosses the network, joins
the source stack, renders through the real VM and lights a real strip at 30 fps.

```
== program complete: 1784 bytes, 435 units/pixel
== source pushed at priority 100
== 150 frames in 5 s (30 fps), 13165 units/frame, 25 datagrams
```

The frame cost is a free arithmetic check on the render loop: 435 units/pixel ×
30 pixels = 13 050, and the remainder is the `frame` section, paid once. 13 165.

Every number this project had measured until now was measured with the output
thrown away. This is the first one anybody can look at.

## What it found

Five bugs, in rough order of how much they mattered. Three were in shipping code
and two were in this spike; all five are fixed.

### `dt` was the show clock, and every feedback effect was silently wrong

`lumen-lang` compiled `dt` to the same register as `t`, so it was the absolute
show time rather than the gap between frames.

Nothing failed. `pow(decay, dt * 60)` — which is how the language documents
rate-independent decay, in the construct the comet example itself calls *the most
common mistake in a feedback effect* — saturated instead. `keep` came out at one,
so trails never decayed: the strip filled with stuck white pixels, one at a time,
and stayed that way.

250 host tests had nothing to say about it, because the wrongness is only visible
across frames on a device. It cost a register to fix, and the corpus now peaks at
31 of 32 — see **What it cost** below.

### The frame budget was reported for one pixel

`FrameReport::spent` read `Machine::spent()` once after the pixel loop, and that
method reports the *last* invocation. A device rendering thirty LEDs reported 391
units a frame, which is what one pixel costs.

A device sizes its frame from that number, so it was wrong by the length of the
strip — and the longer the strip, the further out the answer, which is exactly
backwards.

### No output stage exists

`Rgb`'s own documentation says gamma is applied once by the output stage. There
is no output stage, anywhere in the project. The firmware here writes linear
values straight to the LEDs.

That is physically correct and perceptually wrong: the dark end of a fade
collapses into a handful of 8-bit steps, which is why a correctly-computed comet
tail reads as "only the head". **Not fixed here** — it belongs in shared code so
that a C firmware through `lumen-capi` matches a Rust one, and it is a colour
decision rather than a spike one.

### WiFi interrupts corrupted every frame

The first version with the radio on showed a stable pattern with random pixels
flashing through it. RMT holds 48 words of channel RAM and the CPU refills half
of it every 24 words — about forty times for a 30-LED RGBW frame, each with a
30 µs deadline. A WiFi interrupt that overruns one leaves the line idle
mid-frame, the strip takes the gap for a latch, and everything after it lands in
the wrong LED.

Holding interrupts off across the frame fixes it completely, and is a stopgap:
30 LEDs is 1.2 ms, so 3.6% of the time at 30 fps, but 300 LEDs would be 12 ms —
most of a frame with the radio deaf.

**Since fixed with SPI and DMA** (`strip_dma.rs`), selected by `LUMEN_DRIVER`.
esp-hal 0.23's RMT has no DMA support, but a shift register clocking fixed-width
bits is a perfectly good pulse generator: each LED bit becomes four SPI bits at
3.2 MHz, so a zero is `1000` (312 ns high) and a one is `1100` (625 ns), both
inside what an SK6812 accepts. Three bits at 2.4 MHz is the commonly seen choice
and does *not* work here — it puts a one at 833 ns, out of spec. That kind of
error does not fail on a bench; it fails on the twentieth LED of a long run.

Measured, 30 LEDs, comet, both drivers on the same hardware:

| | render | show |
|---|---|---|
| RMT, interrupts held off | 3 103 µs | 1 340 µs |
| SPI + DMA, no critical section | 3 150 µs | 1 394 µs |

**DMA is not faster, and was never going to be.** 30 LEDs × 32 bits × 1.25 µs is
1.2 ms of wire time whatever drives it, and both land within 4% of that. What
changes is what the CPU is doing during those 1.4 ms: RMT spends them refilling
a 48-word buffer against a 30 µs deadline with interrupts disabled, and DMA
spends them idle with the radio being serviced normally.

The remaining win is not taken yet: with DMA the *next* frame can be rendered
while the current one is still going out. At 30 LEDs that would save 1.4 ms of
33; at 300 it would overlap 12 ms of wire time with 30 ms of rendering, which is
where it starts to matter. `SpiDmaBus::write` blocks, so it needs the
non-blocking transfer API and a second buffer.

### Show time saturated, then lost its fraction

Two of mine, stacked. The sender used Unix epoch time as show time — about
1.79 × 10¹⁵ µs — and Q16 holds roughly 32 768 seconds, so `t * speed` saturated.
Then the firmware's µs-to-Q16 conversion computed the sub-second part in `u32`,
where `999_999 << 16` overflows, so only whole seconds advanced cleanly.

Together: an effect visibly updating once or twice a second, with the pixels
jittering in between, on a device correctly reporting thirty frames a second.

The conversion existed twice — correctly in `lumen-capi` in `i64`, and wrongly
here — so it is now one `Q16::from_micros` in the VM that both use. Show time
counts from the start of the show; wall time travels in the `Tick`'s own field,
which is where a device wanting a date should look.

### No output stage existed — and it is not a gamma stage

Recorded above as "no output stage", which was right, and diagnosed as a missing
gamma curve, which was wrong.

A WS2812-class LED's PWM is proportional to the light it emits and a Lumen colour
is already linear light, so an sRGB curve on the way out would make every strip
brighter than the effect asked for. The problem it gets reached for is real but
it is **quantisation**: eight bits of linear PWM cannot represent anything below
1/255, so the dark end of a fade arrives in a few visible steps and then stops
early.

`lumen_vm::output` now does the three things that were missing, and measured on
this hardware:

- **Power derating**, which every board declares a `max_current_ma` for and
  nothing implemented. Thirty SK6812 at full white want about 1.2 A; against a
  500 mA budget the stage lands the frame at **495–499 mA** and says it derated.
  Scaled uniformly rather than clipped, so an over-budget frame dims instead of
  losing its highlights and shifting colour.
- **Temporal dithering**, deterministic so two devices showing one gradient stay
  in step.
- **Brightness**, which had nowhere to live.

### Nothing connected channels to the VM

The device had a `Channels` store and the VM had a `Uniforms` trait, and there
was nothing between them, so every `CHREAD` returned zero. Silent, because zero
is also what a channel with no producer correctly returns: an effect reading a
live slider and an effect reading nothing look identical on a strip.

`lumen_device::channels::ChannelUniforms` now bridges them, and the realtime path
is proven on hardware — `04-pulse` with its `audio` channel claimed and driven at
30 Hz from a desktop, rendering at 30 fps on the C3.

### A sender that blocked 500 ms per loop

Driving that channel, the strip looked like it was running at about 1 fps. The
device was not slow: it reported a rock-steady 30 fps throughout, of a value
changing twice a second, because the sender's socket had a 500 ms read timeout
and its loop could only publish twice a second whatever `DRIVE_HZ` said.

**A perfectly smooth render of a staircase input is indistinguishable from a slow
device by eye.** What made it a one-step diagnosis was the datagram counter in the
device's five-second report — 20 per five seconds where there should have been
160. Without that number the next move would have been profiling the C3, which
was working perfectly.

That is the third time in this spike the symptom pointed at the wrong layer, and
all three were settled the same way: by instrumenting the boundary rather than
the suspect. Frames against datagrams here, `0 bytes of program` for the dark
strip, and `--simulate` for the missing tail.

## What it cost

`dt` needed a register, so `R_SCRATCH` moved from 15 to 16 and the scratch file
went from 17 registers to 16.

| | registers |
|---|---|
| shipped corpus, worst (`08-air-quality`, `11-chase`) | 31 of 32 |
| the editor's sample fixture with a screen blend | 33 — **refused** |

Two layers, a mask, a palette, a state and a screen blend is not an extravagant
effect to be turned away. The fix that costs nothing is for a program to declare
`dt`'s register in its header, so only effects that read it pay; that is a
wire-format change and goes spec-first. It cannot be done by simply leaving the
register unreserved — a value computed in `once` can live there and survive into
later frames, where the frame section would overwrite it.

## `--simulate`

The sender renders on the host through the **same `lumen-device` renderer the
firmware runs**, and prints each frame as a ramp:

```
$ s5-sender effect.lfx --simulate --leds 30 --fps 30
  87 |    ......:::--==+**#%       | 13165 units
```

Deliberately not the desktop daemon's preview, which renders by its own route:
agreeing with it would prove only that two implementations agree, which was the
thing under suspicion. If the ramp is right and the strip is wrong, the fault is
below the renderer.

It is how "the tail is missing" was settled without touching hardware — the tail
was there, and short because `decay 0.9` is quoted per frame at 60 fps, which is
0.81 per frame at 30.

## An alert over a show, clearing itself

The device holds **two programs**, one per slot, and renders every admitted
source against the one it was pushed with. That is the smallest change that
makes the source stack mean anything on hardware: a device holding one program
can only show one thing, so priority, expiry and per-pixel resolution had never
been exercised outside the simulator.

Measured, with `breathe` in slot 0 at priority 100 and a red alert in slot 1 at
priority 230 with a six-second expiry:

```
program complete: 1716 bytes, 215 units/pixel   breathe
source pushed at priority 100
program complete: 1604 bytes,  76 units/pixel   alert
source pushed at priority 230
150 frames in 5 s, 8061 units/frame             both rendering
150 frames in 5 s, 6626 units/frame             alert expired, breathe alone
```

The frame cost is the evidence: 6626 is breathe by itself, 8061 is both. Nothing
sends a "stop" — the alert removes itself because its expiry passed, which is why
the expiry is not optional above the ambient floor.

One honesty note. The real binding from a source to a program is a scene record
naming one, and this spike has no records — so a source takes the slot of the
program that most recently finished arriving. That is the controller's contract
here, and it is the one place this device is not the real thing.

## `curl` turns the room red, and it clears itself

M6 asks for exactly that sentence. One request, no second one:

```
6626 units/frame                                 breathe alone
program complete: 1604 bytes, 76 units/pixel     the curl arrives
source pushed at priority 230
8906 units/frame                                 both rendering - red
6626 units/frame                                 expired; breathe alone again
```

The endpoint is sixty lines of `TcpListener` rather than an HTTP crate and an
async runtime, for a server answering three paths on a LAN. It is **not**
public-facing and must not become one: no authentication, no TLS, no size limit
beyond the read buffer. It trusts a home network, which is the boundary the rest
of this protocol already assumes — and if that boundary moves, this goes first.

The `seconds` parameter is clamped to 1–300 rather than honoured. The expiry is
the safety property that stops a room being red for ever, so a query string does
not get to ask for zero or for an afternoon.

## Two effects, one strip, a zone each

The device resolves three zones — the whole strip and each half — and renders
every source against the zone it named:

```
program complete: 1784 bytes, 435 units/pixel   comet   -> first half
program complete: 1716 bytes, 215 units/pixel   breathe -> second half
10041 units/frame
```

The arithmetic is the evidence: 435 x 15 + 215 x 15 is 9 750, plus both frame
sections. Each effect renders **fifteen pixels, not thirty**.

And the comet completes a lap across fifteen LEDs rather than being stretched
over the strip, because `u` runs 0..1 across *the zone a source targets*. That is
what makes an effect independent of the fixture it lands on — the property the
whole projection design exists for — and it had only ever run in the simulator.

Zones are defined locally here. In the real system a zone is a record that
arrives over the wire and is resolved on a mapping change, never per frame.

## Two things left out on purpose

**`ProgEnd` carries a zeroed hash and signature, and the device does not check
them.** Verification is `lumen-crypto` behind the `lumen-proto` seam; wiring it
in here would have been testing two things at once.

**The white die is never lit.** White is mixed from the colour dies, exactly as
every other device in the mesh mixes it. Lighting the dedicated white LED would
make this strip a different colour from its neighbours for the same program,
which is the one thing a mesh cannot have. It costs brightness and efficiency and
buys the property the whole project is built around.

## Running it

```bash
cd lumen-dev/spikes/s5-device
LUMEN_WIFI_SSID='...' LUMEN_WIFI_PASS='...' LUMEN_STAGE=device cargo build --release
espflash flash --port COM21 --monitor target/riscv32imc-unknown-none-elf/release/s5-device

cd sender && cargo run --release -- ../../../../lumen-effects/examples/10-comet/effect.lfx
```

`LUMEN_STAGE=strip` runs the stage-1 self-test instead and never touches the
radio: one pixel walking the strip, colour blocks, white, a gradient. It is how
the pin, the LED count, the colour order and the 32-bit RGBW format were
confirmed before anything harder was stacked on top, and it is the first thing to
re-run when the light looks wrong.

The device broadcasts a two-byte hello — "here I am", and whether it already
holds a program. The second byte is what makes this repeatable: a device that
reboots comes back with nothing, and a sender that had already sent its program
would otherwise never send it again. That is not hypothetical; it is how a
reflash mid-transfer left the strip dark with `0 bytes of program, 1 source` in
the log.
