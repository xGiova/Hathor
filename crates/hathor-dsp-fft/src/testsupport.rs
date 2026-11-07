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

/// Sample `idx` of a full-scale sine at `freq` Hz: phase from the **absolute**
/// sample index, so chunked driving never accumulates phase and the tone is
/// exactly the one the analytic reference describes.
pub fn sine_frame(freq: f32, idx: u64, sample_rate: f32) -> f32 {
    (std::f64::consts::TAU * freq as f64 * idx as f64 / sample_rate as f64).sin() as f32
}

/// |X[k]| of the **Hann-windowed** frame, summed directly in f64:
/// `|Σ w(j)·x(j)·e^{−2πikj/n}|`. The bank tests' independent reference — a bank
/// expectation built on our own FFT would share its bugs the same way two copies
/// of a wrong constant do. O(n) per bin, and every test calls it for a handful of
/// bins, which is why the finest tier never appears in a full direct transform.
pub fn direct_hann_bin(frame: &[f32], n: usize, k: usize) -> f64 {
    assert_eq!(frame.len(), n, "the direct reference needs the full frame");
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    for (j, &x) in frame.iter().enumerate() {
        let angle = -std::f64::consts::TAU * k as f64 * j as f64 / n as f64;
        let w = hann(j, n);
        let (s, c) = angle.sin_cos();
        let y = x as f64 * w;
        re += y * c;
        im += y * s;
    }
    re.hypot(im)
}

/// Independent prediction for one shared log bin of one tier: the axis mapping
/// (log position + two-point spread) re-derived here in f64, fed with direct-DFT
/// bin magnitudes — never an FFT, never our kernel. The bank's cross-tier rule is
/// applied by the caller as a max; this function is ONE tier.
pub fn expected_tier_bin(
    stream: &[f32],
    tier_n: usize,
    sample_rate: f32,
    i: usize,
    bins: usize,
) -> f64 {
    let norm = (4.0 / tier_n as f64).powi(2);
    let frame = &stream[stream.len() - tier_n..];
    let mut total = 0.0f64;
    for k in 1..=tier_n / 2 {
        let f = k as f64 * sample_rate as f64 / tier_n as f64;
        if !(crate::bank::AXIS_F_LO as f64..=crate::bank::AXIS_F_HI as f64).contains(&f) {
            continue;
        }
        let p = crate::bank::log_position(f, bins).clamp(0.0, (bins - 1) as f64);
        let i0 = p as usize;
        let frac = p - i0 as f64;
        let (wi0, wi1) = if i0 + 1 < bins && frac > 0.0 {
            (1.0 - frac, frac)
        } else {
            (1.0, 0.0)
        };
        let w = if i0 == i {
            wi0
        } else if i0 + 1 == i {
            wi1
        } else {
            continue;
        };
        let x = direct_hann_bin(frame, tier_n, k);
        total += x * x * norm * w;
    }
    total
}

/// Parameters for [`mask_threshold_oracle`], mirroring the `mask.rs` module doc's
/// formula so the oracle reads straight from the specification, not the code.
pub struct MaskingOracleParams {
    /// Skirt width upward (masker below a bin), in bins.
    pub up: f32,
    /// Skirt width downward (masker above a bin), in bins.
    pub down: f32,
    /// Masker gain.
    pub weight: f32,
    /// Absolute threshold as a linear power (`10^(floor_db/10)`).
    pub at: f64,
    /// Truncation radius `D` from the doc formula, recomputed independently.
    pub d_max: usize,
}

/// The masked threshold `T(k)` of the `mask.rs` doc, summed **per target bin and
/// per distance** in f64 with a fresh `exp()` on every term — deliberately the
/// opposite iteration order and a different code path from the meter's
/// table-driven [`accumulate`](crate::mask::accumulate) (which iterates maskers
/// and reads precomputed weights). House law: the oracle never reuses the
/// implementation-under-test's helper, so a shared bug can't hide in a shared
/// table. Out-of-axis distances are skipped, `d = 0` never contributes (no
/// self-masking), and a zero width disables that skirt side.
pub fn mask_threshold_oracle(x: &[f32], o: &MaskingOracleParams) -> Vec<f64> {
    let n = x.len();
    let mut t = vec![0.0f64; n];
    for (k, tk) in t.iter_mut().enumerate() {
        let mut acc = o.at;
        for d in 1..=o.d_max {
            // Masker d bins *below* k: upward skirt W↑(d) = weight·exp(−d/up).
            if o.up > 0.0 && d <= k {
                let m = x[k - d] as f64;
                if m > 0.0 {
                    acc += m * (o.weight as f64) * (-(d as f64) / o.up as f64).exp();
                }
            }
            // Masker d bins *above* k: downward skirt W↓(d) = weight·exp(−d/down).
            if o.down > 0.0 && k + d < n {
                let m = x[k + d] as f64;
                if m > 0.0 {
                    acc += m * (o.weight as f64) * (-(d as f64) / o.down as f64).exp();
                }
            }
        }
        *tk = acc;
    }
    t
}
