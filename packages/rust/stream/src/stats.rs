//! Latency distributions recorded on the frame path without allocating or locking, and read
//! back off it.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Bins of 0.1 ms up to 51.2 ms; the last also holds everything slower.
const BINS: usize = 512;
const BIN_NS: i64 = 100_000;

pub struct Histogram {
    bins: [AtomicU32; BINS],
    count: AtomicU64,
    max_ns: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            bins: std::array::from_fn(|_| AtomicU32::new(0)),
            count: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    /// Wait-free; a negative duration counts as zero.
    pub fn record(&self, ns: i64) {
        let ns = ns.max(0);
        let bin = ((ns / BIN_NS) as usize).min(BINS - 1);
        self.bins[bin].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.max_ns.fetch_max(ns as u64, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// The upper edge of the bin holding quantile `q`, in ms; `None` before any sample.
    pub fn quantile_ms(&self, q: f64) -> Option<f64> {
        let total: u64 = self
            .bins
            .iter()
            .map(|b| u64::from(b.load(Ordering::Relaxed)))
            .sum();
        if total == 0 {
            return None;
        }
        let rank = ((total as f64 * q.clamp(0.0, 1.0)).ceil() as u64).max(1);
        let mut seen = 0;
        for (index, bin) in self.bins.iter().enumerate() {
            seen += u64::from(bin.load(Ordering::Relaxed));
            if seen >= rank {
                return Some((index as i64 + 1) as f64 * BIN_NS as f64 / 1e6);
            }
        }
        None
    }

    pub fn max_ms(&self) -> f64 {
        self.max_ns.load(Ordering::Relaxed) as f64 / 1e6
    }

    /// `p50 a, p95 b, max c ms (n)`, or `none` before any sample.
    pub fn summary(&self) -> String {
        match (self.quantile_ms(0.5), self.quantile_ms(0.95)) {
            (Some(p50), Some(p95)) => format!(
                "p50 {p50:.1}, p95 {p95:.1}, max {:.1} ms ({})",
                self.max_ms(),
                self.count()
            ),
            _ => "none".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_land_in_the_bin_of_the_sample() {
        let h = Histogram::default();
        assert_eq!(h.quantile_ms(0.5), None);
        for ms in 1..=100 {
            h.record(ms * 1_000_000);
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.quantile_ms(0.5), Some(50.1));
        assert_eq!(
            h.quantile_ms(0.95),
            Some(51.2),
            "beyond the range lands in the last bin"
        );
        assert_eq!(h.max_ms(), 100.0);
        let fast = Histogram::default();
        fast.record(8_340_000);
        fast.record(-5);
        assert_eq!(fast.quantile_ms(1.0), Some(8.4));
        assert_eq!(fast.quantile_ms(0.0), Some(0.1));
    }
}
