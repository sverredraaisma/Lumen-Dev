# Contributing

## Where to start

**Run it with no hardware.** Clone, build, run the simulator, and watch lights
move in a 3D view. The firmware core is host-compilable with deterministic
replay, so this costs you nothing but a checkout.

**Then add your board.** `lumen-firmware/boards/` takes one TOML file per board.
It is a genuinely useful contribution that requires no understanding of the
internals, and it is the intended first PR.

## Why the licence split is where it is

Contributors ask this, and the answer is short: **the boundary is where the
device ends.**

| Component | Licence |
|---|---|
| `lumen-spec` — protocol, wire IDL, conformance suite | Apache-2.0 / CC-BY |
| `lumen-core` — codec, VM, compiler, HAL traits, CLI | Apache-2.0 |
| `lumen-device`, `lumen-firmware`, `lumen-desktop`, `lumen-android` | GPL-3.0 |
| `lumen-effects` — stdlib and shared effects | CC-BY (shared `.lfx` files: author's choice) |

The instinct is "core permissive, firmware GPL". That is wrong, and it would
give away the thing the GPL choice was meant to protect.

Ask what a third-party controller actually needs in order to talk to your
devices: the wire codec, the compiler, and the VM for preview. It does **not**
need election, replication, the source stack or the render loop. Those are what
make a device a device, so they are GPL. Split the other way and someone
assembles a closed commercial device out of permissively licensed mesh logic
with a thin proprietary shell.

Two consequences, both accepted deliberately:

- **The simulator is GPL**, because it links `lumen-device`. Fine — it is a
  development tool.
- **The desktop app is GPL**, because it joins the mesh as a virtual device
  holding `sim`/`keeper`/`gateway`. Wanted rather than tolerated: it is a full
  participant, so it belongs on the device side.

`lumen-cli` stays Apache only while it restricts itself to compiling and
publishing *over the protocol*, which is its actual job.

## Rules that a change is checked against

Four rules do most of the design work here. When in doubt, check your change
against them.

1. **Priority and timeout on every source.** Programs, streams, manual control
   and status overrides all declare a priority and an expiry. Nothing can
   permanently capture a pixel, and a dead publisher releases its claim
   automatically. A source above the ambient floor with no expiry is a bug and
   the tooling should refuse it — that is how a room ends up stuck red at 3am.
2. **Scheduled activation, never "go now".** Changes take effect at a named
   future show time, so the mesh switches together even across a network
   hiccup.
3. **Mapping is a pure upgrade.** Every device always has coordinates —
   synthetic, rough or mapped — so no feature needs an unmapped code path.
4. **Defined degradation.** Every failure has a specified visual outcome. Stale
   channels decay to a default, a corrupt program falls back, a lost network
   keeps rendering. A device should never be dark because of software.

## Two habits worth more than they look

**Keep the core sans-IO.** State machines are `on_event(now, ev) -> Vec<Action>`
and perform no I/O. There is no `rand()` to accidentally call and no socket to
accidentally open, so determinism is enforced by the type system rather than by
code review. Everything else — replay, hardware-free tests, behavioural
conformance — falls out of it. It is a cheap constraint on new code and an
expensive refactor of existing code.

**Write the conformance vector when you write the behaviour**, not later.
Retrofitting a suite across seven repos is miserable. A bug you reproduce in the
simulator should leave a vector in `lumen-spec` behind, so every implementation
inherits the regression test.

## Cross-repo changes

Protocol changes are **spec-first**: `lumen-spec` (IDL + vectors), then
`lumen-core`, then dependents. Work across boundaries from a `lumen-dev`
checkout — see this repo's README.
