# Spike S2 — VM throughput

Run on an **ESP32-C3 at 160 MHz**, 4 MB flash, `riscv32imc-unknown-none-elf`,
esp-hal 0.23, release profile. Five effects from the shipped corpus, compiled by
the real compiler and run by the real interpreter over 300 pixels, 30 frames
averaged.

The plan names an S3 for this spike. A C3 is the harder target — one core at
160 MHz against the S3's two at 240 — so a pass here errs conservative, which is
the direction a spike should err.

## Verdict: conditional pass

**300 pixels at 60 fps works on a C3.** Every corpus effect rendered inside the
16 667 µs frame, the worst at 86% of it.

| effect | budget | µs/frame | µs/pixel | % of frame | max fps |
|---|---|---|---|---|---|
| 07-alert | 136 | 4 496 | 14.98 | 26% | 222 |
| 01-breathe | 215 | 6 877 | 22.92 | 41% | 145 |
| 12-panel-plasma | 388 | 12 104 | 40.34 | 72% | 82 |
| 05-beat-strobe | 462 | 14 467 | 48.22 | 86% | 69 |
| 03-drift | 562 | 9 492 | 31.64 | 56% | 105 |

**The spike's stated criterion is not met.** It asks for "60 fps with ≥1000
instructions/pixel headroom". At 300 pixels and 60 fps a C3 has about **60
instructions per pixel**, not 1000 — short by a factor of 17. That criterion was
written before anything had been measured and assumed an interpreter roughly an
order of magnitude faster than the one that exists.

So the architecture holds and the number behind it was wrong. What the corpus
needs is 12 to 41 instructions per pixel, and it fits. But rendering cannot have
the whole frame — a device also receives channel traffic, runs sync, and clocks
data to the strip — and at 86% of frame the most expensive corpus effect leaves
too little for the mesh. **The comfortable envelope for a C3 is 300 LEDs at
30 fps, or 150 at 60.** 300 at 60 is available to simple effects and to a device
doing nothing else.

## The interpreter is dispatch-bound

Fitted across programs of 16, 64 and 256 `NOP`s, so per-call cost and
per-instruction cost are separated rather than conflated:

```
 16 nops:   15 388 ns
 64 nops:   55 594 ns
256 nops:  216 385 ns
--> 837.5 ns per instruction (134 cycles), 1 996 ns per run_pixel call
```

**Dispatch is about 80% of an average instruction.** A `NOISE3` — the most
elaborate thing in the corpus — costs only 3.5 times a `NOP`. The interpreter
spends most of its time deciding what to do next, not computing.

That reframes the one open question the VM document had left on execution
strategy. Threaded code attacks dispatch specifically, and dispatch is where the
time is. The question is no longer whether it would help but whether 300 LEDs at
60 fps is worth a second execution path and the conformance burden of keeping two
of them bit-identical.

## The cost model was wrong, and is now measured

The old `OpCode::cost()` weights were guesses, and they mis-ranked real effects
by up to **3.8×** — `05-beat-strobe`, priced at 57 units, ran slower than
`03-drift`, priced at 141. Since the budget is what the compiler uses to promise
a frame rate, that made the promise decorative.

Each of 44 opcodes was then timed directly: one instruction, 64 times per
program, 2 000 runs, with the fitted dispatch cost subtracted. The table was
rewritten from the results and **one budget unit is now defined as 100 ns on this
chip**, which makes a device's capacity computable from its clock instead of
discovered by benchmark.

The guesses were wrong in both directions:

| opcode | was | measured | note |
|---|---|---|---|
| `LEN2` / `LEN3` | 8, 9 | **60** | the iterative fixed-point square root; the most underpriced instruction in the set |
| `SQRT` | 8 | **57** | same cause |
| `RGB2HSV` | 12 | 38 | |
| `PALETTE` | 12 | 33 | |
| `POW` | 24 | 25 | the one guess that was close |
| `SMOOTHSTEP` | 9 | 25 | priced at a third of `POW`, actually equal to it |
| `NOISE3` | 28 | 29 | |
| `DOT3` | 9 | 17 | |

An effect built from distance fields was being promised a frame rate it could not
hold, while one built from `POW` was refused a budget it did not need.

### The recalibrated model predicts the chip

| effect | predicted µs/pixel | measured | |
|---|---|---|---|
| 07-alert | 15.6 | 14.98 | 96% |
| 01-breathe | 23.5 | 22.92 | 98% |
| 12-panel-plasma | 40.8 | 40.34 | 99% |
| 05-beat-strobe | 48.2 | 48.22 | 100% |
| 03-drift | 58.2 | 31.64 | 54% — see below |

Prediction is `(budget + 20) / 10`, the 20 being the per-pixel call overhead. The
old model's equivalent spread was 3.9×; this is 4%.

`03-drift` is the only corpus effect carrying a `MASKTEST`, and the only one that
reads well under budget. That is correct, not an error: a mask skips the rest of
a layer, so a static sum is a **worst case**, and a device must promise against
the pixel that runs every layer rather than the average one.

## What this changed outside the spike

- `OpCode::cost()` in `lumen-core` — every weight, plus the unit's definition.
- The **Budgets** section of `lumen-core/docs/bytecode-vm.md`, whose table
  claimed ~900 instructions/pixel for a C3 and was optimistic by about sevenfold.
- Four behavioural conformance vectors, whose hand-written programs declared
  budgets in the old units.
- Four compiler tests that had been using budget numbers as a proxy for
  structural claims ("the sin was hoisted", "the argument was evaluated once")
  against thresholds read off the old table. They now assert the structure.

## A hazard it exposed

The render loop uses a program's **own declared budget** as the interpreter's
fuel limit. So a program compiled under one weight table and run under another
faults through no fault of its own — which is exactly what those four vectors did
the moment the table moved. Recorded, with the design question it raises, under
*Version compatibility* in `lumen-core/docs/bytecode-vm.md`.

## Reproducing

```bash
cd lumen-dev/spikes/s2-vm-throughput
cargo build --release
espflash flash --port COM13 --monitor \
  ../../target/spike/riscv32imc-unknown-none-elf/release/s2-vm-throughput
```

The `opcost/` programs are generated by `../s2-vm-throughput-gen` — one per
opcode, plus the three `NOP` lengths for the dispatch fit. `programs/` holds the
five corpus effects, compiled with `lumen compile`.

**Both need regenerating whenever the cost weights change**, or the spike's
budget column and the manifest's claimed weights go stale against the table they
are meant to be checking:

```bash
cd ../s2-vm-throughput-gen
cargo run --bin opcost   -- ../s2-vm-throughput/opcost
cargo run --bin dispatch -- ../s2-vm-throughput/opcost
cd ../../../lumen-core
for e in 07-alert 01-breathe 05-beat-strobe 12-panel-plasma 03-drift; do
  cargo run -p lumen-cli -- compile "../lumen-effects/examples/$e/effect.lfx" -o "../lumen-dev/spikes/s2-vm-throughput/programs/$e.lfxb"
done
```
