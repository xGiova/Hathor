//! Pair measurements: similarity, alignment, and the null test.
//!
//! ## Two numbers, two questions (the doctrine of the kick↔bass UI)
//!
//! * [`correlation`] answers **similarity**: "how much in phase are these two,
//!   wideband, right now?" It is gain-invariant — it cannot tell the same signal
//!   at a different volume from the same signal, and that is a feature: the
//!   phase question must survive the fader.
//! * [`time_align_lag`] answers **alignment**: "shift `b` by how many samples to
//!   sit on `a`?" It is not the same measurement and must never be collapsed
//!   into one gauge: 1.0 correlation with lag 40 is a delayed copy (perfectly
//!   "similar", badly "aligned"); 0.0 with lag 0 is unrelated signals sharing a
//!   start line.
//!
//! [`null_residue_db`] is the most honest measurement in audio: push `b` against
//! `a` and *listen to what survives*. A residue of `−∞` dB means the two are the
//! same signal (scaled/inverted) — no judgment, pure arithmetic.

use crate::db::{db_to_lin, lin_to_db};

/// Normalized (Pearson) correlation of two signals: `Σ(a·b) / sqrt(Σa²·Σb²)`.
///
/// Gain-invariant (scaling `b` does not move the result — the same factor
/// appears in numerator and denominator), so it measures phase relationship,
/// not level. The common prefix is used when the lengths differ, mirroring the
/// `min` semantics of [`hathor_dsp_core::buffer::Block::mix_add`].
///
/// Silence in either signal (Σa² = 0 or Σb² = 0) returns `0.0`, never `NaN`
/// (invariant 3): a muted track is a legal, frequent input, and the meter must
/// read "no evidence", not "undefined". The result is clamped to `−1..=+1` —
/// mathematically guaranteed by Cauchy–Schwarz, floating point can overshoot by
/// an ulp, and a UI needle must not read 1.0000002. `NaN` in the input
/// propagates to a `NaN` result (a corrupt signal is not "uncorrelated").
pub fn correlation(a: &[f32], b: &[f32]) -> f32 {
    let (mut sab, mut saa, mut sbb) = (0.0f32, 0.0f32, 0.0f32);
    for (&x, &y) in a.iter().zip(b) {
        sab = x.mul_add(y, sab);
        saa = x.mul_add(x, saa);
        sbb = y.mul_add(y, sbb);
    }
    if saa == 0.0 || sbb == 0.0 {
        return 0.0;
    }
    // Sqrt each sum before multiplying: `sqrt(saa·sbb)` would overflow/underflow
    // for extremes of level where the separate roots still behave.
    let r = sab / (saa.sqrt() * sbb.sqrt());
    // `clamp` is NaN-safe by std spec: NaN fails both `<`/`>` comparisons and is
    // returned unchanged — the "corruption is visible" doctrine survives it.
    r.clamp(-1.0, 1.0)
}

/// Optimal integer lag, in samples, `b` must be *advanced* to sit on `a`.
///
/// Sign convention (invariant 4, not renegotiable): **positive = `b` delayed
/// relative to `a`**. `time_align_lag(a, delay(a, k), n) == k`. A flipped sign
/// makes the user's correction go twice as far in the wrong direction.
///
/// The naive M0 scan: a normalized cross-correlation score at every candidate
/// lag in `−max_lag..=max_lag`, maximum wins. Cost O(n·2·max_lag) (~2M
/// multiply-accumulates for n=4096, max_lag=±256 — expected): acceptable
/// **offline** or at UI rate, never per audio block. The score is normalized
/// per lag (like [`correlation`]) so that candidates with different overlap
/// lengths compete honestly: a raw sum shrinks with the overlap triangle and
/// would bias the winner towards small |lag| on noise.
///
/// Ties are won by the smaller `|lag|` (and then the positive side): with no
/// evidence the honest answer is "no movement". A lag whose overlap region is
/// silent for either signal is skipped — no basis, no vote. Empty inputs or
/// `max_lag ≤ 0` return `0`. Caveat for consumers: the winner is the
/// **maximum** correlation, so a sign-flipped copy (true lag scores −1, the
/// unshifted candidates score 0) reports lag 0 — anti-phase belongs to
/// [`null_residue_db`]'s `invert`, not to this search; inventing an alignment
/// from an anti-correlation is how automatic nudges move the wrong way.
/// This is the reference implementation forever: when M3's FFT version lands,
/// it is tested *against this one*, the same way DF2T is tested against the
/// algebraic `Coeffs::response` — two independent paths.
pub fn time_align_lag(a: &[f32], b: &[f32], max_lag: i32) -> i32 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    // Beyond `len − 1` the overlap is empty: the search cap is honesty, not
    // just speed.
    let span = (max_lag.max(0) as usize).min(a.len().max(b.len()) - 1);
    let mut best_score = f32::NEG_INFINITY;
    let mut best_lag = 0i32;
    let mut consider = |lag: i32| {
        if let Some(s) = lag_score(a, b, lag) {
            if s > best_score {
                best_score = s;
                best_lag = lag;
            }
        }
    };
    consider(0);
    for mag in 1..=span {
        // +mag before −mag: the tie-break "smaller |lag| first, then positive".
        consider(mag as i32);
        consider(-(mag as i32));
    }
    best_lag
}

/// Pearson score of `a` against `b` shifted by `lag` (positive = `b` delayed),
/// or `None` when the overlap gives no basis (empty or silent region).
fn lag_score(a: &[f32], b: &[f32], lag: i32) -> Option<f32> {
    let (mut sab, mut saa, mut sbb) = (0.0f32, 0.0f32, 0.0f32);
    if lag >= 0 {
        let l = lag as usize;
        if l >= b.len() {
            return None;
        }
        let n = a.len().min(b.len() - l);
        if n == 0 {
            return None;
        }
        for (&x, &y) in a[..n].iter().zip(&b[l..l + n]) {
            sab = x.mul_add(y, sab);
            saa = x.mul_add(x, saa);
            sbb = y.mul_add(y, sbb);
        }
    } else {
        let l = lag.unsigned_abs() as usize;
        if l >= a.len() {
            return None;
        }
        let n = b.len().min(a.len() - l);
        if n == 0 {
            return None;
        }
        for (&x, &y) in a[l..l + n].iter().zip(&b[..n]) {
            sab = x.mul_add(y, sab);
            saa = x.mul_add(x, saa);
            sbb = y.mul_add(y, sbb);
        }
    }
    if saa == 0.0 || sbb == 0.0 {
        return None;
    }
    Some(sab / (saa.sqrt() * sbb.sqrt()))
}

/// Peak in dBFS of `a + b·g` (or `a − b·g` when `invert`), with `g` from
/// `gain_db`.
///
/// The null test — the kick↔bass acceptance ("residue < −30 dB in 40-120 Hz",
/// band-limited by pre-filtering both signals with `hathor-dsp-filters` when
/// that consumer arrives) and the transparency check of any processor:
/// processor out vs dry `a` with `invert=true, gain_db=0.0` must read `−∞`.
/// Per-band null: filter first, then this function; the measurement stays
/// honest because the *same* window of both signals goes in.
///
/// The common prefix is used when the lengths differ. Residue is a **peak**
/// measurement (one surviving click is not "low average"): silence reads
/// `f32::NEG_INFINITY` (invariant 2), and any `NaN` sample propagates.
pub fn null_residue_db(a: &[f32], b: &[f32], gain_db: f32, invert: bool) -> f32 {
    let g = db_to_lin(gain_db);
    let g = if invert { -g } else { g };
    let mut peak = 0.0f32;
    for (&x, &y) in a.iter().zip(b) {
        let r = g.mul_add(y, x).abs();
        if r > peak {
            peak = r;
        } else if r.is_nan() {
            peak = f32::NAN;
        }
    }
    lin_to_db(peak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    /// Deterministic RNG: numeric tests must not change from run to run
    /// (same Lcg as the filters tests — CONVENTION §4.4).
    struct Lcg(u32);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((self.0 >> 8) as f32 / 8_388_608.0) - 1.0
        }
    }

    fn cos_sine(n: usize, sr: f32, freq: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / sr).cos())
            .collect()
    }

    fn delay(x: &[f32], k: usize) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        if k < x.len() {
            y[k..].copy_from_slice(&x[..x.len() - k]);
        }
        y
    }

    #[test]
    fn correlation_identical_signals_is_one() {
        let a = [1.0f32, 2.0, 3.0, 4.0];
        let b = [1.0f32, 2.0, 3.0, 4.0];
        // Σab = Σa² = Σb² = 30; sqrt(30·30) = 30; 30/30 = 1 exactly.
        assert!((correlation(&a, &b) - 1.0).abs() < 1e-7);
    }

    #[test]
    fn correlation_anti_phase_is_minus_one() {
        // Interpretation caveat: −1 is not "wrong", it is perfect anti-phase —
        // for the null test that is the *greatest success*.
        let a = [1.0f32, 2.0, 3.0, 4.0];
        let b = [-1.0f32, -2.0, -3.0, -4.0];
        assert!((correlation(&a, &b) + 1.0).abs() < 1e-7);
    }

    #[test]
    fn correlation_orthogonal_is_zero_exactly() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        assert_eq!(correlation(&a, &b), 0.0);
    }

    #[test]
    fn correlation_silence_returns_zero_not_nan() {
        // Invariant 3: a muted track is legal. Σb² = 0 → 0.0.
        let a = [1.0f32, 2.0, 3.0];
        let silent = [0.0f32; 3];
        assert_eq!(correlation(&a, &silent), 0.0, "muted channel must read 0");
        assert_eq!(correlation(&silent, &silent), 0.0);
        assert_eq!(
            correlation(&[], &[]),
            0.0,
            "empty input is legal, not illegal"
        );
    }

    #[test]
    fn correlation_is_gain_invariant() {
        // Exercise 3: doubling b leaves Σb / sqrt(Σa²·Σb²) unchanged — a
        // correlation meter cannot distinguish "the same signal at a different
        // volume" from "the same signal", and that is exactly why it measures
        // phase, not level.
        let a = cos_sine(2048, 48_000.0, 100.0);
        let b: Vec<f32> = a.iter().map(|x| 0.3 * x).collect();
        let dbl: Vec<f32> = b.iter().map(|x| 2.0 * x).collect();
        let r1 = correlation(&a, &b);
        let r2 = correlation(&a, &dbl);
        assert!((r1 - 1.0).abs() < 1e-5, "b = 0.3·a must read 1, got {r1}");
        assert!((r1 - r2).abs() < 1e-6, "doubling b moved r: {r1} → {r2}");
    }

    #[test]
    fn correlation_sign_flip_flips_the_result() {
        let a = cos_sine(1024, 48_000.0, 50.0);
        let mut b = a.clone();
        for s in b.iter_mut() {
            *s = -*s;
        }
        // Two 50 Hz sinusoids in opposite phase: correlation = −1 (exercise 2).
        assert!((correlation(&a, &b) + 1.0).abs() < 1e-5);
    }

    #[test]
    fn correlation_uses_the_common_prefix_when_lengths_differ() {
        let a = [1.0f32, 1.0];
        let b = [1.0f32, 1.0, 999.0];
        assert!(
            (correlation(&a, &b) - 1.0).abs() < 1e-7,
            "prefix [1,1] vs [1,1] must read 1; the tail of b is ignored"
        );
    }

    #[test]
    fn correlation_nan_propagates() {
        let a = [1.0f32, f32::NAN, 3.0];
        let b = [1.0f32, 2.0, 3.0];
        assert!(correlation(&a, &b).is_nan());
    }

    #[test]
    fn time_align_lag_impulse_returns_exactly_seven() {
        // Exercise 1 / invariant 5: the answer is exact, not "approximately".
        let mut a = vec![0.0f32; 512];
        a[100] = 1.0;
        let b = delay(&a, 7);
        assert_eq!(time_align_lag(&a, &b, 64), 7, "b = delay(a, 7) must read 7");
    }

    #[test]
    fn time_align_lag_impulse_under_noise_still_reads_seven() {
        // Exercise 1 continued: −20 dB peak noise (uniform in ±0.1) on both
        // signals. Deterministic Lcg (seed 0xC0FFEE): the true lag wins by a
        // wide margin — Σab's coherent spike (1.0) versus noise-vs-noise
        // sums of std ≈ sqrt(505)·0.0033 ≈ 0.074 before normalization.
        let mut a = vec![0.0f32; 512];
        a[100] = 1.0;
        let b = delay(&a, 7);
        let mut g = Lcg(0xC0FFEE);
        let na: Vec<f32> = (0..512).map(|_| g.next_f32() * 0.1).collect();
        let nb: Vec<f32> = (0..512).map(|_| g.next_f32() * 0.1).collect();
        let a2: Vec<f32> = a.iter().zip(&na).map(|(x, n)| x + n).collect();
        let b2: Vec<f32> = b.iter().zip(&nb).map(|(x, n)| x + n).collect();
        assert_eq!(
            time_align_lag(&a2, &b2, 64),
            7,
            "±0.1 noise must not move the impulse lag"
        );
    }

    #[test]
    fn time_align_lag_anti_phase_impulse_reports_zero_flip_it_and_it_reports_seven() {
        // b = −delay(a, 7): the scan is **maximum wins** (the spec), and the
        // correlation at the true lag is −1.0 — *less* than the 0.0 of every
        // unshifted candidate — so the honest answer of this measurement is
        // lag 0, not 7. Anti-phase is the null test's success
        // (`null_residue_db` with `invert`), not the lag finder's; a tool
        // that searched `max|score|` would invent an alignment the signals
        // don't have. Flipping b's sign turns the same pair into the exact
        // match the first impulse test already pins.
        let mut a = vec![0.0f32; 512];
        a[100] = 1.0;
        let b: Vec<f32> = delay(&a, 7).iter().map(|x| -x).collect();
        assert_eq!(
            time_align_lag(&a, &b, 64),
            0,
            "max-wins on correlations must not call anti-phase 'aligned at 7'"
        );
        let b_flip: Vec<f32> = b.iter().map(|x| -x).collect();
        assert_eq!(
            time_align_lag(&a, &b_flip, 64),
            7,
            "the same pair with its sign honest reads 7"
        );
    }

    #[test]
    fn time_align_lag_delayed_sine_returns_exactly_33() {
        // Invariant 5: sine delayed by k samples → exactly k. 100 periods over
        // 4096 frames at 48 kHz, b = delay(a, 33) (first 33 frames silent).
        let a: Vec<f32> = (0..4096)
            .map(|i| (2.0 * PI * 100.0 * (i + 33) as f32 / 4096.0).sin())
            .collect();
        let b = delay(&a, 33);
        assert_eq!(time_align_lag(&a, &b, 64), 33);
    }

    #[test]
    fn time_align_lag_sign_convention_is_b_delayed_positive() {
        // Invariant 4. The same fact from the other side: if `a` is the
        // delayed copy, the lag is negative.
        let src: Vec<f32> = (0..1024)
            .map(|i| (2.0 * PI * 440.0 * i as f32 / 48_000.0).sin())
            .collect();
        let b = src.clone();
        let a = delay(&src, 9); // a = delay(b, 9) → b is AHEAD → negative lag
        assert_eq!(
            time_align_lag(&a, &b, 32),
            -9,
            "positive must mean b delayed"
        );
    }

    #[test]
    fn time_align_lag_silence_and_degenerate_inputs_return_zero() {
        let z = vec![0.0f32; 256];
        assert_eq!(time_align_lag(&z, &z, 16), 0, "no evidence → no movement");
        assert_eq!(time_align_lag(&[], &z, 16), 0);
        assert_eq!(time_align_lag(&z, &[], 16), 0);
        let a = vec![1.0f32; 256];
        assert_eq!(time_align_lag(&a, &z, 16), 0, "b silent → no candidate");
    }

    #[test]
    fn time_align_lag_respects_max_lag() {
        // True lag 100, searched window ±20: whatever wins, it must be inside
        // the window — the scan never reports a lag it did not examine.
        let src: Vec<f32> = (0..4096)
            .map(|i| (2.0 * PI * 50.0 * i as f32 / 48_000.0).sin())
            .collect();
        let b = delay(&src, 100);
        let lag = time_align_lag(&src, &b, 20);
        assert!(lag.abs() <= 20, "lag {lag} outside the searched window ±20");
        // max_lag = 0 examines only lag 0 → 0. Negative max_lag = no search.
        assert_eq!(time_align_lag(&src, &b, 0), 0);
        assert_eq!(time_align_lag(&src, &b, -5), 0);
    }

    #[test]
    fn null_residue_of_a_signal_with_itself_inverted_is_minus_infinity() {
        // The transparency test: "did I touch nothing?" → −∞ dB, no magic
        // floor (invariant 2). −1 is not "wrong" here, it is perfect success.
        let a = [1.0f32, -0.5, 0.25, 0.9];
        let b = [1.0f32, -0.5, 0.25, 0.9];
        let r = null_residue_db(&a, &b, 0.0, true);
        assert!(r == f32::NEG_INFINITY, "perfect null must read −∞, got {r}");
    }

    #[test]
    fn null_residue_without_invert_doubles_the_signal() {
        // a = b full scale, invert off, 0 dB → residue = 2a → peak 2.0 →
        // +6.020599913279624 dBFS (20·log10(2)).
        let a = [1.0f32, -1.0];
        let r = null_residue_db(&a, &a, 0.0, false) as f64;
        assert!(
            (r - 6.020599913279624).abs() < 2e-4,
            "a+b of two full-scale copies → {r:.6}, expected +6.0206"
        );
    }

    #[test]
    fn null_residue_applies_gain_db_to_b() {
        // b = 2·a: scaling b by 0.5 (−6.0206 dB ≈ the f32-representable form of
        // 20·log10(0.5)) and inverting nulls it.
        let a = [1.0f32, 2.0, -0.5];
        let b = [2.0f32, 4.0, -1.0];
        let r = null_residue_db(&a, &b, -6.0206, true);
        assert!(
            r < -100.0,
            "gain-corrected null should residue to numerical dust, got {r:.1} dB"
        );
    }

    #[test]
    fn null_residue_silence_is_minus_infinity() {
        let z = [0.0f32; 8];
        assert!(null_residue_db(&z, &z, 0.0, true) == f32::NEG_INFINITY);
        assert!(null_residue_db(&[], &[], 0.0, false) == f32::NEG_INFINITY);
    }

    #[test]
    fn null_residue_uses_common_prefix() {
        // a = [1,1] ⊕ unpaired tail in b: prefix nulls perfectly → −∞; the
        // extra b samples are outside the measurement window.
        let a = [1.0f32, 1.0];
        let b = [1.0f32, 1.0, 1.0, 1.0];
        assert!(null_residue_db(&a, &b, 0.0, true) == f32::NEG_INFINITY);
    }

    #[test]
    fn null_residue_nan_propagates() {
        let a = [f32::NAN, 1.0];
        let b = [0.0f32, 1.0];
        assert!(null_residue_db(&a, &b, 0.0, true).is_nan());
    }

    #[test]
    fn exercise_2_numbers_one_millisecond_off() {
        // Two 50 Hz sinusoids, one inverted and delayed by 1 ms (48 samples
        // at 48 kHz = 18°). Closed forms over an integer number of periods:
        //   correlation(a, −delay(a, 48)) = −cos(18°) = −0.951056516…
        //   residue peak |cos θ − cos(θ−18°)| = 2·sin(9°) = 0.3128689…
        //     → 20·log10 = −10.093 dBFS
        // (the spec's "~−18 dB" prose in the Exercises section does not match
        // this trigonometry — reported as doc drift; the numbers here are the
        // derived closed forms, evaluated in f64 as an independent reference.)
        let sr = 48_000.0;
        let n = 4800; // 5 periods: Σcos(2θ−δ) vanishes exactly
        let a = cos_sine(n, sr, 50.0);
        let b: Vec<f32> = (0..n)
            .map(|i| -(2.0 * PI * 50.0 * (i as f32 - 48.0) / sr).cos())
            .collect();
        let r = correlation(&a, &b) as f64;
        let want = -(18.0_f64 * core::f64::consts::PI / 180.0).cos();
        assert!(
            (r - (-0.9510565162951535_f64)).abs() < 1e-3,
            "correlation → {r}, expected −cos(18°) = −0.95106 (f64 check: {want})"
        );
        let resid = null_residue_db(&a, &b, 0.0, false) as f64; // a + b
        let amp = 2.0 * (9.0_f64 * core::f64::consts::PI / 180.0).sin(); // 0.3128689…
        let want_db = 20.0 * amp.log10(); // −10.0928…
        assert!(
            (resid - want_db).abs() < 0.02,
            "residue → {resid:.4} dB, expected {want_db:.4} dB (20·log10(2·sin 9°))"
        );
    }
}
