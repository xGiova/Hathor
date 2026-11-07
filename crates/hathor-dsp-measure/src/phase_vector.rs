//! Per-bin phase and group delay over the FFT machinery.
//!
//! ## What this answers, and at which rate
//!
//! [`PhaseVector`] turns one frame of `N` real samples into the quantities the
//! kick↔bass tooling reads: the phase of every analytic bin (f64), its
//! unwrapped version, and the **group delay** `−dφ/dω` in samples. It is an
//! analysis object: `analyze` is the only callback-shaped part (it delegates to
//! the frozen [`hathor_dsp_fft::RealFftPlan`] kernel — zero allocation, zero
//! transcendentals); `extract_phases` runs the `atan2`s and is **UI-rate by
//! contract**, the same split the fft crate makes between `process` and
//! `render_db`. Buffers exist from `new(n)` onward (invariant 1: a declared
//! maximum, never a per-call `Vec`).
//!
//! ## The unwrap convention, in words before code
//!
//! `atan2` returns the *principal* phase in [−π, +π]. A true phase ramp
//! (delayed signal: `φ_k = −2πkD/N`) crosses that cut again and again, and the
//! raw output looks like a sawtooth. Unwrapping walks the bins in order and
//! adds the multiple of 2π that keeps each **step** in `(−π, π]`: the step at
//! the exact boundary is counted as `+π`, consistently, so the convention is
//! total — every input produces exactly one output, and the impulse-ramp test
//! reproduces it bit for bit. This is **unwrapping across bins** (the
//! frequency axis), which is what `dφ/dω` needs; it is *not* the time-axis
//! unwrapping of successive frames (a phase-vocoder concern, no consumer yet,
//! not guessed here).
//!
//! ## Edges: documented exclusion, not silent garbage
//!
//! DC and Nyquist are purely real *by construction* of the unpacking (the fft
//! crate writes their imaginary parts as exact zeros — their `real.rs` header
//! makes that the crux of getting a real FFT right). Their `atan2` is `0` or
//! `±π` according only to the **sign** of the real part: they carry no phase
//! information to differentiate. So `group_delay()` reports `f64::NAN` at bin
//! `0` and bin `N/2`, and every interior bin uses forward differences
//! `(φ_{k+1} − φ_k)·(−N/2π)` with a backward difference at the last interior
//! bin — never a difference that touches an informationless edge. On a true
//! linear ramp forward and backward are identical, which the impulse oracle
//! test asserts across all bins at once.
//!
//! ## Reading a phase you cannot trust
//!
//! Empty bins (magnitude 0 — silence, or a tone's leakage-free zeros) have
//! principal phase `0.0` by IEEE `atan2(0,0)`: finite but **meaningless**.
//! Group delay is only informative where [`PhaseVector::magnitudes`] says
//! there is signal; the consumer gates, this crate reports. `NaN` in the input
//! poisons every output (`NaN` everywhere), same doctrine as [`crate::peak_db`].

use hathor_dsp_fft::{Complex, RealFftPlan};
use std::f64::consts::{PI, TAU};

/// Reduce a phase step to the half-open interval `(−π, π]` — the unwrapping
/// convention of this module. Boundary (`±π`) always resolves to `+π`, which
/// makes the rule total; `NaN` passes through.
fn wrap_step(mut d: f64) -> f64 {
    // Floor-mod onto [0, 2π), then fold the upper half — avoids the
    // round-half-away asymmetry of `d − 2π·(d/2π).round()`.
    d -= TAU * (d / TAU).floor();
    if d > PI {
        d -= TAU;
    }
    d
}

/// One frame of a real signal → per-bin magnitude, phase (principal and
/// unwrapped), and group delay. All buffers are allocated in [`PhaseVector::new`];
/// `analyze` and `extract_phases` never allocate.
pub struct PhaseVector {
    plan: RealFftPlan,
    /// Analytic spectrum, bins `0..=N/2` (no-window frame; windowing is the
    /// caller's business before `analyze`).
    bins: Vec<Complex>,
    magnitudes: Vec<f64>,
    principal: Vec<f64>,
    unwrapped: Vec<f64>,
    group_delay: Vec<f64>,
    /// `−N/2π`: converts an unwrapped-phase step between adjacent bins into a
    /// delay in samples (Δω between bins is the constant `2π/N`). Precomputed
    /// at setup: the hot loop multiplies, it does not divide.
    delay_gain: f64,
}

impl PhaseVector {
    /// Builds the plan and every output buffer for frames of exactly
    /// `n_samples`. **The only allocation in the life of the object.**
    ///
    /// # Panics
    /// If `n_samples` is not a power of two ≥ 4 (delegated to
    /// [`RealFftPlan::new`], whose panic message is the one users see).
    pub fn new(n_samples: usize) -> Self {
        // Setup allocations, declared per CONVENTION §2.2.
        let plan = RealFftPlan::new(n_samples);
        let bins = plan.bin_count();
        Self {
            bins: vec![Complex::ZERO; bins],
            magnitudes: vec![0.0; bins],
            principal: vec![0.0; bins],
            unwrapped: vec![0.0; bins],
            // Edges start and stay NaN; interior bins are refilled on every
            // extract, so no stale value survives an extract call.
            group_delay: vec![f64::NAN; bins],
            delay_gain: -(n_samples as f64) / TAU,
            plan,
        }
    }

    /// Frames per analysis window (`N`).
    pub fn n_samples(&self) -> usize {
        self.plan.n_samples()
    }

    /// Bins in every output slice: `N/2 + 1` (DC through Nyquist). The
    /// bin→Hz mapping is `hathor_dsp_fft::bin_freq(k, n_samples, fs)` —
    /// invariant 3 over there, no consumer re-derives it here.
    pub fn bin_count(&self) -> usize {
        self.plan.bin_count()
    }

    /// Transform one frame into the owned bin buffer. Callback-shaped work:
    /// zero allocation, zero lock, zero transcendental (the half-size
    /// [`FftPlan`](hathor_dsp_fft::FftPlan) kernel and nothing else).
    ///
    /// # Panics
    /// If `samples.len() != n_samples()` (the fft plan's panic, message
    /// included for the caller's benefit).
    pub fn analyze(&mut self, samples: &[f32]) {
        self.plan.process(samples, &mut self.bins);
    }

    /// Derive magnitude, principal phase, unwrapped phase and group delay from
    /// the current bins. **UI/offline rate**: `atan2`/`hypot` are
    /// transcendentals — they run here, never in `analyze`, so the audio-path
    /// half of this object keeps the bounded-cycle guarantee.
    ///
    /// Run `analyze` first; extraction is idempotent (the same bins give the
    /// same outputs, bit for bit).
    pub fn extract_phases(&mut self) {
        let nc = self.plan.n_samples() / 2;
        let mut prev_unwrapped = 0.0f64;
        for (k, &bin) in self.bins.iter().enumerate() {
            let re = bin.re as f64;
            let im = bin.im as f64;
            // f64 hypot: overflow-safe magnitude without a second pass.
            self.magnitudes[k] = re.hypot(im);
            let pv = im.atan2(re);
            self.principal[k] = pv;
            let u = if k == 0 {
                pv
            } else {
                prev_unwrapped + wrap_step(pv - prev_unwrapped)
            };
            self.unwrapped[k] = u;
            prev_unwrapped = u;
        }

        // Group delay, samples. Edges NaN by the module's exclusion rule.
        self.group_delay[0] = f64::NAN;
        self.group_delay[nc] = f64::NAN;
        // Interior forward differences k = 1..=nc−2 (empty when N ≤ 4: a
        // 4-sample frame has one interior bin and no valid difference — every
        // read is honestly NaN).
        for k in 1..nc.saturating_sub(1) {
            self.group_delay[k] = self.delay_gain * (self.unwrapped[k + 1] - self.unwrapped[k]);
        }
        if nc >= 3 {
            // Last interior bin: backward difference, still edge-free.
            self.group_delay[nc - 1] =
                self.delay_gain * (self.unwrapped[nc - 1] - self.unwrapped[nc - 2]);
        }
    }

    /// Per-bin magnitude (f64) of the last analyzed frame. Gate the phase
    /// readings with it: zeros have phase, but it means nothing.
    pub fn magnitudes(&self) -> &[f64] {
        &self.magnitudes
    }

    /// Principal phase per bin, `atan2` convention `−π..=+π`.
    pub fn principal_phases(&self) -> &[f64] {
        &self.principal
    }

    /// Unwrapped phase per bin: the principal readings joined by the
    /// smallest-magnitude steps (convention in the module header). The
    /// continuous curve `dφ/dω` differentiates lives here.
    pub fn phases(&self) -> &[f64] {
        &self.unwrapped
    }

    /// Group delay per bin, **in samples**: `−(φ_{k+1} − φ_k)·N/2π`
    /// (forward), backward at the last interior bin. A pure delay of `D`
    /// samples reads `D` at every informative bin — positive means the frame
    /// content is delayed relative to the window start, the same sign
    /// direction as [`crate::time_align_lag`]'s convention.
    ///
    /// Bins `0` and `N/2` are `f64::NAN` (documented exclusion); bins with no
    /// signal are numerically defined but meaningless — check
    /// [`PhaseVector::magnitudes`].
    pub fn group_delay(&self) -> &[f64] {
        &self.group_delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closed form for the DFT convention of this stack (`e^{−2πikj/n}`, see
    /// the fft plan): `x[j] = δ[j − D]` → `X_k = e^{−2πikD/N}` for every k.
    /// That is the external oracle — algebra, not this code re-run.
    fn impulse(n: usize, d: usize) -> Vec<f32> {
        let mut x = vec![0.0f32; n];
        x[d] = 1.0;
        x
    }

    /// Phase of `e^{−iθ}` via independent trig: `(re, im) = (cos θ, −sin θ)`.
    fn principal_of_neg_ramp(theta: f64) -> f64 {
        (-theta.sin()).atan2(theta.cos())
    }

    fn analyze_extract(n: usize, x: &[f32]) -> PhaseVector {
        let mut pv = PhaseVector::new(n);
        pv.analyze(x);
        pv.extract_phases();
        pv
    }

    #[test]
    fn delayed_impulse_reads_its_delay_at_every_interior_bin() {
        // Oracle: DFT of an impulse at position D has linear phase −2πkD/N →
        // group delay D at ALL bins. Tolerance: the fft kernel's documented
        // absolute accuracy is 1e-4 at |X| = 1 → phase error ≤ ~1e-4 rad →
        // delay error ≤ 1e-4·N/2π = 4.1e-3 samples at N=256. Assert 5× that.
        let (n, d) = (256usize, 13usize);
        let pv = analyze_extract(n, &impulse(n, d));
        let nc = n / 2;
        let gd = pv.group_delay();
        for (k, &g) in gd.iter().enumerate().take(nc).skip(1) {
            assert!(
                (g - d as f64).abs() < 0.02,
                "bin {k}: group delay {g}, expected {d}.0 samples"
            );
        }
    }

    #[test]
    fn zero_delay_impulse_anchors_the_sign_convention() {
        // Impulse at position 0 → delay 0 everywhere; combined with the 13
        // test above, positive = content delayed inside the window (the
        // mirror of time_align_lag's invariant-4 direction).
        let n = 256;
        let pv = analyze_extract(n, &impulse(n, 0));
        for (k, &g) in pv.group_delay().iter().enumerate() {
            if k == 0 || k == n / 2 {
                continue;
            }
            assert!(g.abs() < 0.02, "bin {k}: zero-delay reads {g}");
        }
    }

    #[test]
    fn edges_are_nan_and_interiors_dont_touch_them() {
        let (n, d) = (256usize, 13usize);
        let pv = analyze_extract(n, &impulse(n, d));
        let gd = pv.group_delay();
        assert!(gd[0].is_nan(), "DC must be excluded, not guessed");
        assert!(gd[n / 2].is_nan(), "Nyquist must be excluded, not guessed");
        // Interior is fully populated and finite.
        assert!((1..n / 2).all(|k| gd[k].is_finite()));
    }

    #[test]
    fn unwrapped_phase_is_the_exact_negative_ramp() {
        // The unwrap convention test that matters: the raw principal values
        // wrap at every half-sweep (D·k/N crosses ½ four times here), and
        // unwrapping must rebuild φ_k = −2πkD/N within the kernel's accuracy.
        let (n, d) = (256usize, 13usize);
        let pv = analyze_extract(n, &impulse(n, d));
        for (k, &u) in pv.phases().iter().enumerate().take(n / 2).skip(1) {
            let want = -TAU * k as f64 * d as f64 / n as f64;
            assert!(
                (u - want).abs() < 2e-3,
                "bin {k}: unwrapped {u}, want {want} rad"
            );
        }
    }

    #[test]
    fn principal_phases_are_the_wrapped_ramp_from_independent_trig() {
        // Reference built with sin/cos of the angle, not by re-running the
        // ramp arithmetic — a shared mistake cannot survive two paths
        // (the −a2·out lesson, applied to a test).
        let (n, d) = (256usize, 13usize);
        let pv = analyze_extract(n, &impulse(n, d));
        for (k, &got) in pv.principal_phases().iter().enumerate().take(n / 2).skip(1) {
            let want = principal_of_neg_ramp(TAU * k as f64 * d as f64 / n as f64);
            // Comparison in wrapped space: −π vs +π are the same phase, so
            // the boundary sign choice cannot fake a failure here.
            let diff = wrap_step(got - want).abs();
            assert!(
                diff < 2e-3,
                "bin {k}: principal {got} rad, want {want} rad (wrapped diff {diff:.3e})"
            );
        }
    }

    #[test]
    fn wrap_step_honours_the_half_open_interval() {
        // Convention unit: steps land in (−π, π], boundary → +π, NaN passes.
        assert!((wrap_step(PI) - PI).abs() < 1e-12, "+π must stay +π");
        assert!((wrap_step(-PI) - PI).abs() < 1e-12, "−π resolves to +π");
        assert!(wrap_step(0.0).abs() < 1e-12);
        // Many whole turns: 3·2π + 0.5 reduces to the bare 0.5 step.
        assert!(
            (wrap_step(3.0 * TAU + 0.5) - 0.5).abs() < 1e-12,
            "whole turns must vanish, the residue must not move"
        );
        assert!((wrap_step(-TAU + 0.5) - 0.5).abs() < 1e-12);
        assert!(wrap_step(f64::NAN).is_nan());
    }

    #[test]
    fn delayed_tone_reads_the_closed_form_phase_at_its_bin() {
        // Task's own suggested oracle: sine with a known sample delay.
        // x[j] = cos(2πk₀(j−D)/N), exactly integer periods: X at k₀ is
        // (N/2)·e^{−2πik₀D/N} → principal phase −2πk₀D/N wrapped, magnitude
        // N/2 (unit-amplitude cosine). The task's parenthetical "group delay
        // error" is covered by the impulse/comb tests: a single tone has no
        // phase information in the empty neighbour bins, and pretending
        // otherwise would be the meaningless reading the header warns about.
        let n = 512;
        let k0 = 7usize;
        let d = 13usize;
        let x: Vec<f32> = (0..n)
            .map(|j| {
                let theta = TAU * k0 as f64 * (j as f64 - d as f64) / n as f64;
                theta.cos() as f32
            })
            .collect();
        let pv = analyze_extract(n, &x);
        let want_mag = n as f64 / 2.0; // 256.0
        assert!(
            (pv.magnitudes()[k0] - want_mag).abs() / want_mag < 1e-3,
            "bin {k0}: magnitude {}, expected 256.0",
            pv.magnitudes()[k0]
        );
        let want_phase = principal_of_neg_ramp(TAU * k0 as f64 * d as f64 / n as f64);
        let diff = wrap_step(pv.principal_phases()[k0] - want_phase).abs();
        assert!(
            diff < 1e-4,
            "tone phase at its bin: wrapped diff {diff:.3e} rad, expected ~0"
        );
    }

    #[test]
    fn comb_with_uneven_energies_reads_one_flat_delay() {
        // Group delay must be amplitude-blind: tones on every bin 1..=300 of a
        // 1024-frame, all delayed by the same 9 samples, with deliberately
        // uneven amplitudes (0.02…0.08). Closed form per comb bin: the frame
        // is Σ aₖ cos(2πk(j−D)/N); bins separate (integer periods) and each
        // carries phase −2πkD/N → delay D at every k whose neighbour k+1 is
        // also a comb bin (k ≤ 299).
        let n = 1024;
        let d = 9usize;
        let top = 300usize;
        let x: Vec<f32> = (0..n)
            .map(|j| {
                let mut s = 0.0f64;
                for k in 1..=top {
                    let a = 0.02 + 0.01 * (k % 7) as f64;
                    s += a * (TAU * k as f64 * (j as f64 - d as f64) / n as f64).cos();
                }
                s as f32
            })
            .collect();
        let pv = analyze_extract(n, &x);
        for (k, &g) in pv.group_delay().iter().enumerate().take(top).skip(1) {
            assert!(
                (g - d as f64).abs() < 0.05,
                "comb bin {k}: delay {g}, expected {d}.0"
            );
        }
    }

    #[test]
    fn silence_is_finite_but_flagged_by_magnitudes() {
        let n = 256;
        let z = vec![0.0f32; n];
        let pv = analyze_extract(n, &z);
        for k in 0..pv.bin_count() {
            assert_eq!(pv.magnitudes()[k], 0.0, "bin {k} silent");
            if k != 0 && k != n / 2 {
                assert!(
                    pv.group_delay()[k] == 0.0,
                    "empty-bin delay is finite 0, not NaN: the magnitude slice, \
                     not the delay slice, is what says 'no information'"
                );
            }
        }
    }

    #[test]
    fn nan_input_poisons_every_reading() {
        let n = 256;
        let mut x = impulse(n, 13);
        x[5] = f32::NAN;
        let pv = analyze_extract(n, &x);
        // One NaN sample mixes into every bin; a silent-looking meter is not
        // an option (corruption-visible doctrine, same as peak_db).
        assert!(pv.magnitudes()[1].is_nan());
        assert!(pv.principal_phases()[1].is_nan());
        assert!(pv.phases()[1].is_nan());
        assert!(pv.group_delay()[1].is_nan());
    }

    #[test]
    fn extraction_is_idempotent_bit_for_bit() {
        // Same bins → same outputs: no hidden running state in extract, so
        // calling it twice (after the SAME analyze) cannot drift. Pins
        // determinism of the convention. Note the to_bits: the delay slice
        // carries NaN edges, and `NaN != NaN` in IEEE — a float-equality
        // comparison would fail on two literally identical bit patterns.
        // (This assertion, written as slice equality, was exactly that bug.)
        let n = 256;
        let mut pv = PhaseVector::new(n);
        pv.analyze(&impulse(n, 13));
        pv.extract_phases();
        let first_unwrapped: Vec<u64> = pv.phases().iter().copied().map(f64::to_bits).collect();
        let first_delay: Vec<u64> = pv.group_delay().iter().copied().map(f64::to_bits).collect();
        pv.extract_phases();
        let again_unwrapped: Vec<u64> = pv.phases().iter().copied().map(f64::to_bits).collect();
        let again_delay: Vec<u64> = pv.group_delay().iter().copied().map(f64::to_bits).collect();
        assert_eq!(again_unwrapped, first_unwrapped);
        assert_eq!(again_delay, first_delay);
    }

    #[test]
    fn four_sample_frame_reports_only_nan_delays() {
        // N=4: a single interior bin, no valid neighbour pair → every delay
        // read is honestly NaN (the degenerate case the fill loops handle
        // without indexing accidents).
        let pv = analyze_extract(4, &[1.0, 0.0, 0.0, 0.0]);
        assert!(pv.group_delay().iter().all(|g| g.is_nan()));
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn rejects_a_non_power_of_two_frame() {
        PhaseVector::new(1000);
    }

    #[test]
    #[should_panic(expected = "real input")]
    fn analyze_rejects_the_wrong_frame_length() {
        let mut pv = PhaseVector::new(64);
        pv.analyze(&[0.0f32; 32]);
    }
}
