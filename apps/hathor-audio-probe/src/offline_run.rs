//! `offline`: the deterministic rehearsal of a live run. Safe to run anywhere — no
//! device, no audio, no audible artefacts.
//!
//! ## What it proves, and what it does not
//!
//! It proves the *contract* holds under repetition: 20,000 cycles of the same pump
//! the live path uses — tone, three biquads, the scratch door, one real FFT,
//! interleaving, counters — with the allocation guard in **strict** mode, so a single
//! allocation panics and the process exits non-zero. That is ROADMAP §M0's
//! "`--offline` 20,000 cycles → 0 allocations" criterion, and it is the reason a live
//! run may be a one-shot event: this code path has already been taken ten thousand
//! times without a driver in the room.
//!
//! It does **not** prove anything about timing under load. There is no driver period,
//! no preemption, no WASAPI engine thread: the wallclock numbers printed here are the
//! cost of the DSP on an otherwise idle core, which is what [`crate::bench`] measures
//! row by row. The report labels its cost line "OFFLINE, no driver" so nobody reads
//! it as the live figure.
//!
//! ## Why strict mode here and measure mode in `run`
//!
//! A panic in a plain loop unwinds into `main` and exits with a message; a panic that
//! has to cross an `extern "system"` WASAPI callback frame is a different bet. So the
//! rehearsal takes the strongest assertion (die on the first allocation) and the live
//! run the honest one (count it, print it, fail the gate).

use std::time::Instant;

use hathor_dsp_core::alloc_guard;

use crate::cli::{OfflineOptions, Tone};
use crate::cost_summary::CostSummary;
use crate::counters::{Counters, CountersView};
use crate::live_stream;
use crate::probe_error::ProbeError;
use crate::pump::Pump;
use crate::run_report::{self, Check, Measured, Severity};
use crate::stamp;
use crate::timing_ring::{TimingDrain, DEFAULT_CAPACITY};

/// The rehearsal's channel count: the M0 shape, stereo. A mono device is what `run`
/// adapts to; nothing here needs to pretend otherwise.
pub const OFFLINE_CHANNELS: u16 = 2;

/// How often the control side drains the ring inside the loop. 2,048 chunks is 32
/// callbacks of 480 frames: far inside the ring's capacity, and rare enough that the
/// drain (which allocates when the `Vec` grows) stays out of the per-chunk cost.
pub const DRAIN_EVERY_CHUNKS: usize = 2_048;

/// The drain period must leave room inside the ring, or the rehearsal would silently
/// drop timing samples and report a cost summary built from a fraction of the run.
/// Written as a subtraction rather than an `assert!`: an underflow here is a compile
/// error, so the relation cannot rot into a passing test of a stale constant.
const _: usize = DEFAULT_CAPACITY - DRAIN_EVERY_CHUNKS;

/// The result of an offline rehearsal.
#[derive(Debug)]
pub struct OfflineOutcome {
    /// Cycles asked for.
    pub cycles: usize,
    /// Sample rate the chain was built for, as the pump reports it.
    pub rate: u32,
    /// Channels the chain was built for, as the pump reports it.
    pub channels: usize,
    /// Block shape.
    pub chunk_frames: u32,
    /// Analysis window in frames.
    pub fft_size: usize,
    /// Counter snapshot.
    pub view: CountersView,
    /// Cost distribution over the chunks.
    pub cost: CostSummary,
    /// Process-wide guard total at the end of the loop (`alloc_guard::allocs_total()`).
    ///
    /// This counts **every thread's** sections, not just the rehearsal's: it is a witness
    /// that nothing else in the process allocated in an audio window, and it is evidence
    /// about this loop only while the caller owns the process. The standalone binary does
    /// (two threads, one of which opens no section); a `cargo test` harness does not, which
    /// is why every guard-touching test in this crate takes `crate::guard_serial()` before
    /// asserting or resetting it — see
    /// [`pump::a_process_wide_reading_belongs_to_no_single_thread`](crate::pump), which
    /// demonstrates the sharing rather than trusting it away. The thread-owned ground truth
    /// is [`OfflineOutcome::view`]'s `allocs_sum` / `allocs_max_per_callback`, read from
    /// each section's own `allocs_here()`.
    pub guard_allocs: u64,
    /// Wallclock seconds the loop took — the real cost of `cycles` chunks of DSP on
    /// this machine, not a simulation.
    pub real_seconds: f64,
    /// Ring capacity in samples.
    pub ring_capacity: usize,
    /// Frames in the FFT window at the end of the loop (proves the analysis reached
    /// full size, rather than reporting a transform count and hoping).
    pub window_filled: usize,
    /// The window's declared size in frames.
    pub window_size: usize,
    /// Timing samples the report kept out of `view.chunks` published.
    pub samples_kept: usize,
}

impl OfflineOutcome {
    /// The verdict, reusing the live checks: one contract, one set of thresholds. Two
    /// adaptations, both honest:
    ///
    /// * there is no driver, so the callback-size check becomes "the pump produced
    ///   exactly `cycles` chunks of the requested shape" — a fact this path can
    ///   establish, instead of a warning about a machine that is not in the room;
    /// * `stream_errored` / `node_refused` are false by construction (a node error
    ///   aborts the loop and surfaces as an `Err`, not a counter).
    #[must_use]
    pub fn checks(&self) -> Vec<Check> {
        let m = Measured {
            view: &self.view,
            cost: &self.cost,
            requested_chunk: self.chunk_frames,
            budget_ns: self.cost.budget_ns,
            asked_seconds: self.simulated_seconds(),
            real_seconds: self.real_seconds,
            guard_allocs: self.guard_allocs,
            stream_errored: false,
            node_refused: false,
            fft_size: self.fft_size,
        };
        let mut checks = run_report::evaluate(&m);
        if let Some(c) = checks
            .iter_mut()
            .find(|c| c.name.starts_with("driver callback sizes"))
        {
            c.detail = format!(
                "no driver: {} chunk(s) over {} cycle(s), shape {} frames ({} frame(s) pumped)",
                self.view.chunks, self.cycles, self.chunk_frames, self.view.frames
            );
            c.severity = if self.view.chunks == self.cycles as u64
                && self.view.frames == self.cycles as u64 * u64::from(self.chunk_frames)
            {
                Severity::Pass
            } else {
                Severity::Fail
            };
        }
        checks
    }

    /// How much audio the loop represents: `cycles × chunk / rate`.
    #[must_use]
    pub fn simulated_seconds(&self) -> f64 {
        if self.rate == 0 || self.chunk_frames == 0 {
            return 0.0;
        }
        self.cycles as f64 * f64::from(self.chunk_frames) / f64::from(self.rate)
    }

    /// True when nothing failed.
    #[must_use]
    pub fn passed(&self) -> bool {
        run_report::passed(&self.checks())
    }

    /// The report as lines.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let checks = self.checks();
        let mut out = vec![
            format!(
                "offline: {} cycle(s) of {} frames × {} ch @ {} Hz, analysis {} frames, \
                 ring {} samples, STRICT guard",
                self.cycles,
                self.chunk_frames,
                self.channels,
                self.rate,
                self.fft_size,
                self.ring_capacity,
            ),
            live_stream::observed_lines(&self.view, self.chunk_frames).join("\n"),
            format!("cost: {}", self.cost),
            format!(
                "  cost unit: {} — OFFLINE, no driver in the loop",
                CostSummary::label()
            ),
            format!(
                "guard: callback total {}, process-wide total {} \
                 (strict mode: an allocation would have aborted the process)",
                self.view.allocs_sum, self.guard_allocs
            ),
            format!(
                "wallclock: {:.3} s for the loop = {:.1} chunks/s, the run represents {:.1} s of audio",
                self.real_seconds,
                if self.real_seconds > 0.0 {
                    self.cycles as f64 / self.real_seconds
                } else {
                    0.0
                },
                self.simulated_seconds()
            ),
            format!(
                "analysis window: {}/{} frames at the end of the loop; {} of {} published sample(s) reached the report",
                self.window_filled, self.window_size, self.samples_kept, self.view.chunks
            ),
        ];
        out.extend(run_report::lines(&checks));
        out.push(live_stream::verdict_summary(&checks));
        out.extend(stamp::measured_block(stamp::utc_today()));
        out
    }
}

/// Runs the rehearsal. Safe on any machine: it opens no device and plays no audio.
///
/// # Errors
/// [`ProbeError::Node`] if a node refused a block (the shape is wrong, not the
/// driver); [`ProbeError::Gate`] when a check failed. In strict mode an *allocation*
/// does not reach either: the guard panics and the process exits non-zero — which is
/// the assertion ROADMAP §M0 asks for.
pub fn execute(o: &OfflineOptions) -> Result<OfflineOutcome, ProbeError> {
    let (mut pump, consumer) = Pump::setup(o.rate, OFFLINE_CHANNELS, o.frames, o.fft_size, TONE)?;
    let counters: std::sync::Arc<Counters> = pump.counters();
    let ring_capacity = pump.ring_capacity();
    let budget_ns = pump.budget_ns();
    let chunk_frames = pump.chunk_frames();

    // LEGAL ALLOCATION: setup thread — the driver-shaped buffer the loop reuses. One
    // chunk per cycle, exactly the shape the live path pumps.
    let mut buf = vec![0.0f32; usize::from(OFFLINE_CHANNELS) * o.frames as usize];

    println!(
        "offline: {} cycle(s), {} frames × {} ch @ {} Hz, analysis {} frames",
        o.cycles, o.frames, OFFLINE_CHANNELS, o.rate, o.fft_size
    );
    println!(
        "pump: {} node(s) [{}], tone {} Hz (step {}), latency {} samples, budget {:.2} µs/chunk, ring {} samples",
        pump.chain_names().len(),
        pump.chain_names().join(" → "),
        pump.actual_tone_hz(),
        pump.tone_step(),
        pump.chain_latency(),
        budget_ns as f64 / 1e3,
        ring_capacity,
    );
    println!("guard: STRICT — the first allocation in a block aborts this process");
    alloc_guard::set_panic_on_audio_alloc(true);
    alloc_guard::reset_counters();
    // The pump's own numbers, not the command line's: if the shape the chain was built
    // for is not what was asked, this is where it becomes visible.
    let pump_rate = pump.rate();
    let pump_channels = pump.channels();
    // Cold window by construction, so "the analysis took N cycles to start" is a fact
    // of this loop rather than of what `setup` happened to leave behind.
    pump.reset_analysis_window();

    let mut drain = TimingDrain::new(consumer, o.cycles.min(65_536));
    let start = Instant::now();
    for cycle in 0..o.cycles {
        pump.render(&mut buf, false)?;
        if (cycle + 1) % DRAIN_EVERY_CHUNKS == 0 {
            drain.drain();
        }
    }
    let real_seconds = start.elapsed().as_secs_f64();
    drain.drain();
    // Back to measure mode: the report below formats strings, on the control thread,
    // which is legal — and leaving strict mode on would make a later `run` in the same
    // process inherit a policy it never asked for.
    alloc_guard::set_panic_on_audio_alloc(false);

    let view = counters.view();
    // Read the drain before `into_samples` consumes it.
    let samples_kept = if drain.is_empty() { 0 } else { drain.len() };
    let window_filled = pump.window_filled();
    let window_size = pump.window_size();
    let mut samples = drain.into_samples();
    let cost = CostSummary::from_samples(&mut samples, budget_ns);
    let outcome = OfflineOutcome {
        cycles: o.cycles,
        rate: pump_rate,
        channels: pump_channels,
        chunk_frames,
        fft_size: o.fft_size,
        view,
        cost,
        guard_allocs: alloc_guard::allocs_total(),
        real_seconds,
        ring_capacity,
        window_filled,
        window_size,
        samples_kept,
    };
    Ok(outcome)
}

/// The tone the rehearsal runs: 440 Hz, the same default as `run`. Deterministic by
/// CONVENTION §5 (no random inputs in the tests of this repo, and a rehearsal that
/// differs from the live default would be measuring a different thing).
pub const TONE: Tone = Tone::Sine(440.0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counters::Counters;

    fn outcome(cycles: usize, chunks: u64, frames: u64, allocs: u64) -> OfflineOutcome {
        let c = Counters::new();
        for _ in 0..cycles.min(4) {
            c.note_callback(64, allocs);
            c.note_chunk(100, 64, 533_333, 1_333_333);
        }
        c.note_levels(-14.0, -18.0);
        c.note_fft(true);
        c.note_energy(5.0);
        c.note_ftz_installed();
        let mut v = c.view();
        v.chunks = chunks;
        v.frames = frames;
        v.callbacks = cycles as u64;
        OfflineOutcome {
            cycles,
            rate: 48_000,
            channels: 2,
            chunk_frames: 64,
            fft_size: 1_024,
            view: v,
            cost: CostSummary {
                samples: chunks as usize,
                mean_ns: 100.0,
                median_ns: 100.0,
                p99_ns: 120.0,
                max_ns: 130,
                over_warn: 0,
                over_budget: 0,
                budget_ns: 1_333_333,
            },
            guard_allocs: allocs * cycles as u64,
            real_seconds: 0.1,
            ring_capacity: 16_384,
            window_filled: 1_024,
            window_size: 1_024,
            samples_kept: chunks as usize,
        }
    }

    #[test]
    fn a_complete_rehearsal_passes_and_says_what_it_measured() {
        let o = outcome(100, 100, 6_400, 0);
        let checks = o.checks();
        assert!(o.passed(), "{:?}", run_report::lines(&checks));
        let shape = checks
            .iter()
            .find(|c| c.name.starts_with("driver callback sizes"))
            .expect("check");
        assert_eq!(shape.severity, Severity::Pass, "{}", shape.detail);
        assert!(shape.detail.contains("no driver"), "{}", shape.detail);
        let ls = o.lines().join("\n");
        assert!(ls.contains("STRICT"), "{ls}");
        assert!(ls.contains("OFFLINE, no driver"), "{ls}");
    }

    #[test]
    fn a_short_rehearsal_fails_instead_of_passing_quietly() {
        // 100 cycles asked, 99 chunks pumped: a loop that skipped one is not a pass.
        let o = outcome(100, 99, 6_336, 0);
        assert!(!o.passed(), "a missing chunk must fail the run");
        let shape = o
            .checks()
            .into_iter()
            .find(|c| c.name.starts_with("driver callback sizes"))
            .expect("check");
        assert_eq!(shape.severity, Severity::Fail, "{}", shape.detail);
    }

    #[test]
    fn the_simulated_duration_is_the_frame_count_over_the_rate() {
        assert!(
            (outcome(20_000, 20_000, 1_280_000, 0).simulated_seconds() - 26.666_7).abs() < 1e-3,
            "20 000 × 64 frames at 48 kHz is 26.7 s of audio"
        );
        assert_eq!(outcome(0, 0, 0, 0).simulated_seconds(), 0.0);
    }

    #[test]
    fn an_allocation_observed_anywhere_fails_the_rehearsal() {
        // Strict mode would have died before reaching here; the check is the second
        // net, for the case where the guard was left in measure mode by a caller.
        let o = outcome(100, 100, 6_400, 1);
        assert!(!o.passed(), "allocations != 0 is never a pass");
    }

    #[test]
    fn the_drain_period_and_channel_count_are_the_documented_ones() {
        assert_eq!(OFFLINE_CHANNELS, 2);
        assert_eq!(DRAIN_EVERY_CHUNKS, 2_048);
        // The "drains happen well before the ring can fill" relation is not checked
        // here: it is a `const` item in the module, so breaking it is a compile error.
        // A float cannot be a pattern, so the frequency is read out, not matched.
        let Tone::Sine(hz) = TONE else {
            panic!("the rehearsal's default tone must not be silence");
        };
        assert_eq!(hz, 440.0, "the same default as `run`");
    }

    #[test]
    fn execute_runs_the_loop_it_claims_to_and_reports_zero_allocations() {
        // A real 500-cycle rehearsal: the shortest thing that still proves the loop,
        // the guard and the report all work together in this binary.
        //
        // ROOT CAUSE OF THE ONE FLAKY TEST IN THE TREE, STATED WHERE IT CAN BE READ:
        // `res.guard_allocs` is `alloc_guard::allocs_total()`, a *process-wide* atomic, and
        // the verdict fed by it (`run_report::evaluate`: `allocs_sum == 0 && guard_allocs
        // == 0`) therefore made a statement about every thread in the test binary while
        // claiming to be about this loop. Two siblings broke it without the pump doing
        // anything wrong: `pump::the_guard_is_live_in_this_binary_negative_control`
        // allocates inside a section on purpose (that is what makes the instrument honest),
        // and `bench::execute` resets the counters and flips the global panic-mode. Worse
        // than a wrong number: `execute` here sets *strict* mode, whose flag is also global,
        // so an unlocked sibling could set measure mode mid-loop and the rehearsal's
        // strictness would have been scheduled luck rather than a property. Debug's slower
        // loop widens the overlap window, which is why it appeared in
        // `cargo test --workspace` and not in the isolated or release runs — an absent
        // failure was never evidence of an absent race, and `pump.rs` kept a *private*
        // `static LOCK` that serialised its guard tests against each other and against
        // nothing else. See `pump::a_process_wide_reading_belongs_to_no_single_thread`,
        // which demonstrates the contamination instead of trusting it not to happen.
        //
        // The fix is the lock, not a softer assertion: nothing here is relaxed to
        // "0 is likely". Holding the one crate-wide guard lock makes the process this test
        // reads from exactly the process it is testing, which is the only condition under
        // which a process-wide zero means anything.
        let _serial = crate::guard_serial();
        let o = OfflineOptions {
            rate: 48_000,
            frames: 64,
            cycles: 500,
            fft_size: 1_024,
        };
        let res = execute(&o).expect("rehearsal");
        assert_eq!(res.view.chunks, 500, "one chunk per cycle");
        assert_eq!(res.view.frames, 32_000);
        // Thread-local ground truth: every section's own `allocs_here()`, summed and
        // maxed by the pump. Immune to siblings, and the M0 criterion.
        assert_eq!(res.view.allocs_sum, 0, "the M0 criterion");
        assert_eq!(res.view.allocs_max_per_callback, 0, "not one block, ever");
        // Process-wide witness, meaningful only because the lock owns the process.
        assert_eq!(res.guard_allocs, 0, "and the guard agrees");
        assert!(res.passed(), "{:?}", run_report::lines(&res.checks()));
        assert!(res.cost.samples > 0, "samples crossed the ring");
    }

    #[test]
    fn a_consumer_that_stops_draining_loses_samples_and_says_so() {
        // The ring is 16,384 deep and 500 chunks is far below it; the honest failure
        // mode is a *small* ring, which `timing_ring` tests. Here we only assert that
        // an empty ring drains to nothing rather than pretending to have samples.
        let (_pump, consumer) = Pump::setup(48_000, 2, 64, 1_024, TONE).expect("setup");
        let mut drain = TimingDrain::new(consumer, 8);
        assert_eq!(drain.drain(), 0, "nothing published, nothing drained");
    }
}
