//! Decibel conversions with the honesty rules of this module.
//!
//! ## Why `−∞` and not `−90`
//!
//! Invariant 2 of the spec: dBFS has reference ±1.0 = 0 dBFS and **silence is
//! `f32::NEG_INFINITY`**. Magic constants (−999, −120, the "mute floor" of old
//! consoles) poison every `max`: a chain that takes the quietest of many
//! readings returns the magic number as if it were a measurement, and one that
//! averages them is arithmetic on a lie. `NEG_INFINITY` is the only float for
//! which `max` keeps answering the real question, and `log10` produces it
//! honestly at zero: no special case, no branch that can drift out of sync with
//! the math.

/// Linear amplitude → dBFS (reference ±1.0 = 0 dBFS).
///
/// `20·log10(lin)`. `0.0` maps to `f32::NEG_INFINITY` (invariant 2: silence is
/// not a number pretending to be quiet). Negative inputs and `NaN` map to `NaN`:
/// an amplitude is never negative, and clamping that away would let a broken
/// signal read like a quiet one. Above 1.0 the result is positive — correct, not
/// an error: a digital signal *can* exceed full scale, and the meter must show
/// by how much.
pub fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.log10()
}

/// dBFS → linear amplitude, inverse of [`lin_to_db`].
///
/// `10^(db/20)`. `f32::NEG_INFINITY` maps to `0.0`, so "mute the band" expressed
/// as `−inf dB` is exactly silence after this conversion.
pub fn db_to_lin(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2·log10(2) to 19 digits: 6.0205999132796239… dB.
    const DB_OF_HALF: f64 = -6.020599913279624;
    /// 2·log10(2) positive: 6.020599913279624 dB.
    const DB_OF_TWO: f64 = 6.020599913279624;

    #[test]
    fn full_scale_is_exactly_zero_dbfs() {
        // log10(1.0) = 0 exactly in every IEEE-754 implementation.
        assert_eq!(lin_to_db(1.0), 0.0);
    }

    #[test]
    fn half_amplitude_is_minus_6_02_dbfs() {
        let got = lin_to_db(0.5) as f64;
        assert!(
            (got - DB_OF_HALF).abs() < 2e-4,
            "0.5 → {got:.6} dB, expected −6.020599913… (20·log10(0.5))"
        );
    }

    #[test]
    fn above_full_scale_reads_positive() {
        // 2.0 = +6.020599913… dBFS: a clip meter must be able to say "above 0".
        let got = lin_to_db(2.0) as f64;
        assert!(
            (got - DB_OF_TWO).abs() < 2e-4,
            "2.0 → {got:.6} dB, expected +6.020599913…"
        );
    }

    #[test]
    fn silence_is_negative_infinity_not_a_magic_number() {
        assert!(
            lin_to_db(0.0) == f32::NEG_INFINITY,
            "invariant 2: silence must be f32::NEG_INFINITY"
        );
    }

    #[test]
    fn negative_amplitude_is_nan_not_clamped() {
        // An amplitude < 0 is a caller bug or a broken signal: it must NOT read
        // as a finite level.
        assert!(lin_to_db(-0.5).is_nan());
        assert!(lin_to_db(f32::NAN).is_nan());
    }

    #[test]
    fn db_to_lin_maps_the_references_back() {
        // 10^(0/20) = 1; 10^(−6.020599913…/20) = 0.5; 10^(6.020599913…/20) = 2.
        assert!((db_to_lin(0.0) - 1.0).abs() < 1e-6, "0 dBFS → 1.0");
        let half = db_to_lin(DB_OF_HALF as f32);
        assert!(
            (half - 0.5).abs() < 5e-6,
            "−6.0206 dBFS → {half}, expected 0.5"
        );
        let two = db_to_lin(DB_OF_TWO as f32);
        assert!(
            (two - 2.0).abs() < 2e-5,
            "+6.0206 dBFS → {two}, expected 2.0"
        );
    }

    #[test]
    fn minus_infinity_db_is_exactly_silence() {
        assert_eq!(db_to_lin(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn round_trip_is_stable_to_audio_precision() {
        // FRAC_1_SQRT_2 is the sine-RMS amplitude: the round trip must land on
        // it from its own dB value, −3.0103 dBFS.
        for lin in [
            0.0001_f32,
            0.0316,
            0.5,
            core::f32::consts::FRAC_1_SQRT_2,
            1.0,
            1.778,
        ] {
            let back = db_to_lin(lin_to_db(lin));
            assert!(
                (back - lin).abs() / lin < 1e-5,
                "round trip lost precision at {lin}: got {back}"
            );
        }
    }
}
