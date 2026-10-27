//! The verdict: measured numbers → `ok`/`warn`/`FAIL` lines, decided by pure code.
//!
//! ## Why the checks are separated from the printing
//!
//! `run` happens once, in front of a human, and its output is the G1 artefact. If the
//! pass/fail logic lived inside the print loop it would be tested by *looking at it*,
//! which is how "0 xruns" ends up printed next to a non-zero counter. So
//! [`evaluate`] turns the numbers into [`Check`]s, and `lines` merely renders them;
//! the tests here feed counter values by hand — including the ones that must FAIL —
//! without a sound card or a second of live audio.
//!
//! ## Severity, and why some mismatches are warnings
//!
//! * **Fail** — the contract broke: an allocation was observed, a node refused a
//!   block, a deadline was missed, the run produced nothing.
//! * **Warn** — the *machine* differed from the request in a way the probe cannot
//!   control and must report rather than hide. The canonical case: shared-mode WASAPI
//!   delivers the audio engine's period (~480 frames at 48 kHz), not the 64 that was
//!   requested. The DSP still ran in 64-frame chunks, so the measurement is still the
//!   one M0 asked for — but a run that never saw a 64-frame callback must not be
//!   silently labelled "block size 64". That is a finding, and findings are warnings.
//! * **Ok** — measured, as requested.
//!
//! There is no "N/A": a number the probe could not take is a Warn with the reason.

use crate::cost_summary::{warn_ns, CostSummary};
use crate::counters::{Counters, CountersView};

/// How a check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Measured and inside the contract.
    Pass,
    /// True but worth a human's eye: the machine did not do what was asked.
    Warn,
    /// The contract broke. `run` exits non-zero on any of these.
    Fail,
}

/// One line of the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What was checked, in the report's own words.
    pub name: &'static str,
    /// The numbers behind the verdict, formatted (control thread).
    pub detail: String,
    /// How it reads.
    pub severity: Severity,
}

/// Everything the live run produced, in the order it should be judged.
#[derive(Debug, Clone, Copy)]
pub struct Measured<'a> {
    /// Counter snapshot after the stream was closed.
    pub view: &'a CountersView,
    /// Chunk-cost distribution (wallclock ns).
    pub cost: &'a CostSummary,
    /// Block size the DSP was asked to pump (the G1 shape).
    pub requested_chunk: u32,
    /// The chain's own budget for that shape, ns.
    pub budget_ns: u64,
    /// Seconds the run was asked to last.
    pub asked_seconds: f64,
    /// Seconds the run actually lasted, wallclock.
    pub real_seconds: f64,
    /// The process-wide guard total (independent of the pump's own read).
    pub guard_allocs: u64,
    /// Whether cpal reported a stream error at least once.
    pub stream_errored: bool,
    /// A node refused a block.
    pub node_refused: bool,
    /// The FFT window size the analysis ran at, frames.
    pub fft_size: usize,
}

impl Measured<'_> {
    /// Frames per second the run should have delivered, from the budget and the
    /// asked duration: the denominator of the "delivered audio" check. Derived from
    /// `budget_ns` (which is `frames/rate`) so it needs no rate parameter, and from
    /// the *asked* seconds so a run that stopped early is visible as a shortfall.
    fn expected_frames(&self) -> f64 {
        let blocks = self.asked_seconds * 1e9 / self.budget_ns.max(1) as f64;
        blocks * f64::from(self.requested_chunk)
    }
}

/// Turns measurements into checks. Pure: every branch here is reachable in a test.
#[must_use]
pub fn evaluate(m: &Measured<'_>) -> Vec<Check> {
    let v = m.view;
    let mut c = Vec::new();

    c.push(Check {
        name: "allocations observed in the callback",
        detail: format!(
            "pump saw {} (max {} in one callback), guard total {}",
            v.allocs_sum, v.allocs_max_per_callback, m.guard_allocs
        ),
        severity: if v.allocs_sum == 0 && m.guard_allocs == 0 {
            Severity::Pass
        } else {
            Severity::Fail
        },
    });

    c.push(Check {
        name: "cpal stream errors (xrun proxy)",
        detail: format!(
            "{} error callback(s); {} chunk(s) over budget, {} over 0.40× budget \
             — WASAPI shared mode exposes no xrun counter, so these are the proxy",
            v.stream_errors, v.cost_over_budget, v.cost_over_warn
        ),
        // One condition, both reasons: a driver error and a chunk that overran the
        // whole budget are the same verdict here, and writing them as separate arms
        // only hides that they do the same thing.
        severity: if m.stream_errored || v.stream_errors > 0 || v.cost_over_budget > 0 {
            Severity::Fail
        } else {
            Severity::Pass
        },
    });

    c.push(Check {
        name: "a node refused the block",
        detail: if m.node_refused {
            "the chain returned an error: the shape guard fired".to_string()
        } else {
            "every chunk was accepted".to_string()
        },
        severity: if m.node_refused {
            Severity::Fail
        } else {
            Severity::Pass
        },
    });

    c.push(Check {
        name: "driver callback sizes vs the requested block",
        detail: format!(
            "requested {} frames; callbacks delivered {}..={} frames; \
             {} chunk(s) of at most {} pumped inside them",
            m.requested_chunk,
            fmt_min(v.cb_frames_min),
            v.cb_frames_max,
            v.chunks,
            m.requested_chunk,
        ),
        severity: if v.callbacks == 0 {
            Severity::Fail
        } else if v.cb_frames_min == m.requested_chunk && v.cb_frames_max == m.requested_chunk {
            Severity::Pass
        } else {
            // The finding, not an error: shared mode keeps the engine's period.
            Severity::Warn
        },
    });

    c.push(Check {
        name: "worst chunk cost against 0.40× budget",
        detail: format!(
            "max {:.2} µs of {:.2} µs budget ({}), p99 {:.2} µs, mean {:.2} µs — worst is \
             {:.3}× budget; {} chunk(s) over 0.40×, {} over 1.00×",
            m.cost.max_ns as f64 / 1e3,
            m.budget_ns as f64 / 1e3,
            CostSummary::label(),
            m.cost.p99_ns / 1e3,
            m.cost.mean_ns / 1e3,
            // The ratio criterion 4 is stated in, computed from the same numbers the
            // verdict uses rather than restated by hand: 1.0 means a full budget.
            m.cost.worst_ratio(),
            m.cost.over_warn,
            m.cost.over_budget,
        ),
        severity: if m.cost.samples == 0 || m.cost.max_ns > warn_ns(m.budget_ns) {
            // Two ways this row fails: nothing crossed the ring (so there is no
            // evidence at all), or the worst chunk broke the 0.40× criterion.
            Severity::Fail
        } else {
            Severity::Pass
        },
    });

    c.push(Check {
        name: "cost samples",
        detail: format!(
            "{} sample(s) of {} published through the ring; {} lost to a full ring",
            m.cost.samples, v.chunks, v.ring_drops,
        ),
        severity: if v.ring_drops > 0 || m.cost.samples == 0 {
            // Lost samples and no samples at all are the same problem: the summary
            // below is not built from the whole run.
            Severity::Warn
        } else {
            Severity::Pass
        },
    });

    let expected_frames = m.expected_frames();
    let got = v.frames as f64;
    let ratio = if expected_frames > 0.0 {
        got / expected_frames
    } else {
        0.0
    };
    c.push(Check {
        name: "delivered audio vs the asked duration",
        detail: format!(
            "{:.1} s asked, {:.1} s real: {} frames pumped, expected ~{} ({:.1}% of it)",
            m.asked_seconds,
            m.real_seconds,
            v.frames,
            expected_frames as u64,
            ratio * 100.0,
        ),
        severity: if v.callbacks == 0 || v.frames == 0 {
            Severity::Fail
        } else if !(0.5..=1.5).contains(&ratio) {
            Severity::Warn
        } else {
            Severity::Pass
        },
    });

    let peak = CountersView::from_bits_opt(v.peak_bits);
    c.push(Check {
        name: "output level",
        detail: format!(
            "peak {} dBFS, last chunk rms {} dBFS (the chain is audible and measurable)",
            fmt_db(peak),
            fmt_db(CountersView::from_bits_opt(v.rms_bits)),
        ),
        severity: if v.chunks == 0 {
            Severity::Fail
        } else if !peak.is_some_and(|p| p <= 0.0) {
            // Either no level was ever recorded, or something peaked above full
            // scale. Both mean this row cannot be offered as evidence the chain was
            // audible — a missing reading is not a silent one, and a clipped one is
            // not a clean one.
            Severity::Warn
        } else {
            Severity::Pass
        },
    });

    let energy = CountersView::energy_from_bits_opt(v.energy_bits);
    c.push(Check {
        name: "analysis ran on the scratch door",
        detail: format!(
            "{} transform(s) of {} frame(s), {} skipped while the window filled, \
             Σ|X|² = {}",
            v.fft_runs,
            m.fft_size,
            v.fft_warm_skips,
            energy.map_or_else(|| "none".to_string(), |e| format!("{e:.1}")),
        ),
        severity: if v.fft_runs == 0 {
            Severity::Warn
        } else if energy.is_none() {
            Severity::Fail
        } else {
            Severity::Pass
        },
    });

    c.push(Check {
        name: "denormals",
        detail: if v.ftz_set {
            "FTZ/DAZ installed on the driver thread".to_string()
        } else {
            "FTZ/DAZ NOT installed: block cost may include denormal punishment".to_string()
        },
        severity: if v.ftz_set {
            Severity::Pass
        } else {
            Severity::Warn
        },
    });

    c
}

/// `u32::MAX` means "no callback ever ran": print that instead of 4294967295.
fn fmt_min(min: u32) -> String {
    if min == u32::MAX {
        "none".to_string()
    } else {
        min.to_string()
    }
}

/// `None` is "never measured", which is not the same as 0 dBFS.
fn fmt_db(db: Option<f32>) -> String {
    db.map_or_else(|| "−inf (nothing heard)".to_string(), |d| format!("{d:.1}"))
}

impl CountersView {
    /// The peak/rms fields as real options: `NEG_INFINITY` is *silence*, `NaN` is a
    /// broken measurement, and the report must not conflate them. The decode itself
    /// lives in [`crate::counters::Counters::from_bits`] — one place says what the
    /// bits mean, here we only decide what to do about silence.
    pub fn from_bits_opt(bits: u32) -> Option<f32> {
        let v = Counters::from_bits(bits);
        if v.is_nan() || v == f32::NEG_INFINITY {
            None
        } else {
            Some(v)
        }
    }

    /// The energy field as an option: non-finite means a bin went non-finite.
    pub fn energy_from_bits_opt(bits: u64) -> Option<f64> {
        let v = Counters::energy_from_bits(bits);
        v.is_finite().then_some(v)
    }
}

/// True when nothing failed. Warnings do not fail a run: they are the machine's
/// answer, and the human reading the report is the one who decides.
#[must_use]
pub fn passed(checks: &[Check]) -> bool {
    !checks.iter().any(|c| c.severity == Severity::Fail)
}

/// Renders the verdict, one line per check, in the report's own column layout.
#[must_use]
pub fn lines(checks: &[Check]) -> Vec<String> {
    checks
        .iter()
        .map(|c| {
            format!(
                "{:<5} {:<44} {}",
                match c.severity {
                    Severity::Pass => "ok",
                    Severity::Warn => "warn",
                    Severity::Fail => "FAIL",
                },
                c.name,
                c.detail,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counters::Counters;

    /// A counters view where everything is exactly as a clean 5-second live run:
    /// 64-frame callbacks, zero allocations, no errors, real levels, transform ran.
    fn clean() -> CountersView {
        let c = Counters::new();
        c.note_callback(64, 0);
        c.note_chunk(100, 64, warn_ns(1_333_333), 1_333_333);
        c.note_levels(-14.0, -20.0);
        c.note_fft(true);
        c.note_energy(123.0);
        c.note_ftz_installed();
        // One callback of 64 frames is 0.5 s worth? No: the view is built by hand for
        // the checks that read counters, and the duration check gets its own numbers.
        c.view()
    }

    fn cost() -> CostSummary {
        CostSummary {
            samples: 100,
            mean_ns: 90.0,
            median_ns: 95.0,
            p99_ns: 200.0,
            max_ns: 250,
            over_warn: 0,
            over_budget: 0,
            budget_ns: 1_333_333,
        }
    }

    fn measured<'a>(view: &'a CountersView, cost: &'a CostSummary) -> Measured<'a> {
        Measured {
            view,
            cost,
            requested_chunk: 64,
            budget_ns: 1_333_333,
            asked_seconds: 5.0,
            real_seconds: 5.0,
            guard_allocs: 0,
            stream_errored: false,
            node_refused: false,
            fft_size: 1_024,
        }
    }

    #[test]
    fn a_run_that_did_not_happen_fails_instead_of_passing_empty() {
        let v = Counters::new().view();
        let mut cost = cost();
        cost.samples = 0;
        let checks = evaluate(&measured(&v, &cost));
        assert!(!passed(&checks), "empty counters are not a pass");
        assert!(checks
            .iter()
            .any(|c| c.name == "driver callback sizes vs the requested block"
                && c.severity == Severity::Fail));
    }

    #[test]
    fn a_single_clean_callback_passes_the_contract_checks() {
        let v = clean();
        let cost = cost();
        let checks = evaluate(&measured(&v, &cost));
        for name in [
            "allocations observed in the callback",
            "cpal stream errors (xrun proxy)",
            "a node refused the block",
            "driver callback sizes vs the requested block",
            "worst chunk cost against 0.40× budget",
            "analysis ran on the scratch door",
            "denormals",
        ] {
            let c = checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("missing check {name}"));
            assert_eq!(c.severity, Severity::Pass, "{name}: {}", c.detail);
        }
        assert!(passed(&checks));
    }

    #[test]
    fn one_allocation_anywhere_fails_the_run() {
        let c = Counters::new();
        c.note_callback(64, 1);
        c.note_chunk(100, 64, 533_333, 1_333_333);
        c.note_levels(-14.0, -20.0);
        c.note_fft(true);
        c.note_energy(1.0);
        c.note_ftz_installed();
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        let alloc = checks
            .iter()
            .find(|c| c.name.starts_with("allocations"))
            .expect("check");
        assert_eq!(alloc.severity, Severity::Fail, "{}", alloc.detail);
        assert!(!passed(&checks), "allocations != 0 is the G1 gate");
    }

    #[test]
    fn the_process_wide_guard_counter_is_also_a_veto() {
        // The pump saw nothing but the allocator counted one: e.g. a buffer grown by
        // a node we do not own. Either reading failing is enough.
        let v = clean();
        let sample_cost = cost();
        let mut m = measured(&v, &sample_cost);
        m.guard_allocs = 1;
        assert!(!passed(&evaluate(&m)));
    }

    #[test]
    fn a_stream_error_or_an_over_budget_chunk_fails_the_xrun_proxy_check() {
        let c = Counters::new();
        c.note_callback(64, 0);
        c.note_chunk(100, 64, 533_333, 1_333_333);
        c.note_stream_error();
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        let x = checks
            .iter()
            .find(|c| c.name.contains("xrun proxy"))
            .expect("check");
        assert_eq!(x.severity, Severity::Fail, "{}", x.detail);
    }

    #[test]
    fn a_deadline_miss_without_a_driver_error_still_fails() {
        let c = Counters::new();
        c.note_callback(64, 0);
        for _ in 0..3 {
            c.note_chunk(2_000_000, 64, 533_333, 1_333_333);
        }
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        let x = checks
            .iter()
            .find(|c| c.name.contains("xrun proxy"))
            .expect("check");
        assert_eq!(
            x.severity,
            Severity::Fail,
            "over-budget chunks: {}",
            x.detail
        );
        assert!(x.detail.contains("3 chunk(s) over budget"), "{}", x.detail);
    }

    #[test]
    fn a_480_frame_driver_period_is_a_warning_not_a_failure() {
        // The honest WASAPI case: the engine period arrives, the DSP still ran at 64.
        let c = Counters::new();
        c.note_callback(480, 0);
        for _ in 0..8 {
            c.note_chunk(100, 64, 533_333, 1_333_333);
        }
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        let size = checks
            .iter()
            .find(|c| c.name.starts_with("driver callback sizes"))
            .expect("check");
        assert_eq!(size.severity, Severity::Warn, "{}", size.detail);
        assert!(
            size.detail.contains("callbacks delivered 480..=480 frames"),
            "{}",
            size.detail
        );
        assert!(
            passed(&checks),
            "a warn is the machine's answer, not a broken contract: {size:?}"
        );
    }

    #[test]
    fn a_worst_chunk_over_40_percent_fails_criterion_4() {
        let mut cs = cost();
        cs.max_ns = 533_334; // one ns over 0.40 × 1 333 333
        let v = clean();
        let checks = evaluate(&measured(&v, &cs));
        let worst = checks
            .iter()
            .find(|c| c.name.starts_with("worst chunk cost"))
            .expect("check");
        assert_eq!(worst.severity, Severity::Fail, "{}", worst.detail);
    }

    #[test]
    fn a_lost_timing_sample_is_reported_as_a_warning() {
        let c = Counters::new();
        c.note_callback(64, 0);
        c.note_chunk(100, 64, 533_333, 1_333_333);
        c.note_ring_drop();
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        let s = checks
            .iter()
            .find(|c| c.name == "cost samples")
            .expect("check");
        assert_eq!(s.severity, Severity::Warn, "{}", s.detail);
    }

    #[test]
    fn a_run_far_shorter_than_asked_is_a_warning_and_zero_frames_is_not() {
        let mut v = clean();
        v.frames = 10_000; // ~0.2 s of the 5 s asked
        let checks = evaluate(&measured(&v, &cost()));
        let d = checks
            .iter()
            .find(|c| c.name.starts_with("delivered audio"))
            .expect("check");
        assert_eq!(d.severity, Severity::Warn, "{}", d.detail);
        v.frames = 0;
        assert_eq!(
            evaluate(&measured(&v, &cost()))
                .into_iter()
                .find(|c| c.name.starts_with("delivered audio"))
                .expect("check")
                .severity,
            Severity::Fail,
            "no frames at all is not a run"
        );
    }

    #[test]
    fn silence_is_warned_about_and_a_missing_peak_never_prints_as_zero_decibels() {
        let c = Counters::new();
        c.note_callback(64, 0);
        c.note_chunk(100, 64, 533_333, 1_333_333);
        c.note_fft(true);
        c.note_energy(5.0);
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        let lvl = checks
            .iter()
            .find(|c| c.name == "output level")
            .expect("check");
        assert_eq!(lvl.severity, Severity::Warn, "{}", lvl.detail);
        assert!(
            lvl.detail.contains("−inf (nothing heard)"),
            "{}",
            lvl.detail
        );
    }

    #[test]
    fn a_clipping_output_is_warned() {
        let c = Counters::new();
        c.note_callback(64, 0);
        c.note_chunk(100, 64, 533_333, 1_333_333);
        c.note_levels(3.0, -10.0);
        c.note_fft(true);
        c.note_energy(5.0);
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "output level")
                .expect("check")
                .severity,
            Severity::Warn,
            "+3 dBFS is not a clean probe"
        );
    }

    #[test]
    fn a_non_finite_energy_fails_because_a_bin_went_bad() {
        let c = Counters::new();
        c.note_callback(64, 0);
        c.note_chunk(100, 64, 533_333, 1_333_333);
        c.note_levels(-14.0, -20.0);
        c.note_fft(true);
        c.note_energy(f64::NAN);
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name.starts_with("analysis ran"))
                .expect("check")
                .severity,
            Severity::Fail,
            "NaN must not be printed as a number"
        );
    }

    #[test]
    fn a_window_that_never_filled_warns_instead_of_failing() {
        let c = Counters::new();
        c.note_callback(64, 0);
        c.note_chunk(100, 64, 533_333, 1_333_333);
        c.note_levels(-14.0, -20.0);
        c.note_fft(false);
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name.starts_with("analysis ran"))
                .expect("check")
                .severity,
            Severity::Warn
        );
    }

    #[test]
    fn missing_ftz_is_a_warning_with_its_reason() {
        let c = Counters::new();
        c.note_callback(64, 0);
        c.note_chunk(100, 64, 533_333, 1_333_333);
        c.note_levels(-14.0, -20.0);
        c.note_fft(true);
        c.note_energy(1.0);
        let v = c.view();
        let checks = evaluate(&measured(&v, &cost()));
        let ftz = checks
            .iter()
            .find(|c| c.name == "denormals")
            .expect("check");
        assert_eq!(ftz.severity, Severity::Warn);
        assert!(ftz.detail.contains("NOT installed"), "{}", ftz.detail);
    }

    #[test]
    fn no_callback_ever_prints_none_not_four_billion_frames() {
        assert_eq!(fmt_min(u32::MAX), "none");
        assert_eq!(fmt_min(64), "64");
    }

    #[test]
    fn the_line_layout_is_stable_and_scannable() {
        let v = clean();
        let ls = lines(&evaluate(&measured(&v, &cost())));
        assert!(ls.len() >= 9, "one line per check: {}", ls.len());
        assert!(
            ls.iter()
                .all(|l| l.starts_with("ok") || l.starts_with("warn") || l.starts_with("FAIL")),
            "{ls:?}"
        );
        assert!(
            ls.iter().any(|l| l.contains("allocations observed")),
            "{ls:?}"
        );
    }

    #[test]
    fn an_unasked_for_node_refusal_fails() {
        let v = clean();
        let sample_cost = cost();
        let mut m = measured(&v, &sample_cost);
        m.node_refused = true;
        assert!(!passed(&evaluate(&m)));
    }

    #[test]
    fn silence_and_broken_measurements_are_distinguishable_in_the_helpers() {
        assert_eq!(
            CountersView::from_bits_opt(f32::NEG_INFINITY.to_bits()),
            None
        );
        assert_eq!(CountersView::from_bits_opt(f32::NAN.to_bits()), None);
        assert_eq!(CountersView::from_bits_opt((-6.0f32).to_bits()), Some(-6.0));
        assert_eq!(
            CountersView::energy_from_bits_opt(f64::INFINITY.to_bits()),
            None
        );
        assert_eq!(
            CountersView::energy_from_bits_opt(1.0f64.to_bits()),
            Some(1.0)
        );
    }
}
