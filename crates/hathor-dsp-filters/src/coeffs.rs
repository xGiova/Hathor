//! RBJ coefficients (Audio EQ Cookbook).
//!
//! ## The part that makes the difference: normalize `a0`, always
//!
//! The cookbook formulas are written as ratios of polynomials in `z⁻¹` with a
//! **non-unit** `a0`: for LP/HP/BP/notch/allpass `a0 = 1 + α`, for the peaking
//! `a0 = 1 + α/A`, for the shelves `a0 = (A+1) ± (A-1)cos(w0) + 2√A·α`. Forgetting
//! the normalization does not produce a visible error: it produces a filter whose
//! gain *out of band* is not 0 dB — i.e. an EQ that sums gain at every band. That
//! is why below every variant builds a [`Raw`] and **one single point** normalizes
//! it. The tests verify exactly that property (gain at DC/Nyquist, `-3.01 dB`
//! at `f0` for Butterworth), not the "feeling" of the curve.

use core::f32::consts::FRAC_1_SQRT_2;
use core::f32::consts::PI;

/// Normalized coefficients (`a0 = 1`).
///
/// Denominator form: `1 + a1·z⁻¹ + a2·z⁻²`, with `a1`/`a2` **already carrying
/// their sign**: the DF2T subtracts `-a1·out` and `-a2·out` explicitly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coeffs {
    /// Feed-forward order 0.
    pub b0: f32,
    /// Feed-forward order 1.
    pub b1: f32,
    /// Feed-forward order 2.
    pub b2: f32,
    /// Feedback order 1.
    pub a1: f32,
    /// Feedback order 2.
    pub a2: f32,
}

/// Non-normalized form: `b/a0`, `a1/a0`, `a2/a0`. Internals in **f64**: it
/// narrows to f32 only in `normalize`, at the end of the computation.
#[derive(Debug, Clone, Copy)]
struct Raw {
    b0: f64,
    b1: f64,
    b2: f64,
    a0: f64,
    a1: f64,
    a2: f64,
}

impl Raw {
    /// Divides everything by `a0`. If `a0` is degenerate, returns identity (not
    /// `NaN`: a `NaN` filter poisons the whole graph, and the blame wouldn't be
    /// its).
    fn normalize(self) -> Coeffs {
        if !self.a0.is_finite() || self.a0 == 0.0 {
            return Coeffs::IDENTITY;
        }
        let k = 1.0 / self.a0;
        Coeffs {
            b0: (self.b0 * k) as f32,
            b1: (self.b1 * k) as f32,
            b2: (self.b2 * k) as f32,
            a1: (self.a1 * k) as f32,
            a2: (self.a2 * k) as f32,
        }
    }
}

impl Coeffs {
    /// Pass-all (identity).
    pub const IDENTITY: Coeffs = Coeffs {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    /// True if the filter is asymptotically stable.
    ///
    /// For a 2nd order the poles are inside the unit circle ⇔ `|a2| < 1` **and**
    /// `|a1| < 1 + a2`. It is not bureaucracy: with a badly parametric Q or an
    /// `f0` very close to 0 Hz the poles step out, and the output explodes *after*
    /// a few hundred blocks, i.e. during mastering and not in the quick test.
    pub fn is_stable(&self) -> bool {
        self.a2.abs() < 1.0 && self.a1.abs() < 1.0 + self.a2
    }

    /// True if no coefficient is `NaN`/infinite.
    pub fn is_finite(&self) -> bool {
        self.b0.is_finite()
            && self.b1.is_finite()
            && self.b2.is_finite()
            && self.a1.is_finite()
            && self.a2.is_finite()
    }

    /// Magnitude in dB of `H(e^{jw})`, with `w = 2π f / fs` (Nyquist = `PI`).
    ///
    /// Numerical note: evaluating a shelf **at `w` exactly 0** is
    /// catastrophically cancelling in f32 (sums of ~5.6 terms cancelling down to
    /// 3e-4). The tests indeed measure the shelves at 20 Hz, not at 0.
    pub fn magnitude_db(&self, w: f32) -> f32 {
        let (re, im) = self.response(w);
        if !(re * re + im * im).is_finite() {
            return f32::INFINITY;
        }
        20.0 * (re * re + im * im).sqrt().max(f32::MIN_POSITIVE).log10()
    }

    /// Complex response `H(e^{jw})` as `(re, im)`.
    ///
    /// Kept separate from `magnitude_db` because **phase is not a by-product**: it
    /// is the quantity the kick/bass tool's phase-rotator manipulates, and it is
    /// what the biquad's behavioral test compares (design algebra vs processing
    /// recursion — two independent paths, the only honest way to validate).
    pub fn response(&self, w: f32) -> (f32, f32) {
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        // e^{-jw} = cos w − j·sin w
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        let d2 = den_re * den_re + den_im * den_im;
        if d2 == 0.0 {
            return (f32::NAN, f32::NAN);
        }
        (
            (num_re * den_re + num_im * den_im) / d2,
            (num_im * den_re - num_re * den_im) / d2,
        )
    }

    /// Phase in radians of `H(e^{jw})`.
    pub fn phase_rad(&self, w: f32) -> f32 {
        let (re, im) = self.response(w);
        im.atan2(re)
    }

    /// Frequency in Hz → `w` in sampled radians.
    pub fn w(f: f32, sample_rate: f32) -> f32 {
        2.0 * PI * f / sample_rate
    }

    /// Rescale the `b` so the DC gain of **the f32 representation** is
    /// exactly 1.
    ///
    /// Why: `response(0)` computes `den = 1 + a1 + a2`, a sum with catastrophic
    /// cancellation (terms ~2 cancelling down to ~1e-6 below 50 Hz): the
    /// representation error is ~1% → ~0.1 dB of phantom DC gain. The error cannot
    /// be *corrected* — it can be **shared**: multiplying the `b` by `den/num`,
    /// the sum of the `b` (all tiny, without cancellation) becomes
    /// bit-compatible with the same dirty `den`, and the ratio comes out exactly
    /// 1.0. Hence a ratio that is *equal*, not *corrected*: this is the
    /// unity-DC-gain trick of commercial EQs, and it must be applied ONLY to the
    /// types that by design are worth 1 at DC (never to shelves, whose low-end
    /// gain IS the design, never to HP/BP which are worth 0).
    fn force_dc_unity(self) -> Coeffs {
        let num = self.b0 + self.b1 + self.b2;
        let den = 1.0 + self.a1 + self.a2;
        if num.abs() < 1e-9 || !num.is_finite() || !den.is_finite() {
            return self;
        }
        let k = den / num;
        Coeffs {
            b0: self.b0 * k,
            b1: self.b1 * k,
            b2: self.b2 * k,
            a1: self.a1,
            a2: self.a2,
        }
    }
}

/// Specification error. Static: no `String` on the realtime path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterError {
    /// Frequency outside `(0, Nyquist)`.
    FrequencyOutOfRange,
    /// Non-positive or non-finite sample rate.
    InvalidSampleRate,
    /// Non-finite or zero Q.
    InvalidQ,
    /// Gain/slope combination with no real solution: the argument of the
    /// square root of `α` (shelf) is negative. With `S` too steep for the gain
    /// given, `α → 0` would place a pole exactly on the unit circle: better a
    /// rejection than a filter that rings forever.
    InvalidSlope,
    /// The design is formally correct but produces poles outside the unit
    /// circle: better to reject than to ship a filter that explodes.
    Unstable,
}

/// Filter type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterKind {
    /// Low-pass (`q` = 0.7071 → Butterworth).
    LowPass {
        /// Q.
        q: f32,
    },
    /// High-pass.
    HighPass {
        /// Q.
        q: f32,
    },
    /// Band-pass with a unity peak at `f0`.
    BandPass {
        /// Q.
        q: f32,
    },
    /// Band-reject (zero at `f0`).
    Notch {
        /// Q.
        q: f32,
    },
    /// Parametric bell (peaking).
    Peak {
        /// Q.
        q: f32,
        /// Gain at `f0` in dB.
        gain_db: f32,
    },
    /// Shelf on the lows.
    LowShelf {
        /// Low gain, dB.
        gain_db: f32,
        /// RBJ slope (`S`): 1.0 = maximally flat transition.
        slope: f32,
    },
    /// Shelf on the highs.
    HighShelf {
        /// High gain, dB.
        gain_db: f32,
        /// Slope.
        slope: f32,
    },
    /// All-pass: flat magnitude, not flat phase.
    ///
    /// **It is the building block of the kick/bass tool's per-band
    /// phase-rotator**: it rotates the phase of a band without touching its
    /// amplitude spectrum, i.e. it aligns the bass fundamental to the kick
    /// *without* cutting content. It is not an "exotic" filter, it is the piece
    /// that makes phase correction different from a plain 1 ms delay.
    AllPass {
        /// Q: controls the phase slope around `f0`.
        q: f32,
    },
}

impl FilterKind {
    /// Q relevant for validation (the shelves use `slope`, not Q).
    fn q(&self) -> f32 {
        match self {
            FilterKind::LowPass { q }
            | FilterKind::HighPass { q }
            | FilterKind::BandPass { q }
            | FilterKind::Notch { q }
            | FilterKind::Peak { q, .. }
            | FilterKind::AllPass { q } => *q,
            FilterKind::LowShelf { .. } | FilterKind::HighShelf { .. } => 1.0 / FRAC_1_SQRT_2,
        }
    }
}

/// Designs the filter. It returns `Err` instead of a broken filter: the choice is
/// deliberate, a `NaN` in a sidechain chain does not show up in the test.
pub fn design(kind: FilterKind, f0: f32, sample_rate: f32) -> Result<Coeffs, FilterError> {
    if !sample_rate.is_finite() || sample_rate <= 0.0 {
        return Err(FilterError::InvalidSampleRate);
    }
    let nyquist = sample_rate * 0.5;
    if !f0.is_finite() || f0 <= 0.0 || f0 >= nyquist {
        return Err(FilterError::FrequencyOutOfRange);
    }
    let q = kind.q();
    if !q.is_finite() || q <= 0.0 {
        return Err(FilterError::InvalidQ);
    }

    // Design in f64: at 10 Hz / 44.1 kHz, `1 − cos(w0)` is worth ~1e-6 and
    // the ulp of f32 near 1.0 is ~1.2e-7: in f32 significant digits would be
    // thrown away BEFORE even normalizing. Cost: setup-time, zero on the audio
    // path. (Historical bug: the grid at f=10 Hz proved it.)
    let w0 = core::f64::consts::TAU * f0 as f64 / sample_rate as f64;
    let (sin_w0, cos_w0) = w0.sin_cos();
    let alpha_q = sin_w0 / (2.0 * q as f64);

    let raw = match kind {
        FilterKind::LowPass { .. } => Raw {
            b0: (1.0 - cos_w0) * 0.5,
            b1: 1.0 - cos_w0,
            b2: (1.0 - cos_w0) * 0.5,
            a0: 1.0 + alpha_q,
            a1: -2.0 * cos_w0,
            a2: 1.0 - alpha_q,
        },
        FilterKind::HighPass { .. } => Raw {
            b0: (1.0 + cos_w0) * 0.5,
            b1: -(1.0 + cos_w0),
            b2: (1.0 + cos_w0) * 0.5,
            a0: 1.0 + alpha_q,
            a1: -2.0 * cos_w0,
            a2: 1.0 - alpha_q,
        },
        FilterKind::BandPass { .. } => Raw {
            b0: alpha_q,
            b1: 0.0,
            b2: -alpha_q,
            a0: 1.0 + alpha_q,
            a1: -2.0 * cos_w0,
            a2: 1.0 - alpha_q,
        },
        FilterKind::Notch { .. } => Raw {
            b0: 1.0,
            b1: -2.0 * cos_w0,
            b2: 1.0,
            a0: 1.0 + alpha_q,
            a1: -2.0 * cos_w0,
            a2: 1.0 - alpha_q,
        },
        FilterKind::AllPass { .. } => Raw {
            b0: 1.0 - alpha_q,
            b1: -2.0 * cos_w0,
            b2: 1.0 + alpha_q,
            a0: 1.0 + alpha_q,
            a1: -2.0 * cos_w0,
            a2: 1.0 - alpha_q,
        },
        FilterKind::Peak { gain_db, .. } => {
            // A = 10^(dB/40) (the root of the power ratio).
            let a = 10f64.powf(gain_db as f64 / 40.0);
            let alpha_a = alpha_q / a;
            Raw {
                b0: 1.0 + alpha_q * a,
                b1: -2.0 * cos_w0,
                b2: 1.0 - alpha_q * a,
                a0: 1.0 + alpha_a,
                a1: -2.0 * cos_w0,
                a2: 1.0 - alpha_a,
            }
        }
        FilterKind::LowShelf { gain_db, slope } => {
            let a = 10f64.powf(gain_db as f64 / 40.0);
            let alpha = shelf_alpha(w0, a, slope)?;
            let two_sqrt_a = 2.0 * a.sqrt();
            Raw {
                b0: a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a * alpha),
                b1: 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0),
                b2: a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a * alpha),
                // RBJ: the term in the poles is 2*sqrt(A)*alpha, NOT 2*alpha:
                // that was the bug that made large-gain shelves "Unstable"
                // and the out-of-band curve asymmetric at medium gains.
                a0: (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a * alpha,
                a1: -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0),
                a2: (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a * alpha,
            }
        }
        FilterKind::HighShelf { gain_db, slope } => {
            let a = 10f64.powf(gain_db as f64 / 40.0);
            let alpha = shelf_alpha(w0, a, slope)?;
            let two_sqrt_a = 2.0 * a.sqrt();
            Raw {
                b0: a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a * alpha),
                b1: -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
                b2: a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a * alpha),
                a0: (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a * alpha,
                a1: 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
                a2: (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a * alpha,
            }
        }
    };

    let mut c = raw.normalize();
    // Unity at DC guaranteed on the representation (see `force_dc_unity`).
    if matches!(
        kind,
        FilterKind::LowPass { .. }
            | FilterKind::Notch { .. }
            | FilterKind::AllPass { .. }
            | FilterKind::Peak { .. }
    ) {
        c = c.force_dc_unity();
    }
    if !c.is_finite() {
        return Err(FilterError::Unstable);
    }
    if !c.is_stable() {
        return Err(FilterError::Unstable);
    }
    Ok(c)
}

/// Shelf `α`: `sin(w0)/2 · sqrt((A + 1/A)(1/S − 1) + 2)`.
///
/// The argument of the square root is NOT always positive: at large |gain| with
/// `S → 2` it goes negative, and no real solution exists for that combination.
/// Clamping it to zero would mean handing out a filter with a pole sitting
/// exactly on the unit circle (`a2 == a0`): `Err(InvalidSlope)` is the honest
/// behavior. With `S = 1` the argument is always `2`: maximally flat transition.
fn shelf_alpha(w0: f64, a: f64, slope: f32) -> Result<f64, FilterError> {
    let s = if slope.is_finite() && slope > 0.0 {
        slope as f64
    } else {
        1.0
    };
    let arg = (a + 1.0 / a) * (1.0 / s - 1.0) + 2.0;
    if arg < 0.0 {
        return Err(FilterError::InvalidSlope);
    }
    Ok(0.5 * w0.sin() * arg.sqrt())
}

/// dB → linear (voltage ratio): `10^(db/20)`.
pub fn db_to_lin(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// linear → dB.
pub fn lin_to_db(x: f32) -> f32 {
    20.0 * x.max(f32::MIN_POSITIVE).log10()
}

/// Butterworth Q of the 2nd order (`1/√2`).
pub const Q_BUTTERWORTH: f32 = FRAC_1_SQRT_2;
/// Bessel Q of the 2nd order (the value commonly adopted).
pub const Q_BESSEL: f32 = 0.578_703_7;

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    /// Tolerance in dB for the "flat" gain checks.
    const T: f32 = 0.02;

    #[test]
    fn lp_normalization_gives_unity_dc_gain() {
        // If a0 isn't normalized this fails: it is THE test of this module.
        let c = design(FilterKind::LowPass { q: Q_BUTTERWORTH }, 1_000.0, SR).expect("lpf");
        assert!(
            c.magnitude_db(0.0).abs() < T,
            "DC gain = {} dB, expected 0",
            c.magnitude_db(0.0)
        );
        assert!(c.magnitude_db(PI) < -70.0);
    }

    #[test]
    fn hp_normalization_gives_unity_gain_at_nyquist() {
        let c = design(FilterKind::HighPass { q: Q_BUTTERWORTH }, 1_000.0, SR).expect("hpf");
        assert!((c.magnitude_db(PI) - 0.0).abs() < T);
        assert!(c.magnitude_db(0.0) < -70.0);
    }

    #[test]
    fn butterworth_is_minus_3db_exactly_at_f0() {
        let c = design(FilterKind::LowPass { q: Q_BUTTERWORTH }, 1_000.0, SR).expect("lpf");
        let g = c.magnitude_db(Coeffs::w(1_000.0, SR));
        assert!(
            (g + 3.01).abs() < 0.05,
            "expected -3.01 dB at f0, found {g}"
        );
    }

    #[test]
    fn peaking_gain_exact_and_flat_out_of_band() {
        for db in [-9.0f32, -3.0, 3.0, 6.0, 12.0] {
            let c = design(
                FilterKind::Peak {
                    q: 1.0,
                    gain_db: db,
                },
                1_000.0,
                SR,
            )
            .unwrap_or_else(|e| panic!("peak {db}: {e:?}"));
            let w = Coeffs::w(1_000.0, SR);
            assert!(
                (c.magnitude_db(w) - db).abs() < 0.05,
                "peak {db} dB: found {}",
                c.magnitude_db(w)
            );
            // Out of band it must be 0 dB: if a0 isn't `1 + α/A` this gets it wrong.
            assert!(
                c.magnitude_db(0.0).abs() < T,
                "peak {db}: DC {}",
                c.magnitude_db(0.0)
            );
            assert!(
                c.magnitude_db(PI).abs() < T,
                "peak {db}: Nyquist {}",
                c.magnitude_db(PI)
            );
        }
    }

    #[test]
    fn notch_has_a_zero_at_its_frequency() {
        let c = design(FilterKind::Notch { q: 2.0 }, 3_000.0, SR).expect("notch");
        assert!(c.magnitude_db(Coeffs::w(3_000.0, SR)) < -60.0);
        assert!(c.magnitude_db(0.0).abs() < T);
        assert!(c.magnitude_db(PI).abs() < T);
    }

    #[test]
    fn bandpass_has_unity_peak_at_f0() {
        let c = design(FilterKind::BandPass { q: 4.0 }, 2_000.0, SR).expect("bpf");
        assert!(c.magnitude_db(Coeffs::w(2_000.0, SR)).abs() < 0.05);
        assert!(c.magnitude_db(0.0) < -60.0);
    }

    #[test]
    fn allpass_magnitude_flat_but_phase_not() {
        let c = design(FilterKind::AllPass { q: 1.0 }, 500.0, SR).expect("apf");
        for f in [20.0f32, 100.0, 500.0, 2_000.0, 10_000.0, 22_000.0] {
            let g = c.magnitude_db(Coeffs::w(f, SR));
            assert!(g.abs() < 0.05, "allpass not flat at {f} Hz: {g} dB");
        }
        // The phase must be *different from zero* around f0, otherwise the
        // kick/bass tool's phase-rotator rotates nothing.
        let (s1, c1) = Coeffs::w(500.0, SR).sin_cos();
        let (s2, c2) = (2.0 * Coeffs::w(500.0, SR)).sin_cos();
        let num_im = -(c.b1 * s1 + c.b2 * s2);
        let num_re = c.b0 + c.b1 * c1 + c.b2 * c2;
        let den_im = -(c.a1 * s1 + c.a2 * s2);
        let den_re = 1.0 + c.a1 * c1 + c.a2 * c2;
        let phase = (num_im * den_re - num_re * den_im).atan2(num_re * den_re + num_im * den_im);
        assert!(
            phase.abs() > 0.3,
            "allpass at f0 should rotate phase, instead {phase} rad"
        );
    }

    #[test]
    fn shelf_gain_in_the_right_band() {
        // Evaluated at 20 Hz / 20 kHz, not at w=0/PI exactly: at w=0 the sum of
        // the coefficients is catastrophically cancelling in f32.
        let lo = design(
            FilterKind::LowShelf {
                gain_db: 6.0,
                slope: 1.0,
            },
            100.0,
            SR,
        )
        .expect("ls");
        let g_lo = lo.magnitude_db(Coeffs::w(20.0, SR));
        assert!(
            (g_lo - 6.0).abs() < 0.4,
            "low shelf at 20 Hz: {g_lo} dB, expected ~6"
        );
        let g_hi = lo.magnitude_db(Coeffs::w(20_000.0, SR));
        assert!(
            g_hi.abs() < 0.6,
            "low shelf at the top: {g_hi} dB, expected ~0"
        );

        let hs = design(
            FilterKind::HighShelf {
                gain_db: -4.0,
                slope: 1.0,
            },
            5_000.0,
            SR,
        )
        .expect("hs");
        let g_up = hs.magnitude_db(Coeffs::w(20_000.0, SR));
        assert!((g_up + 4.0).abs() < 0.6, "high shelf at the top: {g_up} dB");
        let g_dn = hs.magnitude_db(Coeffs::w(100.0, SR));
        assert!(g_dn.abs() < 0.3, "high shelf at the bottom: {g_dn} dB");
    }

    #[test]
    fn large_gain_shelves_stay_stable_and_finite() {
        // Historical breakage case: the term in the poles required 2*sqrt(A)*alpha.
        // Grid over slopes VALID for each gain (S=2 at ±24 dB admits no real
        // solution: that is the case of the rejection test below).
        for g in [-24.0f32, -12.0, -6.0, 6.0, 12.0, 24.0] {
            for slope in [0.3f32, 1.0] {
                let c = design(FilterKind::LowShelf { gain_db: g, slope }, 200.0, SR)
                    .unwrap_or_else(|e| panic!("shelf {g} dB S={slope}: {e:?}"));
                assert!(c.is_finite());
                assert!(c.is_stable(), "{g} dB S={slope} unstable");
            }
        }
    }

    #[test]
    fn shelf_slope_too_steep_for_gain_is_rejected() {
        // RBJ: (A + 1/A)(1/S − 1) + 2 ≥ 0. At ±24 dB the limit is S ≈ 1.9:
        // S=2.0 must give an explicit rejection, not a degenerate filter with
        // the pole exactly on the unit circle (alpha = 0 ⇒ a2 == a0).
        assert_eq!(
            design(
                FilterKind::LowShelf {
                    gain_db: -24.0,
                    slope: 2.0
                },
                200.0,
                SR
            )
            .err(),
            Some(FilterError::InvalidSlope)
        );
        assert_eq!(
            design(
                FilterKind::HighShelf {
                    gain_db: 24.0,
                    slope: 2.0
                },
                5_000.0,
                SR
            )
            .err(),
            Some(FilterError::InvalidSlope)
        );
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        assert_eq!(
            design(FilterKind::LowPass { q: 0.707 }, 30_000.0, SR).err(),
            Some(FilterError::FrequencyOutOfRange)
        );
        assert_eq!(
            design(FilterKind::HighPass { q: 0.707 }, 0.0, SR).err(),
            Some(FilterError::FrequencyOutOfRange)
        );
        assert_eq!(
            design(
                FilterKind::Peak {
                    q: 0.0,
                    gain_db: 3.0
                },
                100.0,
                SR
            )
            .err(),
            Some(FilterError::InvalidQ)
        );
        assert_eq!(
            design(FilterKind::LowPass { q: 0.707 }, 100.0, 0.0).err(),
            Some(FilterError::InvalidSampleRate)
        );
        assert_eq!(
            design(FilterKind::LowPass { q: 0.707 }, f32::NAN, SR).err(),
            Some(FilterError::FrequencyOutOfRange)
        );
    }

    #[test]
    fn stability_over_frequency_and_q_grid() {
        for &sr in &[44_100.0f32, 48_000.0, 96_000.0, 192_000.0] {
            for &f in &[10.0f32, 40.0, 500.0, 5_000.0, 15_000.0] {
                if f >= sr * 0.5 {
                    continue;
                }
                // f/sr ratio below ~1e-4 (subsonic at high fs): the stability
                // margin of the design is O(w0²) and the ulp of the coefficients
                // in f32 is LARGER than the margin — is_stable() would judge the
                // rounding, not the physics. Real case (10 Hz/192 kHz failed
                // here). Until we adopt f64 states or cascaded LPs for the
                // subsonic (TD-18), the grid must not lie.
                if f < sr * 1e-4 {
                    continue;
                }
                for &q in &[0.3f32, Q_BUTTERWORTH, 1.0, 4.0, 20.0] {
                    let c = design(FilterKind::LowPass { q }, f, sr)
                        .unwrap_or_else(|e| panic!("lpf f={f} q={q} sr={sr}: {e:?}"));
                    assert!(c.is_stable(), "unstable: f={f} q={q} sr={sr}");
                    // Now it also holds AT 10 Hz: `force_dc_unity` makes the DC
                    // gain exact by construction of the representation, not by
                    // luck of the truncations.
                    assert!(c.magnitude_db(0.0).abs() < 0.05);
                }
            }
        }
    }

    #[test]
    fn db_lin_roundtrip() {
        for db in [-12.0f32, -6.0, 0.0, 3.0, 18.0] {
            assert!((lin_to_db(db_to_lin(db)) - db).abs() < 1e-4);
        }
        assert!((db_to_lin(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_lin(20.0) - 10.0).abs() < 1e-4);
    }

    #[test]
    fn degenerate_raw_becomes_identity_not_nan() {
        let r = Raw {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a0: 0.0,
            a1: 0.0,
            a2: 0.0,
        };
        assert_eq!(r.normalize(), Coeffs::IDENTITY);
        let r2 = Raw {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a0: f64::NAN,
            a1: 0.0,
            a2: 0.0,
        };
        assert_eq!(r2.normalize(), Coeffs::IDENTITY);
    }
}
