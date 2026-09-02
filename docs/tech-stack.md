Languages, frameworks and repository structure. Follows from four constraints already fixed elsewhere: the firmware core must be host-compilable ([[Firmware#Structured for simulation]]), the compiler must embed everywhere ([[Effect Language#Where the compiler lives]]), the preview must run the real VM ([[Desktop Application]]), and the licence boundary must be a real code boundary ([[-README#Licence split]]).

## The stack

| Layer | Choice |
|---|---|
| Shared core | **Rust**, `no_std` + `alloc` where possible |
| Firmware | **Rust on ESP-IDF** via `esp-idf-hal` / `esp-idf-svc` |
| Desktop editor | **Tauri** — Rust backend, web frontend |
| Headless daemon | same Rust core, no UI toolkit |
| CLI | Rust |
| Phone | **Kotlin + Jetpack Compose + ARCore**, core via `uniffi` |
| iOS (later) | Swift + ARKit, same core |

The unifying idea: **the daemon, the simulator, the desktop backend and the firmware are the same Rust library with different HAL implementations.** That is what makes the mixed real/virtual mesh in [[Desktop Application#Simulator]] nearly free, and what makes preview fidelity structural rather than a discipline you have to maintain.

## Sans-IO core

The core performs **no I/O at all**. It is a set of state machines that take events in and return actions out; the shell around them does the sockets, the timers and the flash writes.

```
fn on_event(&mut self, now: Instant, ev: Event) -> Vec<Action>
```

This is the cleanest way to satisfy "all nondeterminism is injected". There is no `rand()` to accidentally call and no socket to accidentally open, because the core cannot reach them — determinism is enforced by the type system rather than by code review. Deterministic replay then falls out: record the event stream, feed it back, get an identical run.

It also means the core tests without hardware, without a network, and without waiting for real time to pass. A 24-hour clock-drift scenario runs in milliseconds.

## Crates

```
lumen-proto     wire framing, codec, message types            Apache-2.0
lumen-vm        bytecode VM, pixel + sim profiles              Apache-2.0
lumen-lang      parser, resolver, partitioner, emitter         Apache-2.0
lumen-hal       traits only: Clock, Net, Storage, LedOut, ...  Apache-2.0
lumen-cli       compile / publish / backup over the protocol   Apache-2.0

lumen-device    mesh state machines: discovery, sync, election,
                replication, source stack, render loop         GPL-3.0
lumen-sim       HAL impl over simulated clock/net + replay     GPL-3.0

lumen-firmware  binary; HAL impl over esp-idf                  GPL-3.0
lumen-desktop   Tauri app                                      GPL-3.0
lumen-android   Kotlin app + uniffi bindings                   GPL-3.0
```

Crates are grouped into repos by licence — see [[#Repositories]].

### The licence boundary is not where it first looks

The instinct is "core is Apache, firmware is GPL". That is wrong, and it would give away the thing you meant to protect.

Ask what a **third-party controller** actually needs in order to talk to your devices: the wire codec, the compiler, and the VM (for preview). It does *not* need election, replication, the source stack or the render loop. Those are what make a **device** a device — so they belong in `lumen-device` under GPL, and the boundary sits between "how to talk to the mesh" (open to everyone) and "how to be part of the mesh" (share your changes).

Split the other way and someone assembles a closed commercial device out of your Apache-licensed mesh logic with a thin proprietary shell, which is precisely the outcome the GPL choice was meant to prevent.

Two consequences, both accepted deliberately:

- **The simulator is GPL**, because it links `lumen-device`. Fine — it is a development tool.
- **The desktop app is GPL**, because it joins the mesh as a virtual device holding `sim`/`keeper`/`gateway` ([[Desktop Application]]) and therefore links `lumen-device` too. Confirmed as wanted rather than tolerated: the desktop is a full participant in the mesh, so it sits on the device side of the boundary where it belongs.

`lumen-cli` stays Apache only if it restricts itself to compiling and publishing over the protocol, which is its actual job.

## Firmware specifics

`esp-idf-hal` puts Rust over Espressif's C SDK rather than replacing it, which buys mature WiFi, BLE, mDNS, NVS and OTA — the parts that are tedious and unglamorous to get right — while the logic above stays in the shared core.

| Concern | Approach |
|---|---|
| Toolchain | `espup` for Xtensa (ESP32-S3); upstream `riscv32imc/imac` targets for C3/C6 |
| Async vs threads | ESP-IDF gives `std` and FreeRTOS threads; the core is sans-IO so this is purely a shell decision. Threads are simpler here |
| Allocation | `lumen-vm` allocation-free; `lumen-lang` needs `alloc`, which is available under esp-idf and is what gates `caps=compile` |
| Zigbee | `esp-zigbee-sdk` is C with vendor binaries — reached by FFI from the bridge shell, never from the core |
| Build variants | Cargo features mapped from board definitions ([[Firmware#Board definitions]]) |

**Prefer RISC-V parts (C3, C6) where the choice is free.** They use upstream Rust with no forked toolchain, which removes a whole class of setup friction for contributors — and contributor friction is the tax an open-source project pays forever.

## Desktop specifics

Tauri gives the Rust backend the core, the simulator, the compiler and the protocol, with the web frontend doing the node editor, timeline and 3D viewport — where the library ecosystem is strongest. The known weak spot is webkitgtk on Linux; the Raspberry Pi case is served by the headless daemon rather than the editor, so it mostly does not bite.

The editor and the daemon are **two binaries over one library**, not one binary with a `--headless` flag, so the daemon never carries a UI toolkit.

## Phone specifics

Kotlin + ARCore directly, because the blink-code decoding needs **frame timestamps and locked exposure** through Camera2 — exactly the low-level access that plugin and wrapper layers obstruct. The hardest part of the app is the part that most needs native access, so wrapping it would be the wrong trade.

`uniffi` generates the Kotlin bindings to the Rust core, and generates Swift bindings later for the same core when iOS arrives. Only the AR module and the UI get rewritten; the protocol, compiler and VM do not.

## CI

| Job | Runs |
|---|---|
| Core tests | host, stable Rust — fast, no hardware |
| Scenario tests | `lumen-sim`, whole-mesh scenarios with fault injection and replay |
| Cookbook | every `.lfx` in `lumen-effects/examples` compiles and meets its `manifest.toml` budget; the prose note is regenerated and checked in |
| Negative cases | every effect in `examples/failing/` must be **rejected**, with the expected diagnostic ([[Effect Cookbook#Negative examples]]) |
| Conformance | recorded exchanges replayed against the implementation ([[-README#The open-source project itself]]) |
| Firmware build | all published board × feature variants |
| Hardware-in-loop | optional, a small rig, for timing claims the host cannot verify |

The first four need no hardware, which is what keeps the contribution story honest.

## Risks to verify early

- **Compiler RAM on device.** Whether `lumen-lang` fits in an ESP32-S3's budget decides whether `caps=compile` is real. Already listed as an open question; it is a Rust-specific measurement now.
- **Zigbee FFI.** Confirm `esp-zigbee-sdk` can be driven from Rust before promising Zigbee bridges — vendor SDK situations change, so check the current state rather than trusting any secondhand account of it.
- **VM throughput in Rust.** Spike S2 should be written in the real language; an interpreter dispatch loop is exactly where codegen differences show up.
- **`uniffi` overhead** on the phone's hot paths, mainly the AR mapping loop handing frames of observations to the core.

## Repositories

**Decided: split repos, one licence each.** Forks and pull requests stay focused — someone adding a board never clones the phone app — and the licence boundary stops being something anyone has to reason about, because each repo simply *is* Apache or GPL.

| Repo | Contents | Licence |
|---|---|---|
| `lumen-spec` | protocol spec, wire IDL, conformance vectors | Apache-2.0 / CC-BY |
| `lumen-core` | `lumen-proto`, `lumen-vm`, `lumen-lang`, `lumen-hal`, `lumen-cli` | Apache-2.0 |
| `lumen-device` | mesh state machines **and the simulator** | GPL-3.0 |
| `lumen-firmware` | ESP-IDF binary, HAL impl, board definitions | GPL-3.0 |
| `lumen-desktop` | Tauri editor and the headless daemon | GPL-3.0 |
| `lumen-android` | Kotlin app, `uniffi` bindings | GPL-3.0 |
| `lumen-effects` | stdlib, effect cookbook, shared effects | CC-BY |

The simulator lives in `lumen-device` rather than in the desktop app, so **that repo's CI can run whole-mesh scenario tests on its own** — the repo where election and replication bugs live is the one that most needs the harness.

Dependency direction, strictly acyclic:

```
lumen-spec ──► lumen-core ──► lumen-device ──┬─► lumen-firmware
                    │                        ├─► lumen-desktop
                    │                        └─► lumen-android
lumen-effects (stdlib, vendored by version) ─┘
```

### Keeping them coherent

The one real cost of splitting is that a protocol change now touches four repos. Three things contain it:

**Spec-first changes.** A protocol change lands in `lumen-spec` first — IDL plus new conformance vectors — then `lumen-core`, then dependents. The vectors make "has this repo caught up yet" a CI answer rather than a memory exercise.

**A dev meta-repo.** `lumen-dev` clones the siblings and provides a Cargo workspace with `[patch.crates-io]` path overrides, so working across repo boundaries is one checkout and one `cargo test`. Without this, split repos make cross-cutting work genuinely unpleasant; with it, the split is nearly free during development.

**Canary CI.** Each dependent repo tests against both its **pinned** `lumen-core` version and `lumen-core` **main**. The pinned build is what ships; the canary build is what tells you a core change broke the firmware before it is merged rather than three weeks later.

`lumen-core` publishes to crates.io on semver so external controllers can depend on it normally — which is the point of it being permissive.

Protocol version negotiation ([[Protocol#Versioning]]) is the runtime backstop: repos that fall out of step degrade visibly rather than failing mysteriously.

## The wire IDL

**The IDL is normative; generating code from it is optional.**

Write `lumen-spec`'s IDL and its conformance vectors now — they are cheap, and they are what lets a fourth implementation exist without reading prose. But `lumen-proto` stays **hand-written Rust**, with CI asserting it round-trips every vector.

That captures most of the value of generation for almost none of the cost: the vectors catch drift, which is the actual failure mode, while hand-written code stays readable and debuggable and needs no generator to maintain. If a second or third implementation appears and the codecs start disagreeing, generation becomes worth building — and the IDL will already be there, so nothing is wasted by waiting.

## Stdlib vendoring

**Decided: stdlib versions are vendored into `lumen-core` by pinned tag**, not fetched at build time. Each version lands as source under `stdlib/v1/`, `stdlib/v2/`, … with a checksum manifest, updated by a script that pulls from a `lumen-effects` tag.

Builds stay hermetic and offline, which an embedded toolchain needs. The less obvious benefit: **compilation becomes deterministic**. The same effect source plus the same stdlib version plus the same compiler produces byte-identical bytecode — which is what the "skip the upload if the source hash matches" optimisation in [[Desktop Application#Compiler and publishing]] silently depends on, and what makes a signed program reproducible by someone auditing it.

The cost is that a stdlib release needs a `lumen-core` release to reach users. Given stdlib versions are additive and old ones never disappear ([[Effect Language#Standard library]]), that is a slow-moving dependency and an acceptable trade.

## The conformance runner

**Decided: `lumen-spec` ships one shared runner.** Making every implementation write its own would defeat the point — divergent runners produce divergent notions of "passing", which is the exact failure the suite exists to prevent.

The runner is a binary plus a set of data-file vectors. It drives an **implementation adapter** over a trivial line protocol on stdin/stdout, so an adapter can be written in any language in an afternoon, and adding an implementation never means touching the runner.

Two vector classes:

| Class | What it checks | How |
|---|---|---|
| **Codec** | framing and message encode/decode | feed bytes, compare parsed structure; feed structure, compare bytes |
| **Behavioural** | sync, election, replication, source-stack resolution, arbitration | feed an event sequence, compare the emitted actions |

The behavioural class is the valuable half, and **[[#Sans-IO core]] is what makes it possible at all**. Because the core is `on_event(now, ev) -> Vec<Action>` with no I/O, conformance is literally "given these events, did you emit these actions" — no sockets, no timing, no flakiness, and a hostile scenario like a three-way split brain is just a longer vector file. An implementation that hid its state machine behind real I/O could only be tested end to end, which is precisely how distributed-systems test suites become slow and unreliable.

This also makes the runner the natural home for regression cases: a bug reproduced in the simulator exports as a vector ([[Desktop Application#Simulator]]), and every implementation inherits the test.

## Open questions

None outstanding for the stack itself. Remaining risks are the measurements listed above — compiler RAM on device, Zigbee FFI, VM throughput, `uniffi` overhead — none of which are decidable on paper.
