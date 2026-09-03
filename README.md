# lumen-dev

The meta-repo. One checkout, one `cargo test`, across repo boundaries.

Splitting the project into seven repos keeps forks and pull requests focused —
someone adding a board never clones the phone app — and makes the licence
boundary a fact about each repo rather than something anyone has to reason
about. The one real cost is that a protocol change now touches four repos. This
repo is one of the three things that contain that cost.

Start here if you want the design: `docs/overview.md`, `docs/tech-stack.md`,
`docs/implementation-plan.md`.

## Layout

Clone this repo, then put the siblings **next to it**:

```
lumen/
  lumen-dev/        <- you are here
  lumen-spec/
  lumen-core/
  lumen-device/
  lumen-firmware/
  lumen-desktop/
  lumen-android/
  lumen-effects/
```

```
scripts/clone-all.sh                           # clone or fast-forward all siblings
scripts/clone-all.sh git@github.com:sverredraaisma   # ... over SSH instead
scripts/foreach.sh git status --short          # run a command in each
scripts/foreach.sh cargo test --workspace      # test everything
cargo test                                     # build all siblings together (canary)
```

`crates/canary` depends on every sibling crate by path. Its only job is to fail:
a `lumen-core` change that breaks `lumen-device` should surface in one command
here, not three weeks later in a dependent repo.

## Keeping the repos coherent

Three mechanisms, in order of how much work they do:

1. **Spec-first changes.** A protocol change lands in `lumen-spec` first — IDL
   plus new conformance vectors — then `lumen-core`, then dependents. The
   vectors turn "has that repo caught up yet" into a CI answer instead of a
   memory exercise.
2. **This meta-repo.** Path overrides, so cross-cutting work is one checkout.
   Without it the split is genuinely unpleasant; with it, it is nearly free
   during development.
3. **Canary CI.** Each dependent repo tests against both its **pinned**
   `lumen-core` version and `lumen-core` **main**. The pinned build is what
   ships; the canary build is what tells you a core change broke the firmware
   before it is merged.

Protocol version negotiation is the runtime backstop: repos that fall out of
step degrade visibly rather than failing mysteriously.

## Dependency direction — strictly acyclic

```
lumen-spec ──► lumen-core ──► lumen-device ──┬─► lumen-firmware
                    │                        ├─► lumen-desktop
                    │                        └─► lumen-android
lumen-effects (stdlib, vendored by version) ─┘
```

## Status

Milestone **M1 — Foundations** (W1). Seven repos plus this one, licence files,
CI on empty crates, HAL traits defined, IDL skeleton. Boring, and doing it later
costs far more than doing it now.

Nothing after this should start before the three spikes are answered on real
hardware — time sync (S1), VM throughput (S2), multicast channels (S3). They are
a few hundred lines each and they are the architecture; see
`docs/overview.md#build-order`.
