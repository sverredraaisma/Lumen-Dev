//! A fixed-size histogram, because the interesting number is a tail.
//!
//! The criterion is stated on loss and on jitter, and both are questions about
//! the worst few percent rather than the average. A channel that is reliably
//! late can be compensated for; one that is usually fine and occasionally awful
//! cannot, because a receiver never knows which kind of packet it is holding.
//!
//! Two resolutions, for the same reason S1 needed them: detail is wanted around
//! one frame and range is wanted to see a network behaving badly, and one bucket
//! width cannot give both without a great deal of memory.

/// Below 40 ms: 250 µs buckets, so a 16 667 µs frame is resolved to about 1.5%.
const FINE_US: i64 = 250;
const FINE_LIMIT_US: i64 = 40_000;
const FINE_BUCKETS: usize = (FINE_LIMIT_US / FINE_US) as usize;

/// Above it: 10 ms buckets out to a second. A gap of a second is a device that
/// has stopped receiving, and the exact size stops mattering well before then.
const COARSE_US: i64 = 10_000;
const COARSE_BUCKETS: usize = 96;

const BUCKETS: usize = FINE_BUCKETS + COARSE_BUCKETS;

pub struct Histogram {
    counts: [u32; BUCKETS],
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub const fn new() -> Histogram {
        Histogram {
            counts: [0; BUCKETS],
        }
    }

    pub fn add(&mut self, value: i64) {
        let i = Self::bucket_of(value);
        self.counts[i] = self.counts[i].saturating_add(1);
    }

    /// How many samples were at least `threshold`.
    pub fn count_above(&self, threshold: i64) -> u32 {
        let first = Self::bucket_of(threshold);
        self.counts[first..].iter().sum()
    }

    /// The upper edge of the bucket holding the `p`th percentile.
    ///
    /// An upper edge rather than an interpolation: the bucket is already finer
    /// than the thing being measured, and interpolating would imply a
    /// confidence the histogram does not have.
    pub fn percentile(&self, p: u32) -> i64 {
        let total: u32 = self.counts.iter().sum();
        if total == 0 {
            return -1;
        }
        let target = ((total as u64 * p as u64) + 99) / 100;
        let mut seen = 0u64;
        for (i, &n) in self.counts.iter().enumerate() {
            seen += n as u64;
            if seen >= target {
                return Self::edge_of(i);
            }
        }
        Self::edge_of(BUCKETS - 1)
    }

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

    fn edge_of(index: usize) -> i64 {
        if index < FINE_BUCKETS {
            (index as i64 + 1) * FINE_US
        } else {
            FINE_LIMIT_US + ((index - FINE_BUCKETS) as i64 + 1) * COARSE_US
        }
    }
}
