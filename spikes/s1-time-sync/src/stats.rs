//! The distribution, which is the whole output of the spike.
//!
//! A mean would hide exactly what matters. The criterion is a **95th
//! percentile**, because a clock that is right on average and wrong once a
//! second still tears the wave once a second, and because a consumer AP's
//! badness is entirely in its tail: a retry or a power-save wake adds
//! milliseconds to one exchange in fifty and nothing at all to the rest.
//!
//! So this keeps histograms rather than running means, in fixed memory, because
//! a spike that grows a buffer for 24 hours is a spike that dies at hour three.
//!
//! # Why the filter is measured rather than assumed
//!
//! The wire format says to discard any sample whose round trip exceeds 1.5× the
//! running minimum. That rule is doing real work — a delayed packet biases the
//! offset by half its delay, and the delay is not symmetric — but how much work,
//! and at what cost in samples, is an empirical question nobody had asked. So
//! the jitter is accumulated twice, once over everything and once over what
//! survives, and both are reported. If the filter is not earning its rejection
//! rate, that is worth knowing before a device throws away nine samples in ten
//! on a network that was fine.

use esp_println::println;

/// Two resolutions, because the interesting range and the possible range differ
/// by two orders of magnitude.
///
/// The first run of this spike put every percentile at the top bucket: the
/// histogram stopped at 5 ms and the samples were tens of milliseconds, so
/// p50, p95 and p99 all read "5000" and said nothing. Detail is needed around
/// the ±500 µs criterion and range is needed to see a network behaving badly,
/// and one uniform bucket width cannot give both without a great deal of memory.
///
/// Below 2 ms: 25 µs buckets, finer than the thing being judged.
const FINE_US: i64 = 25;
const FINE_LIMIT_US: i64 = 2_000;
const FINE_BUCKETS: usize = (FINE_LIMIT_US / FINE_US) as usize;

/// Above it: 500 µs buckets out to 60 ms, which is past anything a network
/// should do and well past anything this design can tolerate.
const COARSE_US: i64 = 500;
const COARSE_BUCKETS: usize = 116;

const BUCKETS: usize = FINE_BUCKETS + COARSE_BUCKETS;

/// Samples whose round trip exceeds this multiple of the running minimum are
/// discarded, per the wire format.
const RTT_TOLERANCE_NUM: i64 = 3;
const RTT_TOLERANCE_DEN: i64 = 2;

/// Burst lengths to evaluate, the first being the wire format's `Syncing until
/// 8 samples`.
///
/// A device does not act on one exchange. It gathers a burst and uses the one
/// that spent least time in the air, because a round trip that was quick had
/// less room to be asymmetric, and asymmetry is the entire error term: a delay
/// on one leg only biases the offset by half of itself, and there is no way to
/// detect which leg it was. Judging the design on single samples, which the
/// first version of this did, measures something the design never uses.
///
/// Three lengths rather than one, because the specified eight turned out to sit
/// just the wrong side of the criterion and the obvious question — does a longer
/// window fix it, and at what cost in how long a device takes to settle — is
/// answerable in the same run rather than in three.
const BURSTS: [usize; 3] = [8, 32, 128];

/// The longest window, so each estimator can hold its samples for a median.
const MAX_BURST: usize = 128;

type Histogram = [u32; BUCKETS];

pub struct Samples {
    /// Round trips, over everything. Reading the network rather than the clock.
    rtt: Histogram,
    /// Dispersion of the offset estimate over every sample.
    ///
    /// The quantity the criterion is about, and measurable with two boards and
    /// no common reference: successive estimates of something that is changing
    /// only by slow drift ought to agree, and how much they disagree bounds how
    /// well a follower could possibly track.
    jitter_all: Histogram,
    /// The same, over the samples the filter kept.
    jitter_kept: Histogram,

    /// One estimator per burst length in `BURSTS`.
    best: [Burst; BURSTS.len()],

    min_rtt: i64,
    last_offset_all: Option<i64>,
    last_offset_kept: Option<i64>,
    total: usize,
    kept: usize,
    worst_all: i64,
    worst_kept: i64,
}

impl Samples {
    pub fn new() -> Self {
        Samples {
            rtt: [0; BUCKETS],
            jitter_all: [0; BUCKETS],
            jitter_kept: [0; BUCKETS],
            best: core::array::from_fn(|_| Burst::new()),
            min_rtt: i64::MAX,
            last_offset_all: None,
            last_offset_kept: None,
            total: 0,
            kept: 0,
            worst_all: 0,
            worst_kept: 0,
        }
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn add(&mut self, now: u64, rtt: i64, offset: i64) {
        self.total += 1;

        for (i, burst) in self.best.iter_mut().enumerate() {
            burst.add(now, BURSTS[i], rtt, offset);
        }

        // The running minimum has to be updated before the filter, or the first
        // sample sets a tolerance nothing can pass.
        if rtt < self.min_rtt {
            self.min_rtt = rtt;
        }
        bump(&mut self.rtt, rtt);

        if let Some(previous) = self.last_offset_all {
            let jitter = (offset - previous).abs();
            bump(&mut self.jitter_all, jitter);
            self.worst_all = self.worst_all.max(jitter);
        }
        self.last_offset_all = Some(offset);

        if rtt * RTT_TOLERANCE_DEN > self.min_rtt * RTT_TOLERANCE_NUM {
            return;
        }

        if let Some(previous) = self.last_offset_kept {
            let jitter = (offset - previous).abs();
            bump(&mut self.jitter_kept, jitter);
            self.worst_kept = self.worst_kept.max(jitter);
        }
        self.last_offset_kept = Some(offset);
        self.kept += 1;
    }

    /// One report: the network, the clock, and whether the filter earned itself.
    pub fn report(&self, now: u64) {
        // Drift measured against the first *burst* estimate rather than the
        // first raw sample. Anchoring to a single exchange makes a bad first
        // sample look like a permanent offset for the rest of the run, which is
        // what the earlier version of this reported: a constant -38 ms that
        // never grew and was therefore not drift at all.
        // Drift from the longest window, which is the least noisy view of it.
        let reference = &self.best[BURSTS.len() - 1];
        let (t0, o0) = reference.first.unwrap_or((now, 0));
        let drift_us = reference.last.unwrap_or(o0) - o0;
        let elapsed_us = now.saturating_sub(t0);
        let seconds = elapsed_us / 1_000_000;
        // Parts per million, the form a crystal's error is specified in and so
        // the form in which this number can be checked against the datasheet.
        let ppm = if seconds > 0 {
            drift_us / seconds as i64
        } else {
            0
        };

        println!(
            "n={} kept={} ({}%) min_rtt={}us",
            self.total,
            self.kept,
            if self.total > 0 {
                self.kept * 100 / self.total
            } else {
                0
            },
            self.min_rtt
        );
        self.line("rtt        ", &self.rtt, -1);
        self.line("jitter each", &self.jitter_all, self.worst_all);
        self.line("jitter 1.5x", &self.jitter_kept, self.worst_kept);
        for (i, burst) in self.best.iter().enumerate() {
            let n = BURSTS[i];
            println!(
                "   best of {n:<4} p50={}us p95={}us p99={}us worst={}us over {} windows",
                percentile(&burst.jitter, 50),
                percentile(&burst.jitter, 95),
                percentile(&burst.jitter, 99),
                burst.worst,
                burst.windows
            );
            println!(
                "     median     p50={}us p95={}us p99={}us worst={}us",
                percentile(&burst.jitter_median, 50),
                percentile(&burst.jitter_median, 95),
                percentile(&burst.jitter_median, 99),
                burst.worst_median,
            );
            println!(
                "     de-drifted p50={}us p95={}us p99={}us worst={}us",
                percentile(&burst.residual, 50),
                percentile(&burst.residual, 95),
                percentile(&burst.residual, 99),
                burst.worst_residual,
            );
        }
        println!("   drift  {drift_us}us over {seconds}s ({ppm} us/s)");

        // The criterion is stated on what a device would actually use: the best
        // exchange of a burst of the specified length.
        let p95 = percentile(&self.best[0].jitter, 95);
        println!(
            "   => p95 {p95}us {} the +/-500us criterion",
            if p95 >= 0 && p95 < 500 {
                "is within"
            } else {
                "is OUTSIDE"
            }
        );
    }

    fn line(&self, label: &str, h: &Histogram, worst: i64) {
        if worst >= 0 {
            println!(
                "   {label} p50={}us p95={}us p99={}us worst={}us",
                percentile(h, 50),
                percentile(h, 95),
                percentile(h, 99),
                worst
            );
        } else {
            println!(
                "   {label} p50={}us p95={}us p99={}us",
                percentile(h, 50),
                percentile(h, 95),
                percentile(h, 99),
            );
        }
    }
}

/// One burst-length estimator: the best exchange in each window of `n`, and how
/// much consecutive windows disagree.
struct Burst {
    jitter: Histogram,
    /// What is left after a constant rate is removed.
    ///
    /// The measurement showed the long windows doing *worse* than the medium
    /// ones, which is only sensible once you see that the clock drifts about
    /// 32 us every second: a 128-sample window spans 25 s, so 800 us of drift
    /// accumulates inside it and swamps the network noise the window was
    /// lengthened to average away. Long windows are drift-limited, short ones
    /// noise-limited, and there is an optimum in between that has nothing to do
    /// with the network.
    ///
    /// Drift is not noise, though — it is a rate, and a rate can be predicted.
    /// This histogram is the second difference of the estimates, which is what a
    /// follower that models skew rather than only offset would be left with. If
    /// it is materially smaller than `jitter`, the criterion is reachable by
    /// estimating the rate rather than by improving the network.
    residual: Histogram,
    /// The same window, resolved by median instead of by quickest.
    ///
    /// `lumen-device` picks the median and gives a good reason: the RTT filter
    /// catches a path that is *occasionally* slow, but not one that is
    /// *consistently* asymmetric, and a median ignores an outlier that a mean
    /// would follow by an eighth of its error. Quickest-wins is the other
    /// standard answer, on the argument that a fast exchange had less room to be
    /// asymmetric in the first place.
    ///
    /// Both are defensible and they disagree, so neither belongs in a
    /// specification on the strength of an argument. Measured side by side here
    /// on identical samples.
    jitter_median: Histogram,
    window: [i64; MAX_BURST],
    worst_median: i64,
    last_median: Option<i64>,
    pending: Option<(i64, i64)>,
    count: usize,
    last: Option<i64>,
    previous: Option<i64>,
    first: Option<(u64, i64)>,
    windows: usize,
    worst: i64,
    worst_residual: i64,
}

impl Burst {
    fn new() -> Self {
        Burst {
            jitter: [0; BUCKETS],
            residual: [0; BUCKETS],
            jitter_median: [0; BUCKETS],
            window: [0; MAX_BURST],
            worst_median: 0,
            last_median: None,
            pending: None,
            count: 0,
            last: None,
            previous: None,
            first: None,
            windows: 0,
            worst: 0,
            worst_residual: 0,
        }
    }

    fn add(&mut self, now: u64, n: usize, rtt: i64, offset: i64) {
        // Keep the quickest exchange seen so far in this window, and the whole
        // window besides, so the two selectors are compared on the same samples
        // rather than on two runs of a network that changes underneath them.
        if self.pending.is_none_or(|(best, _)| rtt < best) {
            self.pending = Some((rtt, offset));
        }
        if self.count < MAX_BURST {
            self.window[self.count] = offset;
        }
        self.count += 1;
        if self.count < n {
            return;
        }

        let median = {
            let mut sorted = self.window;
            sorted[..n].sort_unstable();
            // Even count, so average the middle two. In i128, because two large
            // opposite-signed offsets would otherwise overflow on the way to a
            // perfectly representable answer.
            ((sorted[n / 2 - 1] as i128 + sorted[n / 2] as i128) / 2) as i64
        };
        if let Some(previous) = self.last_median {
            let jitter = (median - previous).abs();
            bump(&mut self.jitter_median, jitter);
            self.worst_median = self.worst_median.max(jitter);
        }
        self.last_median = Some(median);

        let (_, best_offset) = self.pending.take().expect("a window has a best");
        self.count = 0;
        self.windows += 1;
        if let Some(previous) = self.last {
            let jitter = (best_offset - previous).abs();
            bump(&mut self.jitter, jitter);
            self.worst = self.worst.max(jitter);

            // Windows are equally spaced, so extrapolating a constant rate from
            // the two before is just the second difference. Causal on purpose:
            // it uses only what a device would already have, not a line fitted
            // through the whole run in hindsight.
            if let Some(before) = self.previous {
                let predicted = 2 * previous - before;
                let residual = (best_offset - predicted).abs();
                bump(&mut self.residual, residual);
                self.worst_residual = self.worst_residual.max(residual);
            }
        }
        self.previous = self.last;
        self.last = Some(best_offset);
        if self.first.is_none() {
            self.first = Some((now, best_offset));
        }
    }
}

fn bump(histogram: &mut Histogram, value: i64) {
    histogram[bucket_of(value)] = histogram[bucket_of(value)].saturating_add(1);
}

/// Which bucket a microsecond value falls in.
fn bucket_of(value: i64) -> usize {
    if value <= 0 {
        0
    } else if value < FINE_LIMIT_US {
        (value / FINE_US) as usize
    } else {
        let coarse = ((value - FINE_LIMIT_US) / COARSE_US) as usize;
        (FINE_BUCKETS + coarse).min(BUCKETS - 1)
    }
}

/// The upper edge of a bucket, in microseconds.
fn edge_of(index: usize) -> i64 {
    if index < FINE_BUCKETS {
        (index as i64 + 1) * FINE_US
    } else {
        FINE_LIMIT_US + ((index - FINE_BUCKETS) as i64 + 1) * COARSE_US
    }
}

/// The upper edge of the bucket holding the `p`th percentile, in microseconds.
///
/// Reported as an upper edge rather than interpolated: with 25 µs buckets the
/// precision is already finer than the thing being measured, and an interpolated
/// figure would imply a confidence the histogram does not have.
fn percentile(histogram: &Histogram, p: u32) -> i64 {
    let total: u32 = histogram.iter().sum();
    if total == 0 {
        return -1;
    }
    // Round up, so p95 of 100 samples is the 95th and not the 94th.
    let target = ((total as u64 * p as u64) + 99) / 100;
    let mut seen = 0u64;
    for (i, &count) in histogram.iter().enumerate() {
        seen += count as u64;
        if seen >= target {
            return edge_of(i);
        }
    }
    edge_of(BUCKETS - 1)
}
