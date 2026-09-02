An ARGB control system where lightshows, ambient lighting, audio-reactive lighting and status lighting are all easy to set up, and where the system is easy to extend without touching firmware.

## Components

- [[Protocol]]: how the apps and devices talk. The actual product — everything else is an implementation of it.
- [[Wire Format]]: the normative byte layouts, state machines and conformance vectors
- [[Firmware]]: flashed to every device, makes it a first-class mesh participant
- [[Bytecode VM]]: the execution target compiled effects are shipped as
- [[Effects]]: node graph + timeline authoring model, and how it compiles
- [[Effect Language]]: the canonical text format an effect is written and shared in
- [[Effect Language Grammar]]: EBNF, type system and the standard library listing
- [[Effect Cookbook]]: worked examples for all four use cases, doubling as the compiler's test corpus
- [[Runtime Model]]: what is actually rendering right now — the source stack, zones, projections, status lighting, provisioning
- [[Data Model]]: what is stored, and how it replicates with no central server
- [[App]]: phone app — AR mapping, setup, and remote control
- [[Desktop Application]]: authoring environment, integrations, diagnostics
- [[Tech Stack]]: languages, frameworks, crate structure and where the licence boundary actually falls
- [[Implementation Plan]]: 1.0.0 scope, workstreams, dependency graph, milestones and what parallelises

## The core idea in one paragraph

A **mesh** is one place — a house, a workshop, a garden. It owns its own trust, state, coordinate origin and clock, and meshes federate for coarse cross-place cues rather than pretending to be one system.

A **device** is anything that joins a mesh — a light, a microphone, a sensor, a button panel, or a machine that just does maths for the others. Lights are the common case, not the assumption.

Every LED knows where it is in the room. Effects are pure functions of position and time, compiled to portable bytecode and shipped to each device, which renders its own pixels against a mesh-wide clock synced to under a millisecond. Nothing that depends on LED count crosses the network. The things that *cannot* be a pure function — audio, physics simulations, sensors, external state — become small broadcast channels the running program reads as uniforms. Everything else falls out of that: shows are autonomous because devices hold their own programs, status lighting is safe because sources are prioritised and self-expiring, and expandability is real because a user's effect and a shipped effect are the same kind of object.

## Decisions made

| Decision | Choice | Consequence |
|---|---|---|
| Where effects run | On-device, synced clock | Bandwidth independent of LED count; global effects need channels |
| Effect representation | Custom bytecode VM, Q16.16 | Portable across chips, safe, budget-checkable at compile time |
| Stateful effects | Local history buffer **and** a broadcast sim channel | Trails and fire are free; cross-device motion costs one packet per frame |
| Autonomy | Fully autonomous, devices own the show | Needs replicated state and a keeper role |
| Security | Pairing + signed programs | Programs are signed Ed25519, traffic is AEAD under a mesh key |
| Authoring | Node graph with a timeline on top | Timeline automates parameters, it does not render |
| Hardware | ESP32 family + bridged non-WiFi nodes | One VM dialect runs on all of them |
| Interop | Art-Net / E1.31 / DDP / HA / MQTT, plus BLE and Zigbee via bridges | Firmware needs a DIRECT path alongside program rendering |
| Effect format | Text is canonical, editor is a view over it | Effects diff, version and share as single files |
| Scale target | ~50 devices, 10k+ LEDs | Keeper set capped at 5–7; multicast health must be measured |
| Phone platform | Android first, iOS later | ARCore, with mapping written against the ARCore/ARKit common subset |
| Project shape | Open source, community | No vendor CA — trust is user-rooted; needs a conformance suite |
| Compiler | One portable core library, several front-ends | Rust/C, `no_std`-friendly, embeddable on device as `caps=compile` |
| Zones | Explicit sets **and** geometric predicates, combinable | Zones survive rewiring; needs a projection to keep 1D effects usable |
| Runtime | A stack of expiring sources per zone | One mechanism for shows, schedules, alerts, manual and streams |
| Provisioning | QR to start, BLE preferred, SoftAP fallback | Identification by blink is part of the flow, not an afterthought |
| Device shapes | Any topology, and LEDs are **optional** | `render` is one capability among many; audio, sensor and control-surface nodes are first-class |
| Live control | Cue lists **and** live parameters | A timeline is a cue list where every cue auto-follows — one editor, one model |
| Effect sharing | Files only for now | Self-contained files keep every future distribution model open |
| Zone evaluation | On-device predicate evaluation | Zones self-update when devices move; apps must be able to query what a zone resolves to |
| Colour | Per-device calibration matrix | Matrix not gains, so camera-assisted and measured calibration drop in later unchanged |
| Firmware config | Runtime for pins and LEDs, build variants for features | Board definitions as contributable files; prebuilt matrix so most users never compile |
| Testing | Full mesh simulator with deterministic replay | Firmware core must be host-compilable with all nondeterminism injected |
| Clocks | Monotonic show clock **and** a separate wall clock | Render clock never steps; wall time is optional and schedules degrade explicitly |
| Bridged nodes | Thin or full, negotiated by capability | Zigbee bulbs get pixels, RP2040s get programs; bridge declares a thin-node budget |
| Standard library | Small frozen instruction core + versioned source library | Effects written today still compile in two years; firmware and stdlib versions independent |
| Colour authoring | OkLab interpolation by default, other spaces available | Easy path is the good-looking one; costs nothing at runtime |
| Mapping | Pure upgrade — synthetic, rough, or mapped coordinates | No feature is gated on a 50-device mapping session |
| Licence | GPL firmware, permissive protocol and client libraries | Devices stay open; anyone can write an implementation |
| Concurrency | Dynamic, admission-controlled, floor of 2+1 | Compiler reports worst-case concurrency per device, not just per-effect budget |
| Binding evaluation | Every keeper evaluates; actions are idempotent | Events carry a producer-minted id; gateways dedup outbound calls |
| Record integrity | Every record signed — controller key, or device key for its own record | A compromised device can lie about itself and nothing else |
| Sim execution | Second VM profile with bounded arrays and loops | Users can write simulations in the effect language; sims must be deterministic |
| Election input | Static capacity score only, never current load | Prevents role flapping; `load` is advisory and used for budgets and UI |
| Frame timing | 120 Hz grid, rates are integer divisions | Mixed frame rates stay in phase instead of drifting |
| Channel ownership | Claim-and-lease, priority breaks ties, manual pin available | Desktop audio preempts the room mic and hands back on disconnect |
| Scope | Separate meshes per place, federated when linked | Each mesh owns its trust, state, origin and clock; cross-mesh sync is coarse |
| Debugging | Node inspection, on-device probes, time control, compile warnings | `PROBE` instruction and probe builds; hardware time control is leased and explicit |
| Language | Rust everywhere except the phone UI ([[Tech Stack]]) | One VM and one compiler shared by firmware, simulator, desktop and phone |
| Core architecture | Sans-IO state machines | Determinism enforced by the type system, not by code review |
| Firmware base | Rust on ESP-IDF (`esp-idf-hal`) | Mature WiFi/BLE/OTA underneath; Zigbee via FFI; prefer RISC-V parts |
| Desktop | Tauri — Rust backend, web frontend | Editor and daemon are two binaries over one library |
| Phone | Kotlin + Compose + ARCore, `uniffi` to the core | Camera2 access is required by the blink-code decoding |
| Repositories | Split, one licence per repo, plus a `lumen-dev` meta-repo | Focused forks and PRs; spec-first changes and canary CI keep them coherent |
| Wire IDL | Normative spec + conformance vectors; codegen optional | `lumen-proto` stays hand-written, CI asserts it round-trips every vector |
| Conformance | One shared runner in `lumen-spec`, adapters per implementation | Sans-IO makes behavioural conformance testable as events-in / actions-out |
| Stdlib | Vendored into `lumen-core` by pinned tag | Hermetic offline builds, and byte-identical bytecode from identical source |

### Licence split

| Component | Licence | Reasoning |
|---|---|---|
| Firmware **and the mesh state machines** (`lumen-device`, `lumen-firmware`) | GPLv3 | someone selling a device running this must publish their changes |
| [[Protocol]] spec, wire IDL, conformance suite | permissive (Apache 2.0 or CC-BY) | a spec nobody may freely implement is not a standard |
| Wire codec, VM, [[Effect Language]] compiler (`lumen-proto`, `lumen-vm`, `lumen-lang`) | Apache 2.0 | embedded in phone, desktop and devices; permissive is what lets anyone build a controller |
| Shared effects (`.lfx` files) | author's choice, suggest CC-BY | user content, not project code |

The boundary is **"how to talk to the mesh" (open to all) versus "how to be part of the mesh" (share your changes)** — not the naive "core is permissive, firmware is GPL", which would give away the election, replication and rendering logic that makes a device a device. See [[Tech Stack#The licence boundary is not where it first looks]]; it has consequences, including that the desktop app is GPL because it joins the mesh as a virtual device.

Worth writing the reasoning into `CONTRIBUTING.md` — contributors will ask, and "the boundary is where the device ends" is a clear answer.

## Cross-cutting rules

Four rules do most of the work; when in doubt, check a design against them.

1. **Priority + timeout on every source.** Programs, streams, manual control and status overrides all declare a priority and an expiry. Nothing can permanently capture a pixel, and a dead publisher releases its claim automatically. A source above the ambient floor with no expiry is a bug, and the tooling should refuse it — that is how a room ends up stuck red at 3am.
2. **Scheduled activation, never "go now".** Changes take effect at a named future show time, so the mesh switches together even across a network hiccup.
3. **Mapping is pure upgrade.** Everything except genuinely volumetric effects works with nothing mapped. Every device always has coordinates — synthetic, rough, or truly mapped — so no feature needs an unmapped code path and no setup feels broken while a 50-device mapping session is pending.
4. **Defined degradation.** Every failure has a specified visual outcome. Stale channels decay to a default, a corrupt program falls back, a lost network keeps rendering. A device should never be dark because of software.

## The open-source project itself

Four things a newcomer needs, in the order they meet them.

**1. Simulator-first onboarding.** Clone, build, run — and see lights moving in a 3D view with no hardware at all. Because the firmware core is host-compilable with deterministic replay ([[Firmware#Structured for simulation]]), this is nearly free, and it is the strongest possible first impression: the project works before you have spent any money. Make it the first line of the README, ahead of any hardware instructions.

**2. Board definitions as the obvious first PR.** A documented path where someone's first contribution is a single file adding their own board ([[Firmware#Board definitions]]). It grows hardware coverage and creates contributors in the same motion, and it is a genuinely useful contribution that requires no understanding of the internals.

**3. A conformance suite as the spec's teeth.** One shared runner in `lumen-spec` plus data-file vectors, driving any implementation through a trivial stdin/stdout adapter ([[Tech Stack#The conformance runner]]). Prose describes intent; the suite defines correctness. Because the core is sans-IO, behavioural conformance is "given these events, did you emit these actions" — so even a three-way split brain is just a longer vector file, and a bug reproduced in the simulator exports as a test every implementation inherits.

**4. An effect cookbook.** Worked [[Effect Language]] examples covering all four use cases — lightshow, ambient, audio-reactive, status. The examples live as real `.lfx` files and [[Effect Cookbook]] is **generated from them**, so the tutorial and the test corpus cannot drift apart. Alongside them, a `failing/` set that must be *rejected* with the expected diagnostic — error messages are user experience and deserve the same protection as the working path.

The through-line: **each of these is simultaneously documentation and testing**. A cookbook that is also a test corpus, a conformance suite that is also the spec, board files that are also hardware support. For a project maintained in spare time, artefacts that serve two purposes are the only ones that stay current.

## Build order

Nothing is built yet, so the first job is not writing the system — it is **falsifying the three assumptions the architecture rests on**. Each spike below has a number attached. If a spike fails, the design above needs revisiting, and it is far cheaper to learn that now.

### Spikes — do these first, on real hardware

| # | Spike | Passes if | If it fails |
|---|---|---|---|
| S1 | Time sync across 3 ESP32s on ordinary WiFi, left running 24 h | 95th percentile offset under **±500 µs**, no drift over the session | Rendering must move towards streamed frames, or shows accept visible looseness |
| S2 | Hand-written bytecode interpreter **in Rust**, per-pixel kernel, 300 LEDs | **60 fps with ≥1000 instructions/pixel** of headroom on an S3 | Reduce ambition to fixed primitives, or accept 30 fps, or drop weaker chips |
| S3 | Multicast CHAN at 60 Hz to 10+ devices on a consumer AP | Loss under 1%, jitter under a frame | The channel design needs a unicast fallback before anything is built on it |

S1 and S2 together are the architecture. S3 decides how much of the network design needs a plan B. None of them need a compiler, an app, or a protocol — a few hundred lines each.

### Then

1. **Protocol skeleton + firmware skeleton** — discovery, framing, sync, solid colour, one hand-written program.
2. **Compiler core library** ([[Effect Language]]) with a CLI front-end. Text in, bytecode out, budget report. No GUI yet — the CLI is testable and is what CI will use anyway.
3. **Host-compilable firmware core behind a HAL**, then the **simulator** on top of it. Doing this before the system grows is the cheapest it will ever be, and every later step gets a test harness for free.
4. **Channels — audio first.** It validates the channel design under the hardest timing constraints, and it is the most visibly impressive early result.
5. **Zones, source stack, scenes** ([[Runtime Model]]) — the point at which the system becomes usable daily.
6. **Mapping in the phone app.** The hardest single piece and the biggest differentiator. Not last, not first.
7. **Node editor** on desktop, as a view over the text format.
8. **Replication and autonomy**, then **interop and bridges**, then **cues and live control**.
9. **Security.** Design the fields in from step 1; implement before anything is published or shared.

Full breakdown — scope, workstreams, dependency graph, milestones and risks — in [[Implementation Plan]].

Three notes on sequencing. **The HAL split and injected nondeterminism (step 3) has to happen early or not at all** — it is a cheap constraint on new code and an expensive refactor of existing code, and without it distributed bugs stay irreproducible forever. The **compiler before the editor** (2 before 7) is deliberate — with text as canonical, the editor is a convenience and the compiler is the product. And **security last but framed first**: the header fields, pairing flow and signature slots must exist in the protocol from step 1, or adding them later is a breaking change to every implementation that exists by then.

## Open questions across the project

Each note has its own list; these are the ones that cut across everything.

- **Naming.** "Lumen" is a placeholder in [[Protocol]] and it appears in the mDNS service type and probably the effect file extension. Decide before anything ships — and check the name is not already taken in the lighting world.
- **Conformance suite.** Now load-bearing in two ways: it keeps independent implementations compatible, *and* it is how a split-repo layout detects that a repo has fallen behind a protocol change ([[Tech Stack#Keeping them coherent]]). Start it alongside the first firmware, not after.
- **Release signing.** Prebuilt firmware images need a project signing key and a release process, and users need a documented way to verify what they flashed. Same underlying question as the licence — how does the community trust this — and it needs answering before the first public binary.
- **On-device compilation RAM ceiling.** `caps=compile` is only real if a representative effect compiles inside a few hundred KB. Measure this early — it decides whether the mesh can genuinely recompile itself, or whether that stays a desktop-and-phone capability.
