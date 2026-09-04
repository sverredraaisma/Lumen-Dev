# Spike S3 — multicast `CHAN` at 60 Hz

One **ESP32-C3** sending a `CHAN`-shaped datagram every 16 667 µs, two more
receiving it, on a domestic 2.4 GHz network with a consumer AP and whatever else
the household had connected. Loss from gaps in `producer_seq`; jitter from the
spread of arrival intervals.

The plan asks for ten or more devices. This is two receivers, so it does not
answer how the AP behaves as the group grows — which is the question a larger
test would add. What it does answer is whether the mechanism works at all, and
there the answer is clear enough that group size is not the deciding factor.

## Verdict: multicast fails, unicast passes on loss

| | loss | gap p50 | gap p95 | gap p99 |
|---|---|---|---|---|
| **multicast** | **4–6%** | 16 750 µs | 26–32 ms | 37–40 ms |
| **unicast** | **0.00%** | 16 750 µs | 25–27 ms | 31–32 ms |

Both figures are from awake receivers over steady-state windows of 600
datagrams, with the sender confirmed to have dropped none of its own.

**Multicast misses the 1% criterion by four to six times.** That is what an
unacknowledged frame costs: a multicast frame is sent once at a low basic rate
with no ACK and no retry, so every frame the receiver misses is simply gone.
Unicast is acknowledged and retried by the radio, and over three consecutive
windows lost nothing at all.

**Both modes miss the jitter criterion**, which asks for less than a frame.
Arrivals sit at the send interval at the median — the p50 of 16 750 µs against a
16 667 µs send is the sender's own accuracy showing through — but the 95th
percentile is around 26 ms and the 99th around 31–40 ms. That is one and a half
to two and a half frames. A device rendering strictly on arrival would visibly
stutter; one holding the last value and rendering on its own clock would not,
which is what the design already specifies.

So: **the channel design needs the unicast fallback the plan prepared**, and it
needs receivers to render on the show clock rather than on arrival. Neither is a
surprise; both are now measured rather than assumed.

## What this cost to measure honestly

Four of the five runs produced a wrong answer, and each was wrong for a different
reason. Worth recording, because every one of them looked like a result.

**The sender was dropping its own packets.** The first runs showed 11–13% loss.
Instrumenting `send_slice` showed the source refusing 7% of its own sends —
smoltcp's transmit queue backing up behind the radio — while comfortably making
its 16 667 µs deadline. A send that never left the device is not network loss,
however much it looks like it from the far end. Sixty-four packet slots and 8 KiB
of buffer took local refusals to zero.

**Cumulative averages never recover from a bad start.** With the sender fixed,
loss still read 2–3%. Almost all of it was the first few seconds, while sinks
were still associating and the source's queue was draining. Reporting per window
rather than since boot separated startup from steady state, and steady state was
zero.

**A sleeping receiver looks exactly like a lossy network.** In several runs one
of two identical C3s showed arrivals clustered 250 µs apart in bursts about
110 ms apart — the beacon interval — with several times the loss of its twin.
That is a station waking on DTIM, and which device it happened to be changed
between runs. `set_power_saving(None)` before `connect` does not survive
association; after association it mostly does. **Re-applying it every second is
worse**, not better: calling into the radio driver that often disturbed it and
took both sinks from 4–6% loss to 13–17%.

The residual limitation is that one: on esp-wifi 0.12 a station sometimes sleeps
anyway. The signature is unmistakable, so a window with a gap p50 far below the
send interval was discarded as measuring the receiver rather than the network —
but a longer run should verify power state directly rather than inferring it.

## What was nearly reported instead

That multicast loses 11–13% and unicast is no better, and therefore that the
whole channel design fails. Three of those four errors inflated the loss and the
fourth inflated it selectively; together they turned a 4–6% multicast problem
with a working fallback into an architecture that does not function.

The order the errors were found in is the useful part. Each was only visible
once the one before it was fixed: the sender's own drops hid the startup
transient, the startup transient hid the steady state, and the steady state was
where the sleeping receiver finally stood out as one device differing from its
twin rather than as noise.

## What this does not answer

- **Ten or more devices.** Two receivers cannot show whether the AP degrades as
  a multicast group grows, or what unicast's airtime costs at ten — where it is
  ten sends per tick against multicast's one, and 600 packets a second.
- **A busy network.** The measurements were taken on an ordinary evening, not
  against a deliberate load.
- **Whether the AP's multicast handling changes** when a client is genuinely
  asleep. Consumer APs defer multicast to DTIM when any station is power-saving,
  and nothing here controlled what else was connected.

## Reproducing

```bash
cd lumen-dev/spikes/s3-multicast
export LUMEN_WIFI_SSID='your ssid' LUMEN_WIFI_PASS='your password'

LUMEN_ROLE=source LUMEN_MODE=multicast cargo build --release   # then flash one
LUMEN_ROLE=sink   LUMEN_MODE=multicast cargo build --release   # and the rest
```

`LUMEN_MODE=unicast` runs the comparison: sinks announce themselves once a
second and the source addresses each in turn.

Flash with `--monitor`, and read the **windowed** reports rather than the
first — window one is startup and says nothing about the network.
