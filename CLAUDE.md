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
  Blocked on hardware: the three spikes, W9 firmware, W12/W18 Android and AR.

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
scripts/clone-all.sh <git-remote-prefix>       # clone or fast-forward siblings
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

- **The default toolchain on this machine cannot link.**
  `stable-x86_64-pc-windows-msvc` is default but MSVC's `link.exe` is not
  installed. Use `cargo +stable-x86_64-pc-windows-gnu ...`.
- **Local coverage does not work on this machine.** The `windows-gnu` toolchain
  ships no profiler runtime, so `cargo llvm-cov` fails with "the compiler may have
  been built without the profiler runtime". Coverage runs in CI on Linux, which is
  the enforcing gate. Installing the VS Build Tools C++ workload would fix both
  this and the linker.
- **Git Bash shadows MSVC's linker** with `/usr/bin/link` even when MSVC is
  installed. Build from PowerShell if you switch toolchains.

## Where the design lives

| Topic | File | When to read |
|---|---|---|
| The whole system in one page, plus build order and open questions | `docs/overview.md` | Orienting, or deciding what to do next |
| Languages, crates, licence boundary, CI, conformance runner | `docs/tech-stack.md` | Structural or dependency questions |
| Scope, workstreams, dependency graph, milestones, risks | `docs/implementation-plan.md` | Planning, or picking up a workstream |
| Why the licence split is where it is; the four cross-cutting rules | `CONTRIBUTING.md` | Any design decision |

Per-repo notes live in that repo's `docs/` and its own `CLAUDE.md`.

## Nothing large should start before the spikes

Three assumptions carry the architecture, and all three need real hardware:

| # | Spike | Passes if |
|---|---|---|
| S1 | Time sync across 3 ESP32s on ordinary WiFi, 24 h | 95th percentile offset under ±500 µs, no drift |
| S2 | Bytecode interpreter in Rust, per-pixel, 300 LEDs | 60 fps with ≥1000 instructions/pixel headroom on an S3 |
| S3 | Multicast CHAN at 60 Hz to 10+ devices on a consumer AP | Loss under 1%, jitter under a frame |

A few hundred lines each, and far cheaper than discovering the problem later.

## Compact instructions

Preserve decisions, file paths touched, which repo each change landed in, and any
measured number. Drop raw build and test output.
