# Spike S1 — time sync over ordinary WiFi

Two **ESP32-C3s** on a domestic 2.4 GHz network with an ordinary consumer AP and
whatever else in the house was using it. One board is the time source, the other
follows; the exchange is the wire format's `SYNC_REQ` / `SYNC_RESP`, offset
`((t2-t1) + (t3-t4)) / 2`, round trip `(t4-t1) - (t3-t2)`, with the specified
1.5×-minimum filter. Samples every 200 ms rather than the specified 30 s, to
characterise a distribution rather than to hold a clock.

The plan asks for three boards over 24 hours. This is two boards over minutes, so
it does not close the question of long-run stability — but it is decisive about
the part that was in doubt, and it found two things nobody would have guessed.

## Verdict: fails the stated criterion, and the criterion is the wrong one

**±500 µs at the 95th percentile is not met.** The best the specified algorithm
achieved was **p50 225–350 µs, p95 675–1 500 µs** depending on window length and
on how busy the network was — one and a half to three times outside.

But the number that matters visually is a frame, and a frame at 60 fps is
16 667 µs. The measured p95 is **4–9% of a frame**. The tightest requirement the
design actually has is that a wave crossing six strips must not visibly tear, and
1 ms of jitter on a 16.7 ms frame is not a tear. So this reads as a **pass on the
requirement and a fail on the proxy** — and the proxy should be restated, because
the next person to read ±500 µs will spend real effort chasing it.

## Two findings that changed the design

### WiFi power save must be off, and it is not free

The first run measured a **17 ms minimum round trip** on an idle LAN and looked
like a catastrophe. It was the radio asleep. A station in the default power mode
parks between the AP's beacons and wakes on DTIM, which quantises every exchange
to the beacon interval.

Disabling it moved the minimum round trip from **17 ms to 4.3 ms** and the jitter
p95 from **5 000 µs to 1 250 µs** — a factor of four, for one line.

This is not a detail of the spike. A device that renders on a shared clock cannot
sleep between beacons, so the cost belongs in the power budget rather than being
recovered later by someone who does not know why it was disabled.

### The window length has an optimum, and it is not the specified one

| window | spans | p50 | p95 | p99 |
|---|---|---|---|---|
| single sample | 200 ms | 1 875 µs | 9 000 µs | 15 500 µs |
| 1.5× filter only | — | 375 µs | 1 200 µs | 1 750 µs |
| **best of 8** (specified) | 1.6 s | 325 µs | 1 500 µs | 3 000 µs |
| **best of 32** | 6.4 s | **225 µs** | **675 µs** | 825 µs |
| best of 128 | 25.6 s | 925 µs | 1 775 µs | 1 775 µs |

Best-of-32 beats the specified best-of-8 by more than a factor of two, for
nothing but a longer settle. That is the cheapest improvement available and it
should go in the spec.

**Best-of-128 is worse than best-of-32**, which is the interesting part. The
follower's clock drifts **33 µs per second** against the master's — an ordinary
crystal, well within spec — so a 25.6 s window accumulates about 800 µs of drift
*inside itself*, swamping the noise the longer window was meant to average away.
Short windows are noise-limited; long ones are drift-limited; the optimum sits
where the two cross, and where that is depends on the crystal, not the network.

### Modelling the drift does not rescue it, and it is worth saying why

The obvious next move is to estimate the rate as well as the offset. Measured, as
the second difference of consecutive window estimates — causal, using only what a
device would already have:

| window | raw p95 | after removing a constant rate |
|---|---|---|
| best of 32 | 675 µs | 850 µs — **worse** |
| best of 128 | 1 775 µs | 1 600 µs — slightly better |

Removing a rate from two points amplifies the noise on those points by about
2.4×. Where drift dominates, that trade is worth it; where noise dominates, it is
not. At 32 samples the estimator is already noise-limited, so skew correction
costs more than it saves.

A real implementation would fit a rate across many windows rather than
extrapolating from two, which does not amplify noise the same way. The finding is
not "skew correction is useless" — it is that **the limit here is network noise,
not drift**, and skew correction is the answer to the wrong half of the problem.

### The selector barely matters; the window length does

`lumen-device` resolves a burst by taking the **median** of its offsets, on the
argument that the RTT filter catches an occasionally-slow path but not a
consistently asymmetric one. Quickest-wins is the other standard answer, on the
argument that a fast exchange had less room to be asymmetric at all. Both are
defensible, so they were measured on identical samples rather than argued about:

| burst | quickest p95 | median p95 |
|---|---|---|
| 8 | 1 475 µs | 2 500 µs |
| **32** | **825 µs** | **850 µs** |
| 128 | 1 500 µs | 1 225 µs |

Quickest wins decisively at 8, loses at 128, and ties at 32 — median holds up at
long windows because it takes the middle in time as well as in value, while the
quickest sample can sit at either end of a window the clock drifted across.

At the length that matters they are indistinguishable, so **the implementation
does not change**. This started as a recommendation to switch to quickest-wins,
written before the comparison existed; the measurement retired it.

## Where the noise comes from

| | µs |
|---|---|
| minimum round trip | 4 290 |
| median | 10 000 |
| p95 | 30 000 |
| p99 | 60 000 |

Ten milliseconds median on a LAN is not the AP being slow, it is the topology:
station-to-station traffic crosses the air **twice**, once up to the AP and once
back down, on a shared and contended 2.4 GHz channel. The offset error is half
the *asymmetry* between the two legs, so the error scales with that round trip.

The filter keeps only about **22%** of samples, which sounds severe until you see
the single-sample p95 of 9 000 µs it is rejecting. It earns its rejection rate.

Three ways to attack the round trip, in descending order of how much they would
buy:

1. **Make the time master a device with a wired or privileged link** — a bridge,
   or whatever sits nearest the AP. One air hop instead of two.
2. **Prefer 5 GHz where available.** Less contention, shorter airtime.
3. **Accept it.** At 4–9% of a frame the visual requirement is already met.

## What this does not answer

- **24 hours.** Runs here were minutes. Drift was linear and consistent at
  ~33 µs/s across every run, which is the encouraging sign, but thermal drift
  over a day is not something minutes can show.
- **Three or more boards.** Two cannot exhibit the failure modes that matter for
  election, or a master's load under many followers.
- **Absolute skew.** Two boards have no common reference, so this measures the
  *dispersion* of the estimate — which bounds the error, since a follower cannot
  track better than its estimate is stable, but is not the same as measuring the
  error. The definitive test is a wire: one board pulsing a GPIO on its
  show-second and the other timestamping the edge. That needs two jumper leads,
  and it is the right next measurement if anyone doubts these numbers.

## Recommended changes to the spec

1. **Restate the criterion** in terms of a fraction of a frame rather than an
   absolute microsecond figure. What the design needs is "well under a frame";
   ±500 µs was a guess at what that meant and is roughly three times tighter than
   necessary.
2. **Raise the burst from 8 samples to 32.** Roughly halves the p95 at the cost
   of a longer settle, and nothing else in the design cares how long settling
   takes. Keep the median selector — measured against quickest-wins it is a tie
   at this length.
3. **Require power save off**, and say why, in the firmware notes rather than
   only here.
4. **Note the drift/noise crossover.** Any future change to the window length
   needs to know that longer is not monotonically better.

## Reproducing

Credentials come from the environment at build time and are never in the
repository:

```bash
cd lumen-dev/spikes/s1-time-sync
export LUMEN_WIFI_SSID='your ssid' LUMEN_WIFI_PASS='your password'

LUMEN_ROLE=master   cargo build --release && \
  espflash flash --port COM13 --monitor target/.../s1-time-sync
LUMEN_ROLE=follower cargo build --release && \
  espflash flash --port COM17 --monitor target/.../s1-time-sync
```

The follower prints a report every 100 samples. The master needs no
configuration beyond its role; it broadcasts so the follower finds it without
being told an address.

**Flash with `--monitor`.** Without it the board does not reliably restart into
the application, and the resulting silence looks exactly like a hang in `main` —
which cost an hour before the pattern became obvious.
