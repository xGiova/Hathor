//! Block level measurements: [`peak_db`] and [`rms_db`].
//!
//! Two answers to "how loud is this block", both true at the same time:
//!
//! * **peak** is the clipping truth — the worst single sample, what the headroom
//!   of the chain sees;
//! * **rms** is the energy truth — what the ear integrates.
//!
//! They must agree at 0 dBFS on a full-scale **DC** signal and disagree by
//! exactly 3.0103 dB on a full-scale **sine** (RMS of a sine is 1/√2 = −3.0103
//! dBFS). The tests pin both references, because if one of the two functions
//! silently changes meaning, those two numbers are the first things to break.

use crate::db::lin_to_db;
use hathor_dsp_core::buffer::Block;

/// Peak level of the block in dBFS: `20·log10(max|x|)` over every channel.
///
/// Zero-allocation, O(samples). Silence (or an empty block) reads
/// `f32::NEG_INFINITY` (invariant 2). A single `NaN` sample makes the whole
/// reading `NaN`: a corrupt signal is not silence, and a peak meter that hides
/// it turns a bug into an empty meter.
pub fn peak_db(block: &Block<'_>) -> f32 {
    let mut peak = 0.0f32;
    for c in 0..block.channels() {
        for &s in block.channel_ref(c) {
            let x = s.abs();
            if x > peak {
                peak = x;
            } else if x.is_nan() {
                // Sticky: once NaN, the comparisons below never dislodge it.
                peak = f32::NAN;
            }
        }
    }
    lin_to_db(peak)
}

/// RMS level of the block in dBFS: `20·log10(sqrt(mean(x²)))` over every channel.
///
/// Zero-allocation, O(samples). The mean is over **all samples of the block**
/// (channels × frames): the energy of a stereo block is the energy of both
/// channels, not the loudest one. Silence and empty blocks read
/// `f32::NEG_INFINITY` (invariant 2); `NaN` propagates naturally through the
/// sum of squares.
pub fn rms_db(block: &Block<'_>) -> f32 {
    let total = block.channels() * block.frames();
    if total == 0 {
        return f32::NEG_INFINITY;
    }
    let mut sum_sq = 0.0f32;
    for c in 0..block.channels() {
        sum_sq += block.channel_ref(c).iter().map(|s| s * s).sum::<f32>();
    }
    lin_to_db((sum_sq / total as f32).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hathor_dsp_core::buffer::{ChannelBuffer, Shape};

    /// 10·log10(2) = 3.0102999566398120… dB: the sine-RMS reference.
    const SINE_DB: f64 = -3.010299956639812;
    /// 20·log10(0.5) = −6.020599913279624 dB.
    const HALF_DB: f64 = -6.020599913279624;

    fn mono(data: &mut [f32]) -> Block<'_> {
        let n = data.len();
        Block::from_slice(data, Shape::new(1, n))
    }

    #[test]
    fn peak_of_full_scale_is_zero_db() {
        let mut d = [1.0f32, -1.0, 0.5];
        // max|x| = 1.0 → 0 dBFS exactly (log10(1) = 0).
        assert_eq!(peak_db(&mono(&mut d)), 0.0);
    }

    #[test]
    fn peak_of_half_is_minus_6_02_dbfs() {
        let mut d = [0.5f32, -0.25, 0.1];
        let got = peak_db(&mono(&mut d)) as f64;
        assert!(
            (got - HALF_DB).abs() < 2e-4,
            "peak 0.5 → {got:.6} dB, expected −6.020599913…"
        );
    }

    #[test]
    fn peak_reads_positive_above_full_scale() {
        let mut d = [2.0f32, -0.1];
        // max|x| = 2.0 → +6.020599913… dBFS: the meter must show the overshoot.
        let got = peak_db(&mono(&mut d)) as f64;
        assert!(
            (got - 6.020599913279624).abs() < 2e-4,
            "peak 2.0 → {got:.6} dB, expected +6.020599913…"
        );
    }

    #[test]
    fn peak_silence_is_negative_infinity() {
        let mut d = [0.0f32, 0.0, -0.0];
        assert!(peak_db(&mono(&mut d)) == f32::NEG_INFINITY);
    }

    #[test]
    fn peak_nan_sample_propagates_and_sticks() {
        let mut d = [0.5f32, f32::NAN, 0.25];
        assert!(
            peak_db(&mono(&mut d)).is_nan(),
            "a corrupt sample must not read as a level"
        );
    }

    #[test]
    fn rms_of_the_3_4_triangle_is_10_97_dbfs() {
        let mut d = [3.0f32, 4.0];
        // mean(x²) = (9+16)/2 = 12.5; rms = √12.5 = 3.5355339…
        // 20·log10(√12.5) = 10·log10(12.5) = 10.969100130080562 dBFS.
        let got = rms_db(&mono(&mut d)) as f64;
        assert!(
            (got - 10.969100130080562).abs() < 2e-4,
            "rms [3,4] → {got:.6} dB, expected 10.969100130…"
        );
    }

    #[test]
    fn rms_of_half_amplitude_is_minus_6_02_dbfs() {
        let mut d = [0.5f32; 8];
        let got = rms_db(&mono(&mut d)) as f64;
        assert!(
            (got - HALF_DB).abs() < 2e-4,
            "rms 0.5 DC → {got:.6} dB, expected −6.020599913…"
        );
    }

    #[test]
    fn rms_full_scale_dc_is_zero_and_matches_peak() {
        let mut d = [1.0f32, -1.0, 1.0, -1.0];
        // |x| = 1 everywhere → mean(x²) = 1 → 0 dBFS; peak sees the same 1.0.
        assert_eq!(rms_db(&mono(&mut d)), 0.0);
        assert_eq!(peak_db(&mono(&mut d)), 0.0);
    }

    #[test]
    fn full_scale_sine_rms_is_minus_3_01_dbfs() {
        // Σ over an integer number of periods: Σsin²(2πk·i/n) = n/2 exactly for
        // k ≥ 1 integer periods → rms = 1/√2 → −3.010299956639812 dBFS.
        let n = 4096;
        let periods = 100;
        let mut buf = ChannelBuffer::new(2, n / 2);
        {
            let mut b = buf.block();
            for ch in b.channels_mut() {
                for (i, s) in ch.iter_mut().enumerate() {
                    *s = (core::f32::consts::TAU * periods as f32 * i as f32 / n as f32).sin();
                }
            }
        }
        let got = rms_db(&buf.block()) as f64;
        assert!(
            (got - SINE_DB).abs() < 1e-3,
            "full-scale sine → {got:.6} dB, expected −3.0103 (20·log10(1/√2))"
        );
    }

    #[test]
    fn rms_averages_across_channels_not_per_channel() {
        // ch0 full scale, ch1 silent: mean(x²) = (1+0)/2 = 0.5 → rms = 1/√2
        // → −3.010299956639812 dBFS. (If this read 0 dB, energy would be
        // silently invented out of a dead channel.)
        let mut buf = ChannelBuffer::new(2, 4);
        buf.block()
            .channel(0)
            .copy_from_slice(&[1.0, 1.0, 1.0, 1.0]);
        let got = rms_db(&buf.block()) as f64;
        assert!(
            (got - SINE_DB).abs() < 1e-5,
            "one live channel of two → {got:.6} dB, expected −3.0103"
        );
    }

    #[test]
    fn empty_blocks_read_silence_not_nan() {
        let mut d = [0.0f32; 0];
        let blk = Block::from_slice(&mut d, Shape::new(0, 0));
        assert!(rms_db(&blk) == f32::NEG_INFINITY);
        assert!(peak_db(&blk) == f32::NEG_INFINITY);
        let mut buf = ChannelBuffer::new(2, 0);
        assert!(rms_db(&buf.block()) == f32::NEG_INFINITY);
    }

    #[test]
    fn rms_nan_sample_propagates() {
        let mut d = [0.5f32, f32::NAN, 0.5, 0.5];
        assert!(rms_db(&mono(&mut d)).is_nan());
    }
}
