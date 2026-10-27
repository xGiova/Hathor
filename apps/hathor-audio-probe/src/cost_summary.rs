//! Turning a pile of nanosecond samples into the numbers a gate is decided on.
//!
//! Pure functions over a `&mut [u64]`: no atomics, no cpal, no clock. Everything
//! here can be — and is — tested against hand-written samples, because a mean
//! computed wrongly is worse than no mean: it gets quoted into a language decision.
//!
//! ## Percentile method: nearest-rank
//!
//! `p99` is the value at index `ceil(0.99 · n) − 1` of the *sorted* samples. No
//! interpolation: an interpolated p99 invents a number that was never measured, and
//! a latency report is not the place for that. With `n < 100` the p99 is effectively
//! the maximum, which the printout says out loud.

use core::fmt;

/// Block budget in ns for a `frames`-block at `rate`: the number the M0 gate is
/// stated against. Integer nanoseconds, truncated down (a budget you rounded *up*
/// would be a slightly easier gate).
pub fn budget_ns(frames: u32, rate: u32) -> u64 {
    (u64::from(frames) * 1_000_000_000) / u64::from(rate.max(1))
}

/// The 64-frame/48 kHz budget in ns, as a constant: `budget_ns(64, 48_000)` evaluated
/// at compile time = 1,333,333 ns (1333.33 µs, the value
/// [`hathor_dsp_core::BUDGET_64_48K_US`] states in µs). The bench prints it as the
/// denominator of its "% budget" column, so it is written down once rather than
/// divided by hand in a string.
pub const BUDGET_64_48K_NS: u64 = (64 * 1_000_000_000) / 48_000;

/// The G1 warn threshold: 0.40 × budget ([ROADMAP.md](../../../ROADMAP.md) §M0
/// criterion 4: "worst-case callback < 0.40× budget").
pub fn warn_ns(budget_ns: u64) -> u64 {
    budget_ns * 2 / 5
}

/// Nearest-rank percentile over a **sorted** slice. `p` is a fraction, e.g. `0.99`.
///
/// Returns `0.0` for an empty slice rather than panicking: an empty sample set is a
/// real state of a run that never got a callback, and the report says so.
pub fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    let idx = rank.min(sorted.len()) - 1;
    sorted[idx] as f64
}

/// The distribution of chunk costs, as printed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostSummary {
    /// Timed samples behind the numbers.
    pub samples: usize,
    /// Arithmetic mean, ns.
    pub mean_ns: f64,
    /// Median, ns.
    pub median_ns: f64,
    /// Nearest-rank p99, ns.
    pub p99_ns: f64,
    /// Maximum, ns.
    pub max_ns: u64,
    /// Samples above the warn threshold (0.40 × budget).
    pub over_warn: usize,
    /// Samples above the budget itself.
    pub over_budget: usize,
    /// The budget these numbers were judged against, ns.
    pub budget_ns: u64,
}

impl CostSummary {
    /// Sorts `samples` in place and reduces them. `budget_ns` is the *chunk* budget.
    pub fn from_samples(samples: &mut [u64], budget_ns: u64) -> Self {
        samples.sort_unstable();
        let warn = warn_ns(budget_ns);
        let sum: u64 = samples.iter().sum();
        let over_warn = samples.iter().filter(|s| **s > warn && **s <= budget_ns).count();
        let over_budget = samples.iter().filter(|s| **s > budget_ns).count();
        Self {
            samples: samples.len(),
            mean_ns: if samples.is_empty() {
                0.0
            } else {
                sum as f64 / samples.len() as f64
            },
            median_ns: percentile(samples, 0.50),
            p99_ns: percentile(samples, 0.99),
            max_ns: samples.last().copied().unwrap_or(0),
            over_warn,
            over_budget,
            budget_ns,
        }
    }

    /// Worst sample as a fraction of the budget: the number criterion 4 is stated in.
    pub fn worst_ratio(&self) -> f64 {
        if self.budget_ns == 0 {
            return 0.0;
        }
        self.max_ns as f64 / self.budget_ns as f64
    }

    /// The honest label for these numbers: they come from `std::time::Instant` taken
    /// outside-to-outside around a chunk, on the driver thread.
    pub const fn label() -> &'static str {
        "wallclock µs, std::time::Instant outside-to-outside on the driver thread \
         (NOT CPU cycles; includes the timer read itself)"
    }
}

impl fmt::Display for CostSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "n={} mean={:.2}µs median={:.2}µs p99={:.2}µs max={:.2}µs over0.40x={} over1.00x={}",
            self.samples,
            self.mean_ns / 1e3,
            self.median_ns / 1e3,
            self.p99_ns / 1e3,
            self.max_ns as f64 / 1e3,
            self.over_warn,
            self.over_budget,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_of_64_frames_at_48k_is_the_core_constant() {
        // BUDGET_64_48K_US = 1333.33 µs; integer ns truncates to 1333333.
        assert_eq!(budget_ns(64, 48_000), 1_333_333);
        assert_eq!(budget_ns(128, 48_000), 2_666_666);
        assert_eq!(budget_ns(64, 44_100), 1_451_247);
        assert!(
            (crate::cost_summary::budget_ns(64, 48_000) as f64 / 1e3
                - hathor_dsp_core::BUDGET_64_48K_US)
                .abs()
                < 0.001,
            "the probe's budget and the core's constant must be the same number"
        );
    }

    #[test]
    fn a_zero_rate_device_does_not_divide_by_zero() {
        assert_eq!(budget_ns(64, 0), 64_000_000_000 / 1, "clamped, not a panic");
    }

    #[test]
    fn warn_threshold_is_40_percent_of_budget() {
        assert_eq!(warn_ns(1_000), 400);
        assert_eq!(warn_ns(budget_ns(64, 48_000)), 533_333);
    }

    #[test]
    fn percentile_is_nearest_rank_not_interpolated() {
        let mut v: Vec<u64> = (1..=100).collect();
        v.sort_unstable();
        assert_eq!(percentile(&v, 0.50), 50.0, "median of 1..=100 → rank 50");
        assert_eq!(percentile(&v, 0.99), 99.0, "nearest rank, not 99.01");
        assert_eq!(percentile(&v, 1.0), 100.0, "p100 is the maximum");
        assert_eq!(percentile(&[], 0.99), 0.0, "empty is a real state, not a panic");
    }

    #[test]
    fn summary_counts_mean_median_max_and_both_thresholds() {
        let budget = 1_000u64;
        let mut s = [100u64, 200, 300, 450, 900, 1_500];
        let c = CostSummary::from_samples(&mut s, budget);
        assert_eq!(c.samples, 6);
        assert_eq!(c.max_ns, 1_500);
        assert_eq!(c.median_ns, 300.0, "rank 3 of the sorted six");
        assert_eq!(c.mean_ns, (100 + 200 + 300 + 450 + 900 + 1_500) as f64 / 6.0);
        assert_eq!(c.over_warn, 2, "450 and 900 are between 400 and 1000");
        assert_eq!(c.over_budget, 1, "only 1500");
        assert!(
            (c.worst_ratio() - 1.5).abs() < 1e-12,
            "1500/1000 = 1.5× budget"
        );
    }

    #[test]
    fn an_empty_sample_set_summarises_as_zeroes_not_a_panic() {
        let c = CostSummary::from_samples(&mut [], budget_ns(64, 48_000));
        assert_eq!(c.samples, 0);
        assert_eq!(c.mean_ns, 0.0);
        assert_eq!(c.max_ns, 0);
        assert_eq!(c.worst_ratio(), 0.0);
    }

    #[test]
    fn over_budget_samples_are_not_double_counted_as_warn() {
        // A deadline miss is one fact, not two: the printout would otherwise add up
        // to more violations than blocks existed.
        let mut s = [2_000u64, 3_000];
        let c = CostSummary::from_samples(&mut s, 1_000);
        assert_eq!(c.over_warn, 0);
        assert_eq!(c.over_budget, 2);
        assert_eq!(c.over_warn + c.over_budget, 2, "each sample counted at most once");
    }

    #[test]
    fn display_is_microseconds_with_two_decimals() {
        let mut s = [1_234_500u64];
        let c = CostSummary::from_samples(&mut s, 1_333_333);
        let t = c.to_string();
        assert!(t.contains("max=1234.50µs"), "{t}");
        assert!(t.contains("n=1"), "{t}");
    }

    #[test]
    fn the_label_admits_it_is_wallclock() {
        assert!(CostSummary::label().contains("NOT CPU cycles"));
        assert!(CostSummary::label().contains("Instant"));
    }
}
