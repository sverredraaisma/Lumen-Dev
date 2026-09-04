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

Holding interrupts off across the frame fixes it completely. That is a stopgap,
not an answer: 30 LEDs is 1.2 ms, so 3.6% of the time at 30 fps, but 300 LEDs
would be 12 ms — most of a frame. **DMA is the first follow-up.**

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
