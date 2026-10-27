//! Test-only helpers shared by the module test blocks.
//!
//! Not part of the public API (`#[cfg(test)] mod`, never `pub use`): the crate's
//! numerical tests all need deterministic noise (CONVENTION §4.4: a numeric test
//! that is not reproducible is not a test), and duplicating the generator per file
//! is the filters crate's pattern but three files is where a shared one starts
//! paying for itself.

use crate::complex::Complex;

/// Deterministic RNG: the same `Lcg` the filters tests use (multiply-add-1013904223,
/// 24-bit extraction), so a seeded noise vector means the same samples in every
/// Hathor numeric test.
pub struct Lcg(pub u32);

impl Lcg {
    /// Next value in [-1, 1).
    pub fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.0 >> 8) as f32 / 8_388_608.0) - 1.0
    }
}

/// `n` deterministic samples in [-1, 1).
pub fn noise_f32(rng: &mut Lcg, n: usize) -> Vec<f32> {
    (0..n).map(|_| rng.next_f32()).collect()
}

/// `n` deterministic complex samples, both parts in [-1, 1).
pub fn noise_complex(rng: &mut Lcg, n: usize) -> Vec<Complex> {
    (0..n)
        .map(|_| Complex::new(rng.next_f32(), rng.next_f32()))
        .collect()
}

/// Complex exponential `amp·e^{2πi·k0·j/n}` — the exact-bin tone. The angle is
/// computed in f64 and narrowed once so the tone is *exactly* periodic in the
/// frame: an f32 phase walk would drift and fake spectral leakage into the tests.
pub fn tone(n: usize, k0: usize, amp: f32) -> Vec<Complex> {
    use std::f64::consts::TAU;
    (0..n)
        .map(|j| {
            let angle = TAU * k0 as f64 * j as f64 / n as f64;
            let (s, c) = angle.sin_cos();
            Complex::new(c as f32 * amp, s as f32 * amp)
        })
        .collect()
}

/// Periodic Hann window sample `w(j) = ½ − ½cos(2πj/n)`, f64: the analytic test
/// references quote exact three-line spectra, and the window that builds the input
/// must not be the fuzzy part.
pub fn hann(j: usize, n: usize) -> f64 {
    0.5 - 0.5 * (std::f64::consts::TAU * j as f64 / n as f64).cos()
}
