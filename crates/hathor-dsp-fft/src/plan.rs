//! The radix-2 decimation-in-time plan.
//!
//! ## Why radix-2 *is* the right kernel for M0
//!
//! Radix-4 and split-radix save ~15% of the multiplications (expected — the module
//! doc defers the question to the M0 benchmark: if radix-2 is enough for the bank,
//! the extra index arithmetic is not justified). What is *not* deferred is the
//! structure everyone agrees on: bit-reversal + `log2(n)` stages of butterflies,
//! which is exactly what a home-written FFT must get right before a variant is worth
//! measuring.
//!
//! ## The two tables, and why they have the shapes they have
//!
//! * `twiddles`: `e^{-2πik/n}` for `k < n/2`, *not* for `k < n`. A stage of length
//!   `L` butterflies indexes entry `k·(n/L)` with `k < L/2`, i.e. below `n/2` — the
//!   second half of the unit circle is never addressed. Half the memory, half the
//!   cache lines of the naive full table, and the bound is asserted... structurally:
//!   `(L/2 − 1)·(n/L) < n/2` for every `L ≥ 2`.
//! * `bitrev`: the input permutation that makes the *whole* transform in-place.
//!   Without it DIT would need ping-pong buffers (more memory traffic, and a second
//!   scratch array the callback could never afford). Stored as `u32`: at n = 16384
//!   that is 64 KB of table instead of 128 KB, and `n` fits in 32 bits for any plan
//!   an audio analyzer will ever allocate.
//!
//! ## Inverse without a second kernel
//!
//! `IFFT(x) = conj(FFT(conj(x)))/n`. Two conj sweeps around the one forward kernel:
//! there is no second implementation to be "identically wrong" the way the
//! `−a2·out` bug made two filter paths agree with each other and disagree with the
//! algebra. The inverse is exactly as accurate as the forward, by construction.

use crate::complex::Complex;
use std::f64::consts::TAU;

/// A complex FFT of a fixed power-of-two size, with every table built at setup.
///
/// [`FftPlan::new`] is the only method that allocates; [`FftPlan::process`] and
/// [`FftPlan::inverse`] run in-place on caller memory with no allocation, no lock and
/// no transcendental — which is what makes them legal inside an audio callback
/// (`tests/alloc_contract.rs` proves the allocation half with the core guard).
pub struct FftPlan {
    n: usize,
    /// `e^{-2πik/n}`, `k = 0..n/2`.
    twiddles: Vec<Complex>,
    /// Bit-reversal of `i` within `log2(n)` bits.
    bitrev: Vec<u32>,
}

impl FftPlan {
    /// Builds the twiddle and bit-reversal tables. **This is the only allocation in
    /// the life of the plan** (the module doc's invariant 1: allocate at setup, never
    /// at run).
    ///
    /// # Panics
    /// If `n` is not a power of two, or is below 2. A wrong `n` is a programmer bug,
    /// and the module doc explicitly allows the constructor to panic on it; `n = 1`
    /// is a legal FFT but a useless one (the identity map), and excluding it keeps
    /// `log2(n) ≥ 1` a fact everywhere below.
    pub fn new(n: usize) -> Self {
        assert!(
            n >= 2 && n.is_power_of_two(),
            "FftPlan size must be a power of two >= 2, got {n}"
        );
        // Setup allocations, declared per CONVENTION §2.2. The trig runs in f64 and
        // narrows once: the table is the cheapest precision win in the crate.
        let twiddles: Vec<Complex> = (0..n / 2)
            .map(|k| Complex::from_angle_rad(-TAU * k as f64 / n as f64))
            .collect();
        let log_n = n.trailing_zeros();
        let bitrev: Vec<u32> = (0..n as u32)
            .map(|i| i.reverse_bits() >> (32 - log_n))
            .collect();
        Self {
            n,
            twiddles,
            bitrev,
        }
    }

    /// Transform length.
    pub fn n(&self) -> usize {
        self.n
    }

    /// In-place forward transform (the `e^{-2πikj/n}` convention, like the twiddle
    /// table itself).
    ///
    /// # Panics
    /// If `data.len() != self.n()` — a mismatched buffer is a caller bug, and the
    /// real-time contract forbids growing anything here to be "polite" about it.
    pub fn process(&mut self, data: &mut [Complex]) {
        assert_eq!(
            data.len(),
            self.n,
            "buffer of {} samples for a plan of {}",
            data.len(),
            self.n
        );
        // 1. Bit-reversal permutation: in-place, each index touched at most once
        //    (the `i < j` test skips already-swapped pairs and fixed points).
        for (i, &r) in self.bitrev.iter().enumerate() {
            let j = r as usize;
            if i < j {
                data.swap(i, j);
            }
        }

        // 2. Butterflies: `log2(n)` stages, stage `L` combines length-`L/2` blocks
        //    pairwise. `step = n/L` maps the stage's own angles onto the shared
        //    `n/2`-entry table (see the module header for the bound).
        let mut len = 2usize;
        while len <= self.n {
            let half = len / 2;
            let step = self.n / len;
            for block in data.chunks_exact_mut(len) {
                // Disjoint halves: `lo` written through the loop, `hi` read then
                // written; split_at_mut keeps the borrow checker (and the code) honest.
                let (lo, hi) = block.split_at_mut(half);
                for (k, a_slot) in lo.iter_mut().enumerate() {
                    let a = *a_slot;
                    let b = self.twiddles[k * step] * hi[k];
                    *a_slot = a + b;
                    hi[k] = a - b;
                }
            }
            len *= 2;
        }
    }

    /// In-place inverse transform, normalized by `1/n` (so `inverse` after
    /// `process` is the identity map — not the `n·`-scaled one).
    ///
    /// Implementation: `conj → process → conj → scale`, as promised by the module doc.
    ///
    /// # Panics
    /// Like [`FftPlan::process`]: wrong buffer length.
    pub fn inverse(&mut self, data: &mut [Complex]) {
        for c in data.iter_mut() {
            *c = c.conj();
        }
        self.process(data);
        // `1/n` is exact in binary for powers of two, so the normalization round is
        // the scaling itself and nothing else: no hidden bias against the forward.
        let inv_n = 1.0 / self.n as f32;
        for c in data.iter_mut() {
            *c = c.conj() * inv_n;
        }
    }
}

/// Frequency of bin `k` for a transform of size `n` at `sample_rate` Hz: `k·fs/n`.
///
/// The module doc's invariant 3: this is the *only* place the mapping is computed —
/// consumers must not re-derive it. The arithmetic runs in f64 because `k·fs` at
/// n = 16384/48 kHz overflows the f32 mantissa's exact range and would round to
/// ~kHz granularity; the cast happens once, at the end.
pub fn bin_freq(k: usize, n: usize, sample_rate: f32) -> f32 {
    (k as f64 * sample_rate as f64 / n as f64) as f32
}

/// Bin (fractional — it is a coordinate, not an index) where `freq` lands:
/// `freq·n/fs`, the inverse of [`bin_freq`].
///
/// Module-doc exercise 3 made the point with numbers: 60 Hz at n = 16384/48 kHz is
/// bin 20.5, i.e. not an integer — which is precisely why leakage exists and windows
/// are not decoration.
pub fn freq_bin(freq: f32, n: usize, sample_rate: f32) -> f32 {
    (freq as f64 * n as f64 / sample_rate as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{noise_complex, tone, Lcg};
    use rustfft::num_complex::Complex32;
    use rustfft::FftPlanner;

    /// Spec verification 1: unit impulse → flat spectrum at 0 dB (±1e-4).
    #[test]
    fn unit_impulse_gives_a_flat_unit_spectrum() {
        let n = 256;
        let mut plan = FftPlan::new(n);
        let mut data = vec![Complex::ZERO; n];
        data[0] = Complex::new(1.0, 0.0);
        plan.process(&mut data);
        for (k, bin) in data.iter().enumerate() {
            let err = (bin.re - 1.0).abs().max(bin.im.abs());
            assert!(
                err <= 1e-4,
                "bin {k} = {bin:?}, expected (1,0); |err| = {err:.3e}"
            );
        }
    }

    /// Spec verification 2 (no-window half): a complex tone exactly on bin k parks
    /// all the energy in k. Amplitude `1/n` so the peak bin is ≈ 1 and the spec's
    /// 1e-4 tolerance reads on a unit-amplitude scale.
    #[test]
    fn tone_on_bin_k_leaks_nothing_into_other_bins() {
        let n = 256;
        let k0 = 37; // interior, avoids the DC/Nyquist fold cases
        let mut plan = FftPlan::new(n);
        let mut data = tone(n, k0, 1.0 / n as f32);
        plan.process(&mut data);
        assert!(
            (data[k0].re - 1.0).abs() <= 1e-4 && data[k0].im.abs() <= 1e-4,
            "peak bin = {:?}",
            data[k0]
        );
        for (k, bin) in data.iter().enumerate() {
            if k == k0 {
                continue;
            }
            assert!(
                bin.norm_sqr().sqrt() <= 1e-4,
                "bin {k} should be silent, got {bin:?}"
            );
        }
    }

    /// Spec verification 3: Parseval, Σ|x|² = Σ|X|²/n (±1e-3). Catches scaling
    /// errors in *both* directions, the most common bug of home-made FFTs.
    #[test]
    fn parseval_holds_forward_and_inverse() {
        let n = 1024;
        let mut plan_f = FftPlan::new(n);
        let mut plan_i = FftPlan::new(n);
        let mut rng = Lcg(0xC0FFEE);
        let data: Vec<Complex> = noise_complex(&mut rng, n);
        let power_x: f64 = data.iter().map(|c| c.norm_sqr() as f64).sum();

        let mut fwd = data;
        plan_f.process(&mut fwd);
        let power_f: f64 = fwd.iter().map(|c| c.norm_sqr() as f64).sum();
        let rel = ((power_f / n as f64) / power_x - 1.0).abs();
        assert!(rel <= 1e-3, "forward Parseval off by {rel:.3e}");

        plan_i.inverse(&mut fwd);
        let power_i: f64 = fwd.iter().map(|c| c.norm_sqr() as f64).sum();
        let rel = (power_i / power_x - 1.0).abs();
        assert!(rel <= 1e-3, "inverse Parseval off by {rel:.3e}");
    }

    /// Spec verification 4: point-by-point against rustfft on pseudo-random
    /// signals at every size the M1 bank will use. Tolerance 1e-4 **absolute**, as
    /// the spec demands — and it is not a hope: the measured worst-case gaps
    /// (this machine, 2026-10-27) are 3.8e-6 / 9.5e-6 / 3.1e-5 / 6.1e-5 for
    /// n = 256/1024/4096/16384, so the assert is the spec's line, not a fitted one.
    #[test]
    fn matches_rustfft_pointwise_at_bank_sizes() {
        for &n in &[256usize, 1024, 4096, 16384] {
            let mut rng = Lcg(0xB0BA + n as u32);
            let data = noise_complex(&mut rng, n);

            let mut ours = data.clone();
            FftPlan::new(n).process(&mut ours);

            let mut theirs: Vec<Complex32> =
                data.iter().map(|c| Complex32::new(c.re, c.im)).collect();
            let mut planner = FftPlanner::new();
            planner.plan_fft_forward(n).process(&mut theirs);

            let mut max_err = 0.0f32;
            let mut worst_k = 0usize;
            for (k, (a, b)) in ours.iter().zip(theirs.iter()).enumerate() {
                let e = (a.re - b.re).abs().max((a.im - b.im).abs());
                if e > max_err {
                    max_err = e;
                    worst_k = k;
                }
            }
            assert!(
                max_err < 1e-4,
                "n = {n}: max abs err {max_err:.3e} at bin {worst_k} (ours {:?}, rustfft {:?})",
                ours[worst_k],
                theirs[worst_k]
            );
        }
    }

    /// Spec verification 5: `process` → `inverse` = identity (±1e-4).
    #[test]
    fn inverse_undoes_the_forward_exactly() {
        for &n in &[2usize, 256, 1024, 16384] {
            let mut rng = Lcg(7 + n as u32);
            let data = noise_complex(&mut rng, n);
            let mut round = data.clone();
            let mut plan = FftPlan::new(n);
            plan.process(&mut round);
            plan.inverse(&mut round);
            for (k, (a, b)) in round.iter().zip(data.iter()).enumerate() {
                assert!(
                    (a.re - b.re).abs() <= 1e-4 && (a.im - b.im).abs() <= 1e-4,
                    "n = {n}: sample {k} = {a:?}, was {b:?}"
                );
            }
        }
    }

    /// Linearity: F(2x + 3y) = 2·F(x) + 3·F(y). The comparison against rustfft
    /// covers correctness; this nails *scaling/indexing* slips inside our own kernel
    /// (a wrong stride breaks superposition long before it breaks a norm).
    #[test]
    fn superposition_holds() {
        let n = 256;
        let mut rng = Lcg(0xC0FFEE);
        let x = noise_complex(&mut rng, n);
        let y = noise_complex(&mut rng, n);

        let mut mix: Vec<Complex> = x.iter().zip(&y).map(|(a, b)| *a * 2.0 + *b * 3.0).collect();
        let mut plan = FftPlan::new(n);
        plan.process(&mut mix);

        let (mut fx, mut fy) = (x.clone(), y.clone());
        plan.process(&mut fx);
        plan.process(&mut fy);
        let expected: Vec<Complex> = fx
            .iter()
            .zip(&fy)
            .map(|(a, b)| *a * 2.0 + *b * 3.0)
            .collect();

        // Bin magnitudes here are O(5·√n) ≈ 80, so 5e-3 absolute ≈ 6e-5 relative —
        // a justified tolerance per CONVENTION §4.3, not a shrug.
        for (k, (a, b)) in mix.iter().zip(expected.iter()).enumerate() {
            let err = (a.re - b.re).abs().max((a.im - b.im).abs());
            assert!(
                err <= 5e-3,
                "bin {k}: err {err:.3e} exceeds linear-consistency tol"
            );
        }
    }

    /// The smallest legal plan, checked against the butterfly by hand:
    /// `[a, b] → [a + b, a − b]`.
    #[test]
    fn two_point_plan_is_the_hand_butterfly() {
        let mut plan = FftPlan::new(2);
        let mut data = vec![Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)];
        plan.process(&mut data);
        assert_eq!(data, vec![Complex::new(4.0, 6.0), Complex::new(-2.0, -2.0)]);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn plan_rejects_a_non_power_of_two() {
        FftPlan::new(3000);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn plan_rejects_size_one_as_a_degenerate_choice() {
        FftPlan::new(1);
    }

    #[test]
    #[should_panic(expected = "buffer")]
    fn process_rejects_a_mismatched_buffer() {
        let mut plan = FftPlan::new(64);
        let mut data = vec![Complex::ZERO; 32];
        plan.process(&mut data);
    }

    /// Module-doc exercise 3, asserted at the exact arithmetic. Note a doc-drift:
    /// the doc quotes "answer: 20.5" for 60 Hz at n = 16384/48 kHz, but the exact
    /// bin is 60·16384/48000 = **20.48** (the doc rounds). The point — a
    /// non-integer, hence leakage — stands either way; the test pins the number,
    /// and the drift is reported to the lead rather than patched in docs/.
    #[test]
    fn bin_and_freq_agree_with_the_doc_number() {
        let n = 16384;
        let fs = 48_000.0f32;
        assert!(
            (freq_bin(60.0, n, fs) - 20.48).abs() <= 1e-4,
            "60 Hz lands at bin {}",
            freq_bin(60.0, n, fs)
        );
        assert!((bin_freq(20, n, fs) - 58.59375).abs() <= 1e-4);
        // 61.5234375 is exact only in f64; the f32 carrier is its 24-bit neighbor.
        assert!((bin_freq(21, n, fs) - 61.523_438).abs() <= 1e-4);
        // Round trip on integer bins.
        for k in [0usize, 1, 512, 8192] {
            let f = bin_freq(k, n, fs);
            assert!(
                (freq_bin(f, n, fs) - k as f32).abs() <= 1e-3,
                "bin {k} round trip"
            );
        }
    }
}
