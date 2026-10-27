//! [`RmsTracker`]: the running window the human eye needs.
//!
//! The block-rate [`crate::rms_db`] is per-block truth — it answers "what was in
//! those 64 samples", and a UI needle fed by it at 48 kHz/64-frame blocks would
//! be unreadable noise. The tracker adds the *running* mean-square with a
//! one-pole of chosen time constant (300 ms for a level meter: fast enough to
//! track a phrase, slow enough that the eye can read it). Two numbers, two
//! questions — the same doctrine as correlation vs optimal lag in [`crate::phase`].
//!
//! The one-pole is deliberately **on the mean-square**, not on the RMS: `x²`
//! is additive in energy (mean of squares over any partition equals the total
//! mean), while averaging *amplitudes* would silently bias the reading upward
//! and drift with signal statistics. `τ` semantics are shared with
//! [`hathor_dsp_core::param::SmoothedParam`]: τ = the 63 % time constant,
//! `coef = 1 − exp(−1/(τ·fs))` per frame.
//!
//! For the M6 calibration contract this is the honest-dBFS supplier: the UI maps
//! `rms_db()` onto the sweet-spot band from the channel-strip contract
//! (`reference_dbfs ± window`, −20…−14 dBFS by default). The band classification
//! itself is consumer-side and deliberately *not* here (Deferred in the spec):
//! a measurement library must not pre-guess the strip's policy.

use hathor_dsp_core::buffer::Block;

/// One-pole mean-square RMS tracker (`RmsTracker`) with a τ in seconds.
///
/// Zero-allocation at block rate: the state is two `f32`s.
#[derive(Debug, Clone, Copy)]
pub struct RmsTracker {
    mean_square: f32,
    coef: f32,
    tau_s: f32,
}

impl RmsTracker {
    /// Starts silent (so `rms_db()` reads `f32::NEG_INFINITY` before the first
    /// block — never an invented level) with the τ recorded, but **with the
    /// default conservative coefficient** (1e-3 per frame ≈ 1.3 s at 48 kHz),
    /// exactly like [`hathor_dsp_core::param::SmoothedParam::new`]: the
    /// coefficient is `1 − exp(−1/(τ·fs))` and cannot exist before the sample
    /// rate is known. Call [`RmsTracker::set_time`] in `setup()`, where the
    /// sample rate is known — the spec's `new(tau_s)` sketch omits it, and a
    /// meter that *silently* ran on a guessed coefficient is the bug class the
    /// SmoothedParam docs already call out.
    pub fn new(tau_s: f32) -> Self {
        Self {
            mean_square: 0.0,
            coef: 1e-3,
            tau_s,
        }
    }

    /// Recomputes the coefficient from the sample rate and τ. Call in `setup()`
    /// or when the sample rate changes, **not** per block (it is an `exp`).
    /// Same semantics and same `τ ≥ 1 µs` clamp as
    /// [`hathor_dsp_core::param::SmoothedParam::set_time`]: 63 % of the target
    /// mean-square in one τ, 99 % at ~6.9 τ.
    pub fn set_time(&mut self, sample_rate: u32, tau_s: f32) {
        self.tau_s = tau_s;
        let tau = tau_s.max(1e-6);
        self.coef = 1.0 - (-1.0 / (tau * sample_rate as f32)).exp();
    }

    /// The τ in seconds last given to [`RmsTracker::new`] /
    /// [`RmsTracker::set_time`].
    pub fn tau_s(&self) -> f32 {
        self.tau_s
    }

    /// Advances the tracker over a block. Audio rate: zero allocation, zero
    /// lock; two fused multiply-adds per frame for mono, one more per extra
    /// channel (expected).
    ///
    /// The one-pole steps **once per frame on the channel-mean energy**
    /// `Σ_c x_c²/channels`. Not "channel after channel on the same state":
    /// with a τ ≫ block, updating the state sequentially per channel lets the
    /// **last channel win** (a silent ch1 would drag a full-scale ch0's meter
    /// to −∞ — `stereo_energy_averages_channels_not_the_last_one_wins` is the
    /// test that caught this). Frame-mean energy is the quantity whose
    /// long-run average equals the all-samples mean of [`crate::rms_db`]: one
    /// doctrine, two windows.
    ///
    /// A `NaN` sample poisons the state permanently, exactly like a `NaN` in a
    /// console meter's input should: the broken signal must stay visible, not
    /// decay into a reassuring silence.
    pub fn process_block(&mut self, block: &Block<'_>) {
        let frames = block.frames();
        let channels = block.channels();
        if frames == 0 || channels == 0 {
            return; // nothing to measure: the state is an honest old value
        }
        let inv_c = 1.0 / channels as f32;
        let c = self.coef;
        let mut m = self.mean_square;
        for f in 0..frames {
            let mut e = 0.0f32;
            for ch in 0..channels {
                let x = block.channel_ref(ch)[f];
                e = x.mul_add(x, e);
            }
            m = c.mul_add(e * inv_c - m, m);
        }
        self.mean_square = m;
    }

    /// The tracked RMS in dBFS, read at UI rate.
    ///
    /// `10·log10(mean_square)`: the same value `20·log10(sqrt(ms))` would give,
    /// one `log10` cheaper. Silence (state 0, the initial state after only
    /// silent blocks) reads `f32::NEG_INFINITY` (invariant 2).
    ///
    /// Asymptotic note: the one-pole converges to `mean(x²)` within ULP/coef —
    /// at τ=300 ms, 48 kHz that plateau is ≈0.007 dB, a third of an
    /// eye-sampling interval: the meter stops "lying" far before it stops
    /// "creeping".
    pub fn rms_db(&self) -> f32 {
        10.0 * self.mean_square.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rms_db as block_rms_db;
    use hathor_dsp_core::buffer::{Block, ChannelBuffer, Shape};

    const SR: u32 = 48_000;

    fn dc_block(data: &mut [f32], v: f32) -> Block<'_> {
        data.fill(v);
        let n = data.len();
        Block::from_slice(data, Shape::new(1, n))
    }

    #[test]
    fn fresh_tracker_reads_silence_not_an_invented_level() {
        // Invariant 2 at the tracker boundary: before any block the reading is
        // −∞, never 0 dBFS ("default max") and never a −90 magic floor.
        let t = RmsTracker::new(0.3);
        assert!(t.rms_db() == f32::NEG_INFINITY);
        assert_eq!(t.tau_s(), 0.3);
    }

    #[test]
    fn default_coef_is_conservative_until_setup() {
        // Without set_time the coefficient is SmoothedParam's default 1e-3:
        // one 64-frame block of full scale moves the state to
        // 1 − (1 − 1e-3)^64 = 0.0620 → 10·log10 = −12.07 dB, *not* 0 dB.
        // This is the documented "slow default", not a bug — and the reason
        // setup() must call set_time.
        let mut t = RmsTracker::new(0.3);
        let mut d = [0.0f32; 64];
        t.process_block(&dc_block(&mut d, 1.0));
        let got = t.rms_db() as f64;
        assert!(
            got < -10.0 && got > -14.0,
            "unset-time tracker jumped to {got:.2} dB"
        );
    }

    #[test]
    fn converges_to_block_rms_on_dc() {
        // Steady DC of amplitude 0.5: mean(x²) → 0.25 → rms → 0.5 →
        // −6.020599913279624 dBFS, and the block-rate rms_db of the same
        // samples is the exact reference the tracker must approach.
        let mut t = RmsTracker::new(0.3);
        t.set_time(SR, 0.3);
        let mut d = [0.0f32; 64];
        let want = block_rms_db(&dc_block(&mut d, 0.5)) as f64; // −6.0206…
        for _ in 0..4_000 {
            // 4000·64/48000 ≈ 5.3 s ≈ 17 τ → converged within e^−17.
            t.process_block(&dc_block(&mut d, 0.5));
        }
        let got = t.rms_db() as f64;
        assert!(
            (got - want).abs() < 0.01,
            "tracker settled at {got:.4} dB, block rms is {want:.4} dB"
        );
    }

    #[test]
    fn one_tau_of_silence_drops_4_34_db() {
        // The τ semantics test (shared with SmoothedParam: 63 % in one τ).
        // Settled state S decaying toward 0: after exactly τ of silent blocks
        // (0.3 s = 14 400 frames = 225 blocks of 64), the discrete one-pole
        // gives S·(1−coef)^frames = S·e^(−1) → the dB reading drops by
        // 10·log10(e) = 4.342944819032518 dB.
        let mut t = RmsTracker::new(0.3);
        t.set_time(SR, 0.3);
        let mut d = [0.0f32; 64];
        for _ in 0..4_000 {
            t.process_block(&dc_block(&mut d, 1.0));
        }
        let base = t.rms_db();
        assert!(
            (base as f64).abs() < 0.01,
            "DC 1.0 must settle at 0 dBFS, got {base:.4}"
        );
        let mut zd = [0.0f32; 64];
        for _ in 0..225 {
            t.process_block(&dc_block(&mut zd, 0.0));
        }
        let got = t.rms_db() as f64;
        let want = base as f64 - 4.342944819032518;
        assert!(
            (got - want).abs() < 0.03,
            "1τ decay read {got:.4} dB, expected {want:.4} dB (−10·log10(e))"
        );
    }

    #[test]
    fn one_tau_from_silence_reaches_63_percent_energy() {
        // Rising side of the same contract: target 1.0, state 0, after one τ
        // (225 blocks × 64 frames): state = 1 − e^(−1) = 0.6321205588…
        // → 10·log10 = −1.9923 dB. The "63 % in one τ" of the spec, measured.
        let mut t = RmsTracker::new(0.3);
        t.set_time(SR, 0.3);
        let mut d = [0.0f32; 64];
        for _ in 0..225 {
            t.process_block(&dc_block(&mut d, 1.0));
        }
        let got = t.rms_db() as f64;
        let want = 10.0 * (1.0 - (-1.0_f64).exp()).log10(); // −1.99234…
        assert!(
            (got - want).abs() < 0.03,
            "1τ rise read {got:.4} dB, expected {want:.4} dB (10·log10(1−e⁻¹))"
        );
    }

    #[test]
    fn tiny_tau_tracks_immediately() {
        // τ clamped to 1 µs → coef ≈ 1: the state follows the last sample's
        // energy; the clamp of set_time is the SmoothedParam one, shared.
        let mut t = RmsTracker::new(0.3);
        t.set_time(SR, 0.0); // → max(1e-6)
        let mut d = [0.0f32; 64];
        t.process_block(&dc_block(&mut d, 1.0));
        assert!(
            (t.rms_db() as f64).abs() < 0.01,
            "instant tracker should read 0 dBFS, got {}",
            t.rms_db()
        );
    }

    #[test]
    fn stereo_energy_averages_channels_not_the_last_one_wins() {
        // ch0 full-scale DC, ch1 silent, τ near-instant: the target energy is
        // (1+0)/2 = 0.5 → −3.010299956639812 dBFS — the same number
        // `rms_db(&block)` gives for this block (one doctrine). History: the
        // first implementation stepped the state per-sample in channel order
        // and read −57.9 dB here — the silent ch1 "won" because it was last.
        // Frame-mean energy is the fix this test nailed in place.
        let mut t = RmsTracker::new(0.3);
        t.set_time(SR, 1e-4); // coef ≈ 0.19 → converges well inside the loop
        let mut buf = ChannelBuffer::new(2, 64);
        {
            let mut b = buf.block();
            b.channel(0).fill(1.0);
        }
        for _ in 0..4_000 {
            t.process_block(&buf.block());
        }
        let got = t.rms_db() as f64;
        assert!(
            (got + 3.010299956639812).abs() < 0.02,
            "stereo energy read {got:.4} dB, expected −3.0103"
        );
    }

    #[test]
    fn silence_keeps_silence() {
        // Exact-zero input on an exact-zero state must stay at −∞ forever:
        // coef·(0 − 0) = 0 is stable in IEEE-754 — the muted-track case of
        // the calibration meter must not "warm up".
        let mut t = RmsTracker::new(0.3);
        t.set_time(SR, 0.3);
        let mut d = [0.0f32; 64];
        for _ in 0..100 {
            t.process_block(&dc_block(&mut d, 0.0));
        }
        assert!(t.rms_db() == f32::NEG_INFINITY);
    }

    #[test]
    fn nan_sample_poisons_the_meter_and_sticks() {
        let mut t = RmsTracker::new(0.3);
        t.set_time(SR, 0.3);
        let mut d = [0.5f32; 64];
        d[10] = f32::NAN;
        t.process_block(&Block::from_slice(&mut d, Shape::new(1, 64)));
        assert!(t.rms_db().is_nan(), "NaN must stay visible");
        let mut clean = [0.5f32; 64];
        t.process_block(&Block::from_slice(&mut clean, Shape::new(1, 64)));
        assert!(
            t.rms_db().is_nan(),
            "clean blocks must not heal a poisoned meter"
        );
    }

    #[test]
    fn set_time_recomputes_the_coefficient_only() {
        // Changing the rate mid-stream is legal and must not invent or erase
        // level: the state survives, only the smoothing constant moves.
        let mut t = RmsTracker::new(0.3);
        t.set_time(SR, 0.3);
        let mut d = [0.0f32; 64];
        for _ in 0..500 {
            t.process_block(&dc_block(&mut d, 0.5));
        }
        let before = t.rms_db();
        t.set_time(96_000, 0.3);
        assert_eq!(t.rms_db(), before, "set_time must not touch the state");
        assert_eq!(t.tau_s(), 0.3);
    }
}
