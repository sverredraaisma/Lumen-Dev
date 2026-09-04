# lumen-dev

The meta-repo for the Lumen ARGB mesh project. One checkout, one `cargo test`,
across repo boundaries. **Start here** if you need the design.

- **Licence:** Apache-2.0
- **Main branch:** `main`
- **Status:** M1 complete. Implemented: W2 codec, W3 VM, W4 compiler, W10 CLI and
  the W14 crypto seam in `lumen-core`; W5 sync and election, W6 source stack,
  zones, projections and render loop, W7 records and replication, W13 channels
  and W16 gateway policy in `lumen-device`; W8 simulator; W11 preview daemon;
  W15 stdlib and example corpus; the codec conformance suite in `lumen-spec`.
  **The chain runs end to end on real hardware.** A phone compiles an effect,
  finds a node, transfers the program, claims a channel and drives it from a
  slider; an ESP32-C3 renders it through the real VM at 30 fps and drives 30
  SK6812 RGBW LEDs over SPI/DMA, with power derating against its supply. Spikes
  S1-S5 are all measured. What W9 and W12 still owe is the shape around that:
  provisioning, pairing, NVS, OTA, multiple devices, zones. W18 (AR) is
  untouched beyond the decoder.

## Layout — siblings, not submodules

```
LED_control_system/
  lumen-dev/        <- this repo
  lumen-spec/       Apache + CC-BY   protocol, IDL, conformance suite
  lumen-core/       Apache-2.0       codec, VM, compiler, HAL traits, CLI
  lumen-device/     GPL-3.0          mesh state machines + simulator
  lumen-firmware/   GPL-3.0          ESP-IDF binary, board definitions
  lumen-desktop/    GPL-3.0          Tauri editor + headless daemon
  lumen-android/    GPL-3.0          Kotlin app, uniffi bindings
  lumen-effects/    CC-BY            stdlib, cookbook, shared effects
```

Each is an **independent git repo with its own remote**. Never commit across
them in one command; a change that spans repos is one commit per repo, and the
spec repo goes first.

## Commands

```bash
cargo test                                     # canary: builds all siblings together
scripts/foreach.sh git status --short          # run anything in every repo
scripts/foreach.sh cargo test --workspace      # test everything
scripts/clone-all.sh                           # clone or fast-forward siblings
```

## Dependency direction — strictly acyclic

```
lumen-spec ──► lumen-core ──► lumen-device ──┬─► lumen-firmware
                    │                        ├─► lumen-desktop
                    │                        └─► lumen-android
lumen-effects (stdlib, vendored by version) ─┘
```

`crates/canary` depends on every sibling by path. Its only job is to fail when a
core change breaks a dependent — in one command here, rather than three weeks
later in a dependent repo.

## Hard rules

- **Protocol changes are spec-first.** `lumen-spec` (IDL + conformance vectors),
  then `lumen-core`, then dependents. The vectors turn "has that repo caught up"
  into a CI answer instead of a memory exercise.
- **Coverage floor is 95%** in every repo that has tests.
- **Respect the licence boundary.** Apache = how to talk to the mesh. GPL = how to
  be part of the mesh. Moving election, replication, the source stack or the
  render loop into `lumen-core` defeats the whole arrangement. Reasoning in
  `CONTRIBUTING.md`.
- **Do not add a dependency that reverses an arrow** in the graph above.

## Gotchas

> Living section. Add anything that cost real time.

- **The "cannot link / no local coverage" note used to be wrong; both now work.**
  `link.exe` was never missing. What was missing was the **Windows SDK**, so the
  linker had no `kernel32.lib` to link against and Rust reported that as
  "linker `link.exe` not found". Adding the SDK component to the existing VS
  2022 install fixed the MSVC toolchain and `cargo llvm-cov` together. If a
  fresh machine shows this symptom, install the C++ workload rather than
  switching to `windows-gnu`: that workaround builds, which is why nobody
  revisits it, and it silently costs you coverage because the `windows-gnu`
  toolchain ships no profiler runtime.

## Where the design lives

| Topic | File | When to read |
|---|---|---|
| The whole system in one page, plus build order and open questions | `docs/overview.md` | Orienting, or deciding what to do next |
| Languages, crates, licence boundary, CI, conformance runner | `docs/tech-stack.md` | Structural or dependency questions |
| Scope, workstreams, dependency graph, milestones, risks | `docs/implementation-plan.md` | Planning, or picking up a workstream |
| Why the licence split is where it is; the four cross-cutting rules | `CONTRIBUTING.md` | Any design decision |

Per-repo notes live in that repo's `docs/` and its own `CLAUDE.md`.

## M2, most of the way

`spikes/s5-device/MESH.md`. A C3 and a desktop peer running the *same*
`lumen_device::node::Node` elect a leader by capacity, the follower disciplines
its clock to sub-millisecond corrections, and killing the leader produces
`show clock lost` then `now Leader in epoch 2`. Election, discovery and failover
are demonstrated on real hardware.

**"Change colour on the same frame" is not**, and that is what M2 still owes.
Both nodes agree on show time and `lumen-vm::digest` proves a host and a device
render identical frames for the same show time - but nothing yet renders one
effect on two nodes at once and compares. Two devices with LEDs settle it by eye.

It found three bugs a lossless simulated network cannot: **one lost datagram
deadlocked time sync for ever** (the outstanding probe was cleared only by a
matching answer), the **slew rate was silently zero** to integer division, and a
join offset was being slewed at 200 ppm rather than stepped - a day's work for a
clock reporting itself synced throughout.

## What runs on hardware today

`spikes/s5-device` is a whole node - WiFi, the protocol, the source stack, the
render loop, the output stage, an LED driver - and `lumen-android` is a phone
that drives it. Between them they exercise nearly every part of the design
against real silicon, which is why the last few days have found more bugs than
the previous months of host tests: **each one was only wrong across frames, on a
device, over a network.**

They are still a spike and an app, not W9 and W12. Missing: provisioning and
pairing, NVS, OTA, more than one device, zones that are not "the whole strip",
and any of the reliability the plan asks for. What they prove is that the
architecture works, which is what a spike is for.

## Nothing large should start before the spikes

Three assumptions carry the architecture, and all three need real hardware:

| # | Spike | Passes if | Result |
|---|---|---|---|
| S1 | Time sync across 3 ESP32s on ordinary WiFi, 24 h | 95th percentile offset under ±500 µs, no drift | **fails the number, meets the requirement** |
| S2 | Bytecode interpreter in Rust, per-pixel, 300 LEDs | 60 fps with ≥1000 instructions/pixel headroom on an S3 | **conditional pass, now on the S3 too** |
| S3 | Multicast CHAN at 60 Hz to 10+ devices on a consumer AP | Loss under 1%, jitter under a frame | **multicast fails, unicast passes on loss** |
| S4 | Splitting the pixel loop across an S3's two cores | Faster, and byte-identical to one core | **passes: 2.1×, identical** |
| S5 | A whole device: WiFi, the protocol, the render loop, a real strip | A program written on a desktop lights real LEDs | **passes: 30 fps on a C3** |
| M2 | Two nodes elect a timebase, and the survivor takes over | Election, sync, failover | **election and failover pass; "same frame" not shown** |
| M6 | `curl` turns the room red for 30 s and it clears itself | One request, no second one | **passes** (the audio half needs capture hardware) |

**S2 ran on a C3 and passed on the thing that mattered, not on its own
criterion.** Every corpus effect renders 300 pixels inside a 60 fps frame, the
worst at 86% — but the criterion asked for ≥1000 instructions/pixel of headroom
and a C3 has about 60. That number was written before anything was measured. The
comfortable envelope is 300 LEDs at 30 fps or 150 at 60.

**S3 ran with one sender and two receivers.** Multicast loses **4-6%** against a
1% criterion - an unacknowledged frame sent once at a low basic rate is simply
gone if missed - while **unicast lost nothing at all** over three consecutive
windows. Both miss the jitter criterion: arrivals sit at the send interval at the
median but p95 is around 26 ms, one and a half frames. So the channel design
needs the unicast fallback the plan prepared, and receivers must render on the
show clock rather than on arrival.

Four of five runs produced a wrong answer first, each for a different reason, and
`spikes/s3-multicast/RESULTS.md` records them because each looked like a result:
the sender dropping its own packets, a cumulative average that never recovers
from a bad start, and a sleeping receiver that is indistinguishable from a lossy
network until you notice its twin is fine.

**S1 ran on two boards over a domestic AP.** It misses ±500 µs at p95 — the best
the specified algorithm managed was p50 225 µs, p95 675 µs — but that is 4% of a
16.7 ms frame, and "does not visibly tear" is the requirement the ±500 µs was
standing in for. Two changes came out of it, both now in the spec: **power save
must be off** (worth 4× on its own) and **the burst is 32 samples, not 8** (worth
better than 2×). Longer is not monotonically better — at 128 the crystal's
33 µs/s drift accumulates inside the window faster than the noise averages out.
Full findings in `spikes/s1-time-sync/RESULTS.md`.

**S4 answers what a second core is worth**, which S2 raised by finding the
speed-up from a bigger ESP32 to be entirely its clock. Splitting the *pixels*
rather than offloading comms is where the headroom is, and it came out slightly
above 2x - each core's per-LED history map is half the size, so the second core
makes its own half cheaper too. What it does not answer is contention with a live
radio, which is the other thing a second core is for and what S3's jitter points
at. Full findings in `spikes/s4-dual-core/RESULTS.md`.

**S2 has since run on the actual S3**, the chip its criterion named. It is 1.4x
faster than the C3 - close to the 1.5x its clock ratio predicts - and the worst
corpus effect drops from 86% of a 60 fps frame to 60%. The speedup is *entirely*
the clock: 583 ns per instruction against 838, which at their respective clocks
is 140 cycles against 134. A faster ESP32 buys its clock and nothing else, which
is worth knowing before reaching for a bigger chip to buy headroom.

Two things came out of S2 that changed the code. The interpreter is
**dispatch-bound** — 837 ns of every instruction is dispatch, 134 cycles, about
80% of an average one. And the cost model was wrong: it mis-ranked real effects
by up to 3.8×, so `OpCode::cost()` has been rewritten from measurement and one
budget unit is now **100 ns on a C3**. Full findings in
`spikes/s2-vm-throughput/RESULTS.md`.

A few hundred lines each, and far cheaper than discovering the problem later.

## Supporting cheaper hardware

`docs/esp8266.md` costs out the ESP8266 and ESP-01. The short version: **there
is no Rust WiFi for the ESP8266 and no path to one that is not months of work**,
because its radio blobs come from the NONOS SDK and predate the adapter
interface `esp-wifi` is built on. The cheaper and better route is to compile the
portable core - `lumen-vm`, `lumen-proto`, `lumen-hal`, all `no_std` and
allocator-free - as a static library and let C own the radio. **Built:**
`lumen-core/nodes/esp8266/` produces `liblumen_esp8266.a`, 23.6 KB of Lumen
code, eleven C entry points, with `nodes/esp8266/README.md` carrying a worked
Arduino integration.

## Using the second core

`docs/multicore.md` answers whether to hand tasks to the second core on the
dual-core chips. The short version: yes, but the win is **not** offloading
comms - it is splitting the *pixel loop*, which is embarrassingly parallel and
is the part S2 found short. It belongs in the firmware shell, never in the
sans-IO core.

**Built and measured.** `lumen_device::render::Shard` is the seam, a board
declares `render_cores`, and S4 ran it on the S3: **2.08-2.20x over the whole
shipped corpus, byte-identical to a single core**. The worst effect drops from
103% of a 60 fps frame to 49%. The identity is the half that decides whether it
ships - a two-core device rendering a different show would break the mesh's
agreement with itself - so it is checked in `lumen-capi`, in `lumen-device`, and
pixel by pixel on hardware in S4.

S4 also found two things worth more than the split, both now fixed: the render
loop was charging the `frame` section the header's **per-pixel** budget, which
faulted `07-alert` every frame so it rendered nothing; and it looked an LED up
with a linear scan **per pixel**, quadratic in the strip and worth 20-25% of
every frame. Both were free in every host test, because every host test is four
LEDs long.

## The first light

**S5 is the first thing in this project anybody can look at.** Every number
before it - VM throughput, sync offsets, multicast loss, the dual-core split -
was measured with the output thrown away. `spikes/s5-device/` is the whole chain
with nothing stubbed: an effect compiled on a desktop, sent as
`ProgBegin`/`ProgChunk`/`ProgEnd`, admitted to the source stack, rendered by the
real `Renderer` through the real VM, and driven out of RMT to 30 SK6812 RGBW
LEDs. It runs at 30 fps on a C3.

It found five bugs, three of them in shipping code, and the first one is the
lesson: **`dt` compiled to the same register as `t`**, so it was the absolute
show time. Nothing failed - `pow(decay, dt * 60)` saturated, trails never
decayed, and the strip filled with stuck pixels. 250 host tests said nothing,
because the wrongness only shows across frames on a device. The others: the
frame budget was reported for one pixel rather than the whole strip; WiFi
interrupts corrupted every RMT frame; and show time both saturated Q16 and lost
its sub-second part to a `u32` overflow.

Two things it surfaced and did **not** fix, both recorded in
`spikes/s5-device/RESULTS.md`: there is **no output stage** anywhere in the
project, so linear values go straight to the LEDs and the dark end of every fade
collapses; and holding interrupts off across an RMT frame is a stopgap that does
not scale past about 30 LEDs - DMA is the follow-up.

`sender --simulate` renders on the host through the *same* `lumen-device`
renderer the firmware runs. Reach for it before reaching for hardware: if the
ramp is right and the strip is wrong, the fault is below the renderer.

## Compact instructions

Preserve decisions, file paths touched, which repo each change landed in, and any
measured number. Drop raw build and test output.
