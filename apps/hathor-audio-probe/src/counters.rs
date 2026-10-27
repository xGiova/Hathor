//! What the callback is allowed to touch: atomics, and nothing else.
//!
//! ## Why this shape
//!
//! The realtime contract ([CONVENTION.md](../../../CONVENTION.md) §2) says the
//! callback may not allocate, lock, do I/O or format a string. But it still has to
//! *report*: block counts, the worst block, the allocations the guard saw. So the
//! callback writes into [`Counters`] — plain `AtomicU*`, `Relaxed` except for the
//! stop flag — and the control thread reads a [`CountersView`] after the stream is
//! closed. No `Mutex`, no `Option::map` on a heap type, no `format!` on the hot
//! path: the formatting of these numbers happens in `live_stream.rs`, after the
//! device is free again.
//!
//! ## The float-as-bits trick
//!
//! The level readouts store `f32` through `to_bits()` in an `AtomicU32`. Peak uses
//! `fetch_max`: for **non-negative** floats the IEEE-754 bit pattern is monotonic
//! with the value, so a bit compare is a value compare. RMS just stores the last
//! block's reading (`store`), because "the level at the end of the run" is a
//! sequence, not an extremum. Both are `Relaxed`: a torn reading of a level meter is
//! not a correctness property of this binary, and a lock for it would be worse than
//! the tear.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// The shared counter block. `Send + Sync` by construction: only atomics.
#[derive(Debug)]
pub struct Counters {
    /// Callbacks entered.
    callbacks: AtomicU64,
    /// 64-frame (or clamped) DSP chunks pumped.
    chunks: AtomicU64,
    /// Frames delivered to the driver.
    frames: AtomicU64,
    /// Frames in the smallest callback seen. `u32::MAX` until the first one.
    cb_frames_min: AtomicU32,
    /// Frames in the largest callback seen.
    cb_frames_max: AtomicU32,
    /// Sum of chunk costs, in ns.
    cost_sum_ns: AtomicU64,
    /// Worst chunk cost, in ns.
    cost_max_ns: AtomicU64,
    /// Chunks whose cost crossed the G1 warn threshold (0.40 × budget).
    cost_over_warn: AtomicU64,
    /// Chunks whose cost crossed the budget itself: a deadline miss, whatever the
    /// driver decided to do about it.
    cost_over_budget: AtomicU64,
    /// Allocations the guard saw, summed over callbacks.
    allocs_sum: AtomicU64,
    /// The worst single callback, in allocations.
    allocs_max_per_callback: AtomicU64,
    /// `cpal` error-callback invocations: our proxy for "the driver dropped us".
    stream_errors: AtomicU64,
    /// Callbacks whose render returned an error: a node refused the block, or the
    /// guard refused to open a section. Counted here because the callback may not
    /// format the message — the *label* is one of the fixed strings in
    /// [`crate::pump::node_err`], and the count is what survives the thread boundary.
    render_failures: AtomicU64,
    /// Timing samples the ring could not accept (a consumer that stopped draining).
    ring_drops: AtomicU64,
    /// Real-FFT transforms run.
    fft_runs: AtomicU64,
    /// Transforms skipped while the FFT window was still filling.
    fft_warm_skips: AtomicU64,
    /// Peak output level seen, in dBFS as `f32` bits. Starts at `f32::MIN` bits so a
    /// real (negative) peak can win the CAS; decode with [`Counters::from_bits`].
    peak_bits: AtomicU32,
    /// RMS output level of the last chunk, in dBFS as `f32` bits.
    rms_bits: AtomicU32,
    /// Running Σ|X|² of the analysis bins, as `f64` bits. This is what makes the FFT
    /// a *used* result: without a consumer the release optimiser is entitled to eat
    /// the transform, and a probe that measures an elided FFT is worse than no probe.
    energy_bits: AtomicU64,
    /// FTZ/DAZ was installed on the driver thread (once, at the first callback).
    ftz_set: AtomicBool,
    /// The control thread asked the pump to stop counting.
    stopped: AtomicBool,
}

/// A read of [`Counters`]. `Copy` so the report can hold it without the atomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountersView {
    /// See [`Counters::callbacks`].
    pub callbacks: u64,
    /// See [`Counters::chunks`].
    pub chunks: u64,
    /// See [`Counters::frames`].
    pub frames: u64,
    /// Smallest callback frame count; `u32::MAX` when no callback ran.
    pub cb_frames_min: u32,
    /// See [`Counters::cb_frames_max`].
    pub cb_frames_max: u32,
    /// See [`Counters::cost_sum_ns`].
    pub cost_sum_ns: u64,
    /// See [`Counters::cost_max_ns`].
    pub cost_max_ns: u64,
    /// See [`Counters::cost_over_warn`].
    pub cost_over_warn: u64,
    /// See [`Counters::cost_over_budget`].
    pub cost_over_budget: u64,
    /// See [`Counters::allocs_sum`].
    pub allocs_sum: u64,
    /// See [`Counters::allocs_max_per_callback`].
    pub allocs_max_per_callback: u64,
    /// See [`Counters::stream_errors`].
    pub stream_errors: u64,
    /// See [`Counters::render_failures`].
    pub render_failures: u64,
    /// See [`Counters::ring_drops`].
    pub ring_drops: u64,
    /// See [`Counters::fft_runs`].
    pub fft_runs: u64,
    /// See [`Counters::fft_warm_skips`].
    pub fft_warm_skips: u64,
    /// Peak output level in dBFS as `f32` bits; `f32::NEG_INFINITY` when nothing was
    /// ever heard (decode with [`Counters::from_bits`]).
    pub peak_bits: u32,
    /// Last RMS output level in dBFS, as `f32` bits.
    pub rms_bits: u32,
    /// Running Σ|X|² of the analysis bins, as `f64` bits.
    pub energy_bits: u64,
    /// See [`Counters::ftz_set`].
    pub ftz_set: bool,
}

impl Default for Counters {
    fn default() -> Self {
        Self::new()
    }
}

impl Counters {
    /// Zeroed counters, with the callback-size minimum armed at `u32::MAX`.
    pub fn new() -> Self {
        Self {
            callbacks: AtomicU64::new(0),
            chunks: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            cb_frames_min: AtomicU32::new(u32::MAX),
            cb_frames_max: AtomicU32::new(0),
            cost_sum_ns: AtomicU64::new(0),
            cost_max_ns: AtomicU64::new(0),
            cost_over_warn: AtomicU64::new(0),
            cost_over_budget: AtomicU64::new(0),
            allocs_sum: AtomicU64::new(0),
            allocs_max_per_callback: AtomicU64::new(0),
            stream_errors: AtomicU64::new(0),
            render_failures: AtomicU64::new(0),
            ring_drops: AtomicU64::new(0),
            fft_runs: AtomicU64::new(0),
            fft_warm_skips: AtomicU64::new(0),
            peak_bits: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            rms_bits: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
            energy_bits: AtomicU64::new(0.0f64.to_bits()),
            ftz_set: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }
    }

    /// One callback entered: how many frames the driver actually handed us, and how
    /// many allocations the guard counted inside it.
    pub fn note_callback(&self, frames_in_callback: u32, allocs: u64) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        self.cb_frames_min
            .fetch_min(frames_in_callback, Ordering::Relaxed);
        self.cb_frames_max
            .fetch_max(frames_in_callback, Ordering::Relaxed);
        if allocs > 0 {
            self.allocs_sum.fetch_add(allocs, Ordering::Relaxed);
            self.allocs_max_per_callback
                .fetch_max(allocs, Ordering::Relaxed);
        }
    }

    /// One DSP chunk pumped. `warn_ns`/`budget_ns` are the G1 thresholds for *this*
    /// chunk shape, passed in by the pump so the counters stay policy-free.
    pub fn note_chunk(&self, cost_ns: u64, chunk_frames: u32, warn_ns: u64, budget_ns: u64) {
        self.chunks.fetch_add(1, Ordering::Relaxed);
        self.frames.fetch_add(u64::from(chunk_frames), Ordering::Relaxed);
        self.cost_sum_ns.fetch_add(cost_ns, Ordering::Relaxed);
        self.cost_max_ns.fetch_max(cost_ns, Ordering::Relaxed);
        if cost_ns > budget_ns {
            self.cost_over_budget.fetch_add(1, Ordering::Relaxed);
        } else if cost_ns > warn_ns {
            self.cost_over_warn.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A driver/stream error reached us through cpal's error callback.
    pub fn note_stream_error(&self) {
        self.stream_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// A callback returned an error from [`crate::pump::Pump::render`].
    pub fn note_render_failure(&self) {
        self.render_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// The timing ring was full: the sample was lost, and that is a fact about the
    /// control thread, not the audio thread.
    pub fn note_ring_drop(&self) {
        self.ring_drops.fetch_add(1, Ordering::Relaxed);
    }

    /// A transform ran / was skipped because the window was still filling.
    pub fn note_fft(&self, ran: bool) {
        let c = if ran { &self.fft_runs } else { &self.fft_warm_skips };
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// Output level of the chunk, in dBFS: peak is an extremum, rms is a sequence.
    ///
    /// Peak needs a *maximum*, and `AtomicU32::fetch_max` on float bits is wrong for
    /// negative values (the sign bit makes −8 dBFS look smaller than −80 dBFS), so
    /// this is the one CAS loop in the callback. It is uncontended by construction —
    /// a single producer per CONVENTION §2.3 — and costs one `lock cmpxchg`
    /// (expected ~20 cycles), which is why the probe accepts it and a plugin would
    /// not put a lock here.
    pub fn note_levels(&self, peak_db: f32, rms_db: f32) {
        let mut cur = self.peak_bits.load(Ordering::Relaxed);
        loop {
            let candidate = f32::from_bits(cur);
            // `!(peak_db > candidate)` also rejects a NaN candidate and a NaN reading:
            // a corrupt level never overwrites a good one, and the corrupt run still
            // shows up in the block counters.
            if !(peak_db > candidate) {
                break;
            }
            match self.peak_bits.compare_exchange_weak(
                cur,
                peak_db.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => cur = seen,
            }
        }
        self.rms_bits.store(rms_db.to_bits(), Ordering::Relaxed);
    }

    /// Stores the running Σ|X|² of the analysis bins. See [`Counters::energy_bits`]:
    /// this is the consumer that keeps the transform out of the optimiser's reach.
    pub fn note_energy(&self, energy: f64) {
        self.energy_bits.store(energy.to_bits(), Ordering::Relaxed);
    }

    /// Marks FTZ/DAZ as installed on the driver thread.
    pub fn note_ftz_installed(&self) {
        self.ftz_set.store(true, Ordering::Relaxed);
    }

    /// Asks the pump to stop (Acquire/Release pair with [`Counters::stopped`]).
    pub fn request_stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// The pump reads this once per callback: a relaxed load is enough because a
    /// missed-by-one-block stop is not a correctness property.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Snapshot for the report. Call it **after** the stream is dropped, or the
    /// numbers are a mid-run photograph of a moving car.
    pub fn view(&self) -> CountersView {
        CountersView {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            chunks: self.chunks.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            cb_frames_min: self.cb_frames_min.load(Ordering::Relaxed),
            cb_frames_max: self.cb_frames_max.load(Ordering::Relaxed),
            cost_sum_ns: self.cost_sum_ns.load(Ordering::Relaxed),
            cost_max_ns: self.cost_max_ns.load(Ordering::Relaxed),
            cost_over_warn: self.cost_over_warn.load(Ordering::Relaxed),
            cost_over_budget: self.cost_over_budget.load(Ordering::Relaxed),
            allocs_sum: self.allocs_sum.load(Ordering::Relaxed),
            allocs_max_per_callback: self.allocs_max_per_callback.load(Ordering::Relaxed),
            stream_errors: self.stream_errors.load(Ordering::Relaxed),
            render_failures: self.render_failures.load(Ordering::Relaxed),
            ring_drops: self.ring_drops.load(Ordering::Relaxed),
            fft_runs: self.fft_runs.load(Ordering::Relaxed),
            fft_warm_skips: self.fft_warm_skips.load(Ordering::Relaxed),
            peak_bits: self.peak_bits.load(Ordering::Relaxed),
            rms_bits: self.rms_bits.load(Ordering::Relaxed),
            energy_bits: self.energy_bits.load(Ordering::Relaxed),
            ftz_set: self.ftz_set.load(Ordering::Relaxed),
        }
    }

    /// Decodes a stored dBFS level.
    pub fn from_bits(bits: u32) -> f32 {
        f32::from_bits(bits)
    }

    /// Decodes the stored analysis energy (Σ|X|² of the bins).
    pub fn energy_from_bits(bits: u64) -> f64 {
        f64::from_bits(bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_counters_read_as_nothing_happened() {
        let c = Counters::new();
        let v = c.view();
        assert_eq!(v.callbacks, 0);
        assert_eq!(v.chunks, 0);
        assert_eq!(
            v.cb_frames_min,
            u32::MAX,
            "an unarmed minimum must not masquerade as a 0-frame callback"
        );
        assert_eq!(
            Counters::from_bits(v.peak_bits),
            f32::NEG_INFINITY,
            "nothing heard yet: −inf dBFS, not 0 dBFS"
        );
        assert_eq!(Counters::energy_from_bits(v.energy_bits), 0.0, "no transform ran");
        assert!(!v.ftz_set, "nothing installed FTZ yet");
    }

    #[test]
    fn callback_sizes_keep_the_true_min_and_max() {
        let c = Counters::new();
        for f in [480u32, 64, 96, 480, 1] {
            c.note_callback(f, 0);
        }
        let v = c.view();
        assert_eq!(v.cb_frames_min, 1, "the odd 1-frame callback must be visible");
        assert_eq!(v.cb_frames_max, 480);
        assert_eq!(v.callbacks, 5);
    }

    #[test]
    fn allocations_are_summed_and_the_worst_callback_is_kept() {
        let c = Counters::new();
        c.note_callback(64, 3);
        c.note_callback(64, 0);
        c.note_callback(64, 5);
        let v = c.view();
        assert_eq!(v.allocs_sum, 8, "total across callbacks");
        assert_eq!(v.allocs_max_per_callback, 5, "worst single callback");
    }

    #[test]
    fn a_clean_run_records_exactly_zero_allocations() {
        let c = Counters::new();
        for _ in 0..100 {
            c.note_callback(64, 0);
        }
        assert_eq!(c.view().allocs_sum, 0, "0 is the only passing number");
    }

    #[test]
    fn chunk_cost_classifies_against_both_thresholds() {
        let c = Counters::new();
        let (warn, budget) = (400u64, 1000u64);
        c.note_chunk(100, 64, warn, budget); // under warn
        c.note_chunk(500, 64, warn, budget); // over warn, under budget
        c.note_chunk(5_000, 64, warn, budget); // over budget
        let v = c.view();
        assert_eq!(v.chunks, 3);
        assert_eq!(v.frames, 192, "3 chunks × 64 frames");
        assert_eq!(v.cost_max_ns, 5_000);
        assert_eq!(v.cost_sum_ns, 5_600);
        assert_eq!(v.cost_over_warn, 1, "the 500 ns block, and only it");
        assert_eq!(v.cost_over_budget, 1, "a deadline miss is not a warn");
    }

    #[test]
    fn threshold_boundary_is_not_counted_as_a_violation() {
        let c = Counters::new();
        c.note_chunk(1_000, 64, 400, 1_000);
        assert_eq!(c.view().cost_over_budget, 0, "exactly at budget is not over it");
    }

    #[test]
    fn peak_is_a_running_maximum_and_rms_the_last_reading() {
        let c = Counters::new();
        c.note_levels(-40.0, -50.0);
        c.note_levels(-20.0, -30.0);
        c.note_levels(-25.0, -45.0);
        let v = c.view();
        assert_eq!(
            Counters::from_bits(v.peak_bits),
            -20.0,
            "max over callbacks, in dBFS: −20 beats −25 and −40"
        );
        assert_eq!(
            Counters::from_bits(v.rms_bits),
            -45.0,
            "rms is a sequence: the newest reading wins"
        );
    }

    #[test]
    fn silence_never_overwrites_a_real_peak_and_nan_is_dropped() {
        let c = Counters::new();
        c.note_levels(-12.0, -20.0);
        c.note_levels(f32::NEG_INFINITY, f32::NEG_INFINITY);
        assert_eq!(
            Counters::from_bits(c.view().peak_bits),
            -12.0,
            "−inf is not louder than −12"
        );
        // A NaN peak must not poison the reading either: `!(nan > x)` is true, so it
        // falls out of the CAS loop without being stored.
        c.note_levels(f32::NAN, f32::NAN);
        assert_eq!(Counters::from_bits(c.view().peak_bits), -12.0);
    }

    #[test]
    fn errors_drops_and_ffts_are_all_countable() {
        let c = Counters::new();
        c.note_stream_error();
        c.note_stream_error();
        c.note_ring_drop();
        c.note_fft(true);
        c.note_fft(true);
        c.note_fft(false);
        let v = c.view();
        assert_eq!(v.stream_errors, 2, "two error callbacks is not 'none'");
        assert_eq!(v.ring_drops, 1);
        assert_eq!(v.fft_runs, 2);
        assert_eq!(v.fft_warm_skips, 1);
    }

    #[test]
    fn stop_is_visible_across_the_boundary() {
        let c = Counters::new();
        assert!(!c.stopped());
        c.request_stop();
        assert!(c.stopped(), "Release/Acquire: the pump must see the stop");
    }

    #[test]
    fn ftz_install_is_reported_once_and_sticky() {
        let c = Counters::new();
        c.note_ftz_installed();
        assert!(c.view().ftz_set);
    }

    #[test]
    fn counters_are_send_and_sync_because_they_are_only_atomics() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Counters>();
    }
}
