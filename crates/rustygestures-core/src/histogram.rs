//! Fixed-size atomic histogram for hook-callback duration.
//!
//! Recording only bumps atomics, so the hook callback can use it.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bucket `i` counts samples whose duration in microseconds is `i`, for `i` in
/// `0..1000`. The last bucket counts anything at or above 1000 µs.
const BUCKETS: usize = 1001;
const OVERFLOW_US: u64 = 1000;

/// Lock-free latency histogram. One histogram is shared by the hook callback
/// and the thread that prints the summary.
pub struct LatencyHistogram {
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    max_ns: AtomicU64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    /// Record one sample. `elapsed_ns` is the callback duration.
    pub fn record_ns(&self, elapsed_ns: u64) {
        let us = elapsed_ns / 1000;
        let index = if us >= OVERFLOW_US {
            BUCKETS - 1
        } else {
            us as usize
        };
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        let mut current = self.max_ns.load(Ordering::Relaxed);
        while elapsed_ns > current {
            match self.max_ns.compare_exchange_weak(
                current,
                elapsed_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    #[must_use]
    pub fn summary(&self) -> LatencySummary {
        let count = self.count.load(Ordering::Relaxed);
        let max_ns = self.max_ns.load(Ordering::Relaxed);
        LatencySummary {
            count,
            p50_us: self.quantile_us(count, 0.50),
            p99_us: self.quantile_us(count, 0.99),
            max_us: max_ns.div_ceil(1000),
        }
    }

    fn quantile_us(&self, count: u64, q: f64) -> u64 {
        if count == 0 {
            return 0;
        }
        let target = ((count as f64) * q).ceil().max(1.0) as u64;
        let mut accumulated = 0u64;
        for (index, bucket) in self.buckets.iter().enumerate() {
            accumulated += bucket.load(Ordering::Relaxed);
            if accumulated >= target {
                if index == BUCKETS - 1 {
                    return OVERFLOW_US;
                }
                return index as u64;
            }
        }
        OVERFLOW_US
    }
}

/// Percentiles computed outside the hook callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencySummary {
    pub count: u64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

impl std::fmt::Display for LatencySummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "latency samples={} p50={}us p99={}us max={}us",
            self.count, self.p50_us, self.p99_us, self.max_us
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_summary_is_zero() {
        let summary = LatencyHistogram::new().summary();
        assert_eq!(summary.count, 0);
        assert_eq!(summary.p50_us, 0);
        assert_eq!(summary.p99_us, 0);
        assert_eq!(summary.max_us, 0);
    }

    #[test]
    fn single_bucket_percentiles() {
        let histogram = LatencyHistogram::new();
        for _ in 0..10 {
            histogram.record_ns(5_000);
        }
        let summary = histogram.summary();
        assert_eq!(summary.count, 10);
        assert_eq!(summary.p50_us, 5);
        assert_eq!(summary.p99_us, 5);
        assert_eq!(summary.max_us, 5);
    }

    #[test]
    fn overflow_bucket_and_max() {
        let histogram = LatencyHistogram::new();
        histogram.record_ns(1_500_000);
        let summary = histogram.summary();
        assert_eq!(summary.p50_us, OVERFLOW_US);
        assert_eq!(summary.p99_us, OVERFLOW_US);
        assert_eq!(summary.max_us, 1500);
    }

    #[test]
    fn p99_uses_the_upper_tail() {
        let histogram = LatencyHistogram::new();
        for _ in 0..90 {
            histogram.record_ns(3_000);
        }
        for _ in 0..10 {
            histogram.record_ns(40_000);
        }
        let summary = histogram.summary();
        assert_eq!(summary.count, 100);
        assert_eq!(summary.p50_us, 3);
        assert_eq!(summary.p99_us, 40);
        assert_eq!(summary.max_us, 40);
    }
}
