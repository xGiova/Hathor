//! The real-input FFT: pack, transform half-size, unpack hermitian.
//!
//! ## The trick, in three sentences
//!
//! A real signal of `N` samples carries the same information as `N/2` complex
//! samples (evens into the real parts, odds into the imaginary parts), so the
//! `N/2`-point complex FFT of the packed stream already contains the full spectrum
//! *twice* — the second copy conjugated, because a real signal has hermitian
//! symmetry. Solving the 2×2 system that relates the packed spectrum `Y` to the true
//! one gives, for `k = 0..=N/2` with `m = N/2 − k`:
//!
//! ```text
//! X[k] = ½(Y[k] + conj(Y[m])) − ½·i·e^{−2πik/N}·(Y[k] − conj(Y[m]))
//! ```
//!
//! (the module doc's formula, verbatim). The cost is one half-size complex FFT plus
//! an O(N) unpack — the analyzer's cheapest structural win.
//!
//! ## Where "almost right" dies: the two edges
//!
//! At `k = 0` (DC) and `k = N/2` (Nyquist) the partner index `m` wraps onto `k`
//! itself *mod N/2*, and both bins are purely real. The general formula evaluates
//! correctly there too, but an implementation that forgets the wrap — or leaves a
//! `±1e-7` imaginary residue on the two bins that are mathematically real — is the
//! classic almost-right unpack. They are therefore handled by hand (their true
//! closed forms are `Re(Y₀) ± Im(Y₀)`) and the imaginary parts are written as exact
//! zeros. The packed-vs-full-complex test is the specification of everything else.
//!
//! ## Why a second twiddle table
//!
//! The unpack needs `e^{−2πik/N}`, the *half* angles of the `N/2`-point kernel —
//! they are not in the complex plan's table. They are built here, in `new`, so
//! `process` stays allocation-free *and* transcendental-free.

use crate::complex::Complex;
use crate::plan::FftPlan;
use std::f64::consts::TAU;

/// Real-input FFT of a fixed power-of-two signal length.
///
/// `bins` are indexed `k = 0..=N/2`; [`crate::bin_freq`] maps an index to Hz
/// (invariant 3 — no consumer re-derives it). Like [`FftPlan`], everything
/// allocates in [`RealFftPlan::new`] and nothing in [`RealFftPlan::process`].
pub struct RealFftPlan {
    /// The half-size complex kernel that does the heavy lifting.
    plan: FftPlan,
    /// `e^{-2πik/N}` for `k = 0..=N/2`: the unpack's half-angles.
    unpack_twiddles: Vec<Complex>,
    /// Packed-signal workspace, `N/2` entries. The spec's `scratch` field, in the
    /// one kernel that needs it: packing an `&[f32]` input needs a destination that
    /// is not the caller's memory.
    scratch: Vec<Complex>,
}

impl RealFftPlan {
    /// Builds a plan for real signals of exactly `n_samples` frames.
    ///
    /// # Panics
    /// If `n_samples` is not a power of two or is below 4 (the minimum signal whose
    /// `N/2`-point complex kernel is non-degenerate — see [`FftPlan::new`]).
    pub fn new(n_samples: usize) -> Self {
        assert!(
            n_samples >= 4 && n_samples.is_power_of_two(),
            "RealFftPlan signal length must be a power of two >= 4, got {n_samples}"
        );
        let nc = n_samples / 2;
        // Setup allocation, declared per CONVENTION §2.2: the k=0 entry exists even
        // though the edges bypass the table, so the loop below is one index rule,
        // not two.
        let unpack_twiddles: Vec<Complex> = (0..=nc)
            .map(|k| Complex::from_angle_rad(-TAU * k as f64 / n_samples as f64))
            .collect();
        Self {
            plan: FftPlan::new(nc),
            unpack_twiddles,
            scratch: vec![Complex::ZERO; nc],
        }
    }

    /// Real signal length `N` this plan transforms.
    pub fn n_samples(&self) -> usize {
        self.plan.n() * 2
    }

    /// Number of output bins: `N/2 + 1` (DC through Nyquist, hermitian fold).
    pub fn bin_count(&self) -> usize {
        self.plan.n() + 1
    }

    /// In-place-free real transform: reads `samples` (`N` frames), writes the
    /// `N/2 + 1` bins of the analytic spectrum into `bins`.
    ///
    /// Zero allocations, zero locks, zero transcendentals.
    ///
    /// # Panics
    /// If `samples.len() != N` or `bins.len() != N/2 + 1`.
    pub fn process(&mut self, samples: &[f32], bins: &mut [Complex]) {
        let nc = self.plan.n();
        assert_eq!(
            samples.len(),
            nc * 2,
            "real input of {} samples for a plan of {}",
            samples.len(),
            nc * 2
        );
        assert_eq!(
            bins.len(),
            nc + 1,
            "bin buffer of {} for a plan producing {}",
            bins.len(),
            nc + 1
        );

        // 1. Pack: even frames → re, odd frames → im. N is a power of two >= 4, so
        // the chunk size is constant and the tail is provably empty.
        let (pairs, tail) = samples.as_chunks::<2>();
        debug_assert!(tail.is_empty());
        for (pair, slot) in pairs.iter().zip(self.scratch.iter_mut()) {
            *slot = Complex::new(pair[0], pair[1]);
        }

        // 2. Half-size complex FFT, in-place on the packed scratch.
        self.plan.process(&mut self.scratch);

        // 3. Unpack. The edges first, by hand (see the module header).
        let y0 = self.scratch[0];
        bins[0] = Complex::new(y0.re + y0.im, 0.0); // DC: Σ samples.
        bins[nc] = Complex::new(y0.re - y0.im, 0.0); // Nyquist: alternating sum.
                                                     // The loop index addresses three collections at once (bins[k], scratch[k],
                                                     // scratch[nc−k], twiddles[k]): an enumerate/zip rewrite would scatter the
                                                     // formula across iterator plumbing. `k` *is* the subject here — justified
                                                     // allow, the documented pattern of this codebase.
        #[allow(clippy::needless_range_loop)]
        for k in 1..nc {
            let yk = self.scratch[k];
            let ym = self.scratch[nc - k].conj();
            let s = (yk + ym) * 0.5;
            let d = (yk - ym) * 0.5;
            // −i·z is (z.im, −z.re): the rotation applied to the window term.
            // Writing it as field arithmetic instead of a Complex mul keeps the
            // one remaining sign decision of this file visible on one line.
            let wd = self.unpack_twiddles[k] * d;
            bins[k] = Complex::new(s.re + wd.im, s.im - wd.re);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{hann, noise_f32, Lcg};
    use rustfft::num_complex::Complex32;
    use rustfft::FftPlanner;

    /// Spec verification 6 — *the* specification of the unpack: the packed path
    /// against a full-size complex FFT of the zero-padded same signal, run by
    /// rustfft. If the hermitian formula is wrong anywhere, this dies. Tolerance
    /// 1e-4 absolute per the spec; measured worst cases (this machine, 2026-10-27)
    /// run 2.9e-6 → 4.6e-5 across n = 256…16384.
    #[test]
    fn packed_real_matches_full_complex_reference() {
        for &n in &[256usize, 1024, 4096, 16384] {
            let mut rng = Lcg(0x5EED + n as u32);
            let samples = noise_f32(&mut rng, n);

            let mut ours = vec![Complex::ZERO; n / 2 + 1];
            RealFftPlan::new(n).process(&samples, &mut ours);

            let mut padded: Vec<Complex32> =
                samples.iter().map(|&x| Complex32::new(x, 0.0)).collect();
            let mut planner = FftPlanner::new();
            planner.plan_fft_forward(n).process(&mut padded);

            let mut max_err = 0.0f32;
            let mut worst_k = 0usize;
            for (k, (a, b)) in ours.iter().zip(padded.iter()).enumerate() {
                let e = (a.re - b.re).abs().max((a.im - b.im).abs());
                if e > max_err {
                    max_err = e;
                    worst_k = k;
                }
            }
            assert!(
                max_err < 1e-4,
                "n = {n}: max abs err {max_err:.3e} at bin {worst_k} (ours {:?}, reference {:?})",
                ours[worst_k],
                padded[worst_k]
            );
        }
    }

    /// Second view on the same reference, focused where the hermitian system is
    /// self-dual: at bin N/4 the partner index wraps onto itself (`m == k`), the
    /// one interior corner where "almost right" unpacking can hide. Also covers a
    /// small N=64 the bank sizes would never exercise. (rustfft 6.4.1 has no public
    /// real-input planner, so the independent reference for the real path is the
    /// padded complex FFT — as the spec's verification 6 prescribes.)
    #[test]
    fn unpack_self_dual_bin_survives_the_padded_reference() {
        for &n in &[64usize, 1024] {
            let mut rng = Lcg(0xC055 + n as u32);
            let samples = noise_f32(&mut rng, n);

            let mut ours = vec![Complex::ZERO; n / 2 + 1];
            RealFftPlan::new(n).process(&samples, &mut ours);

            let mut padded: Vec<Complex32> =
                samples.iter().map(|&x| Complex32::new(x, 0.0)).collect();
            let mut planner = FftPlanner::new();
            planner.plan_fft_forward(n).process(&mut padded);

            for k in 0..=n / 2 {
                let (a, b) = (ours[k], padded[k]);
                let e = (a.re - b.re).abs().max((a.im - b.im).abs());
                assert!(
                    e < 1e-4,
                    "n = {n} bin {k}{}: ours {a:?} vs reference {b:?}, err {e:.3e}",
                    if k == n / 4 { " (self-dual)" } else { "" }
                );
            }
        }
    }

    /// Edge grid (CONVENTION §4.6): DC and Nyquist. Constant input → all energy at
    /// bin 0; alternating ±1 → all energy at the Nyquist bin; and both edge bins
    /// carry *exact* zero imaginary part (the hand-handled case the header warns
    /// about).
    #[test]
    fn dc_and_nyquist_edges_are_purely_real() {
        let n = 512;
        let mut plan = RealFftPlan::new(n);
        let mut bins = vec![Complex::ZERO; n / 2 + 1];

        let ones: Vec<f32> = vec![1.0; n];
        plan.process(&ones, &mut bins);
        assert!((bins[0].re - n as f32).abs() <= 1e-4, "DC = {n}");
        assert_eq!(bins[0].im, 0.0, "DC is mathematically real");
        for (k, b) in bins.iter().enumerate().skip(1) {
            assert!(b.norm_sqr() <= 1e-4, "bin {k} should be silent, got {b:?}");
        }

        let alt: Vec<f32> = (0..n)
            .map(|j| if j % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        plan.process(&alt, &mut bins);
        assert!(
            (bins[n / 2].re - n as f32).abs() <= 1e-4,
            "Nyquist = {n}, got {:?}",
            bins[n / 2]
        );
        assert_eq!(bins[n / 2].im, 0.0, "Nyquist is mathematically real");
        for (k, b) in bins.iter().enumerate().take(n / 2) {
            assert!(b.norm_sqr() <= 1e-4, "bin {k} should be silent, got {b:?}");
        }
    }

    /// Spec verification 2, real half: a cosine exactly on bin k parks all energy
    /// in that bin (normalized so the peak reads 1.0). The negative-frequency copy
    /// folds to N − k and is not returned — hermiticity is the reason there is a
    /// real FFT at all.
    #[test]
    fn cosine_on_bin_k_parks_energy_in_the_bin() {
        let n = 512;
        let k0 = 11usize;
        let samples: Vec<f32> = (0..n)
            .map(|j| {
                let angle = 2.0 * std::f64::consts::PI * (k0 * j) as f64 / n as f64;
                // cos of amplitude A puts A·N/2 in the bin: scale 2/N reads 1.0.
                (cos_exact(angle) * (2.0 / n as f64)) as f32
            })
            .collect();
        let mut bins = vec![Complex::ZERO; n / 2 + 1];
        RealFftPlan::new(n).process(&samples, &mut bins);
        assert!(
            (bins[k0].re - 1.0).abs() <= 1e-4 && bins[k0].im.abs() <= 1e-4,
            "peak bin = {:?}",
            bins[k0]
        );
        for (k, b) in bins.iter().enumerate() {
            if k == k0 {
                continue;
            }
            assert!(b.norm_sqr() <= 1e-4, "bin {k} should be silent, got {b:?}");
        }
    }

    /// Spec verification 2, windowed half: Hann multiplies the tone's spectrum by
    /// the window's own three-line spectrum, exactly: bin k₀ → 1, k₀±1 → −0.5,
    /// everything else → 0 (input normalized so the peak bin is 1). This is the
    /// analytic formula the doc demands the window test be compared against.
    #[test]
    fn hann_tone_matches_the_three_line_window_analytic() {
        let n = 1024;
        let k0 = 5usize;
        let scale = 1.0 / (0.25 * n as f64) as f32; // peak bin = 0.25·N → 1
        let samples: Vec<f32> = (0..n)
            .map(|j| {
                let w = hann(j, n);
                let tone = cos_exact(2.0 * std::f64::consts::PI * (k0 * j) as f64 / n as f64);
                (w * tone * scale as f64) as f32
            })
            .collect();
        let mut bins = vec![Complex::ZERO; n / 2 + 1];
        RealFftPlan::new(n).process(&samples, &mut bins);

        let check = |k: usize, want: f32| {
            let b = bins[k];
            assert!(
                (b.re - want).abs() <= 1e-4 && b.im.abs() <= 1e-4,
                "bin {k} = {b:?}, expected ({want}, 0)"
            );
        };
        check(k0, 1.0);
        check(k0 - 1, -0.5);
        check(k0 + 1, -0.5);
        for (k, b) in bins.iter().enumerate() {
            if k.abs_diff(k0) <= 1 {
                continue;
            }
            assert!(
                b.norm_sqr() <= 1e-4,
                "bin {k} should be a zero of the window, got {b:?}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn real_plan_rejects_non_power_of_two() {
        RealFftPlan::new(1000);
    }

    #[test]
    #[should_panic(expected = "real input")]
    fn process_rejects_wrong_input_length() {
        let mut plan = RealFftPlan::new(64);
        let samples = vec![0.0f32; 32];
        let mut bins = vec![Complex::ZERO; 33];
        plan.process(&samples, &mut bins);
    }

    #[test]
    #[should_panic(expected = "bin buffer")]
    fn process_rejects_wrong_bin_length() {
        let mut plan = RealFftPlan::new(64);
        let samples = vec![0.0f32; 64];
        let mut bins = vec![Complex::ZERO; 8];
        plan.process(&samples, &mut bins);
    }

    /// f64 cosine narrowed once: test *inputs* must not carry the f32 argument
    /// error into the reference (a tone that is not exactly periodic in the frame
    /// would fake leakage).
    fn cos_exact(angle: f64) -> f64 {
        angle.cos()
    }
}
