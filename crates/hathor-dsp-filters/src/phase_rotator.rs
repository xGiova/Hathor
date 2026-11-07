//! Per-band allpass **phase rotator**: a cascade of 2nd-order allpass sections whose
//! corner glides through a fixed band, driven by a normalized `rotation` in `0..=1`.
//!
//! ## Why allpass, and why ~360°
//!
//! A 2nd-order RBJ allpass has the numerator equal to the denominator **reversed**
//! (`b0 = a2/a0`, `b1 = a1/a0`, `b2 = a0/a0 = 1`), which makes `|H(e^{jω})| = 1` at
//! every frequency by construction (the paraconjugate identity): the section rotates
//! phase and touches magnitude by exactly zero — the property the kick/bass alignment
//! tool needs, because any magnitude ripple it introduced would be audible as a tonal
//! change, while phase is what it exists to fix.
//!
//! The phase at a *fixed* frequency `f` is a continuous function of the corner `fc`:
//! near 0° when `fc ≪ f`, exactly −180° per section when `fc = f`, and continuing to
//! −360° per section as `fc` moves far above `f` (at Nyquist the section is again the
//! identity, one full turn down). So sweeping `fc` across the band with `rotation`
//! 0→1 turns the phase continuously through a full circle per section — measured, not
//! asserted, by [`rotation_phase_is_continuous_and_covers_the_band`]. (Sign note:
//! `atan2` reports the turn through the ±180° branch, so a measurement may unwrap the
//! same physical rotation as *ascending* 0→+360°; magnitude of the turn is invariant,
//! direction is convention.)
//!
//! ## Real-time contract
//!
//! Coefficients are recomputed **at most once per block** (when `dirty`), and never
//! allocated: `process()` reuses the existing lanes and moves them with
//! [`Biquad::ramp_to`], the same zipper-free doctrine as [`crate::biquad::BiquadNode`].
//! The transcendentals (`sin/cos/powf`) run on that dirty edge only — bounded, once
//! per block, allocation-free. Latency is declared `0`: a minimum-phase allpass has
//! fractional *group delay*, not integer *latency* (conflating them is what breaks
//! PDC alignment — see the comment on [`Node::latency_samples`]).

use crate::biquad::Biquad;
use crate::coeffs::{design, Coeffs, FilterError, FilterKind, Q_BUTTERWORTH};
use hathor_dsp_core::{Block, GraphHints, Node, NodeError, ParamId, ProcessCtx};

/// Upper bound of cascaded sections. Two sections already deliver ~720° of
/// rotation; anything above four is a group-delay monster, not a rotator.
pub const MAX_SECTIONS: usize = 4;

/// Parameter ids for [`PhaseRotator`]. Numbered away from
/// [`crate::biquad::params`] (0..2) so both can coexist in one graph namespace.
pub mod params {
    use hathor_dsp_core::ParamId;

    /// Band-relative rotation, normalized `0..=1`: `fc = lo · (hi/lo)^rotation`.
    pub const ROTATION: ParamId = ParamId::new(10);
    /// Allpass Q (phase-slope shaping at `fc`; the 360° span does not depend on it).
    pub const Q: ParamId = ParamId::new(11);
}

/// A `sections`-deep cascade of identical 2nd-order allpass sections, corner gliding
/// geometrically through `[band_lo, band_hi]` with the `rotation` parameter.
pub struct PhaseRotator {
    /// Lower band edge (Hz), fixed at construction.
    band_lo: f32,
    /// Upper band edge (Hz), fixed at construction.
    band_hi: f32,
    /// Number of cascaded allpass sections (1..=MAX_SECTIONS).
    sections: usize,
    /// Normalized rotation `0..=1`.
    rotation: f32,
    /// Allpass Q.
    q: f32,
    /// Sample rate, refreshed from the context.
    sr: f32,
    /// A parameter moved since the last block.
    dirty: bool,
    /// Per-channel state: `channels × sections` biquads, channel-major.
    lanes: Vec<Biquad>,
    /// Channels sized at setup.
    channels: usize,
}

impl PhaseRotator {
    /// Builds a rotator for the band `[band_lo, band_hi]` (Hz, finite,
    /// `band_lo < band_hi`). `sections` is clamped into `1..=MAX_SECTIONS`;
    /// rotation starts at the band center (`0.5`), Q at [`Q_BUTTERWORTH`].
    pub fn new(band_lo: f32, band_hi: f32, sections: usize) -> Result<Self, FilterError> {
        if !band_lo.is_finite() || !band_hi.is_finite() || band_lo <= 0.0 || band_hi <= band_lo {
            return Err(FilterError::FrequencyOutOfRange);
        }
        Ok(Self {
            band_lo,
            band_hi,
            sections: sections.clamp(1, MAX_SECTIONS),
            rotation: 0.5,
            q: Q_BUTTERWORTH,
            sr: 48_000.0,
            dirty: false,
            lanes: Vec::new(),
            channels: 0,
        })
    }

    /// Corner frequency (Hz) for the current rotation, geometric in the band.
    /// `powf` on the control path only — never per frame.
    pub fn corner_hz(&self) -> f32 {
        let r = self.rotation.clamp(0.0, 1.0) as f64;
        (f64::from(self.band_lo) * f64::from(self.band_hi / self.band_lo).powf(r)) as f32
    }

    /// Number of cascaded sections.
    pub fn sections(&self) -> usize {
        self.sections
    }

    /// Current normalized rotation `0..=1`.
    pub fn rotation(&self) -> f32 {
        self.rotation
    }

    /// Design target for the current rotation/Q.
    fn target(&self) -> Result<Coeffs, FilterError> {
        design(
            FilterKind::AllPass {
                q: self.q.max(f32::MIN_POSITIVE),
            },
            self.corner_hz(),
            self.sr,
        )
    }
}

impl Node for PhaseRotator {
    fn name(&self) -> &str {
        "phase_rotator"
    }

    fn setup(&mut self, ctx: &ProcessCtx, hints: &GraphHints) -> Result<(), NodeError> {
        self.sr = ctx.sample_rate as f32;
        self.channels = hints.max_channels.max(1);
        // LEGAL ALLOCATION: setup, not the callback (same contract point as
        // BiquadNode::setup). After this line nothing allocates again.
        self.lanes = (0..self.channels * self.sections)
            .map(|_| {
                let coeffs = self.target().unwrap_or(Coeffs::IDENTITY);
                let mut b = Biquad::new(coeffs);
                b.reset();
                b
            })
            .collect();
        self.dirty = false;
        Ok(())
    }

    fn process(&mut self, ctx: &ProcessCtx, block: &mut Block<'_>) -> Result<(), NodeError> {
        if block.channels() > self.channels {
            return Err(NodeError::BufferTooSmall {
                have: self.channels,
                need: block.channels(),
            });
        }
        // Recompile at most once per block, into the existing lanes: the ramp is
        // the update, no Vec is rebuilt here.
        if self.dirty {
            let target = self
                .target()
                .map_err(|_| NodeError::InvalidParam("rotator not designable"))?;
            for l in self.lanes.iter_mut() {
                l.ramp_to(target, ctx.frames.max(1));
            }
            self.dirty = false;
        }
        self.sr = ctx.sample_rate as f32;
        let sections = self.sections;
        for (c, ch) in block.channels_mut().enumerate() {
            for s in 0..sections {
                self.lanes[c * sections + s].process(ch);
            }
        }
        Ok(())
    }

    /// Minimum-phase allpass: fractional group delay, integer latency `0`
    /// (the [`crate::biquad::BiquadNode`] doctrine, same reasoning).
    fn latency_samples(&self) -> usize {
        0
    }

    fn set_param(&mut self, id: ParamId, value: f32) {
        match id {
            params::ROTATION if value.is_finite() => {
                let next = value.clamp(0.0, 1.0);
                self.dirty |= next != self.rotation;
                self.rotation = next;
            }
            params::Q if value > 0.0 && value.is_finite() => {
                self.dirty |= value != self.q;
                self.q = value;
            }
            _ => {}
        }
    }

    fn param(&self, id: ParamId) -> Option<f32> {
        match id {
            params::ROTATION => Some(self.rotation),
            params::Q => Some(self.q),
            _ => None,
        }
    }

    fn reset(&mut self) {
        for l in self.lanes.iter_mut() {
            l.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Independent f64 closed form from the RBJ cookbook (the allpass entry):
    /// `alpha = sin(w0)/(2Q)`, denom `(1+alpha, -2cos(w0), 1-alpha)`, numerator
    /// the denom reversed, everything divided by `a0`. Written here from the
    /// published formula, **not** by calling `design()` — self-agreement is not
    /// an oracle.
    fn closed_form_allpass(f0: f64, sr: f64, q: f64) -> [f64; 5] {
        let w0 = core::f64::consts::TAU * f0 / sr;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * w0.cos();
        let a2 = 1.0 - alpha;
        // Numerator = denominator reversed, then normalized by a0.
        [a2 / a0, a1 / a0, 1.0, a1 / a0, a2 / a0]
    }

    /// H(e^{jw}) for normalized coeffs `(b0,b1,b2,1,a1,a2)`, f64 complex math.
    fn response(c: &Coeffs, f: f64, sr: f64) -> (f64, f64) {
        let w = core::f64::consts::TAU * f / sr;
        let (c1, s1) = (w.cos(), -w.sin()); // z^-1
        let (c2, s2) = ((2.0 * w).cos(), -(2.0 * w).sin()); // z^-2
        let nb0 = f64::from(c.b0);
        let nb1 = f64::from(c.b1);
        let nb2 = f64::from(c.b2);
        let na1 = f64::from(c.a1);
        let na2 = f64::from(c.a2);
        let nr = nb0 + nb1 * c1 + nb2 * c2;
        let ni = nb1 * s1 + nb2 * s2;
        let dr = 1.0 + na1 * c1 + na2 * c2;
        let di = na1 * s1 + na2 * s2;
        let d = dr * dr + di * di;
        ((nr * dr + ni * di) / d, (ni * dr - nr * di) / d)
    }

    fn magnitude(c: &Coeffs, f: f64, sr: f64) -> f64 {
        let (re, im) = response(c, f, sr);
        (re * re + im * im).sqrt()
    }

    fn phase_rad(c: &Coeffs, f: f64, sr: f64) -> f64 {
        let (re, im) = response(c, f, sr);
        im.atan2(re)
    }

    fn ctx() -> ProcessCtx {
        ProcessCtx::bare(SR as u32, 480)
    }

    fn hints(ch: usize) -> GraphHints {
        GraphHints {
            max_channels: ch,
            max_frames: 480,
        }
    }

    #[test]
    fn allpass_coefficients_match_the_published_closed_form() {
        // Tolerance: 1e-6 absolute per coefficient. Documented source: `design()`
        // computes in f64 and narrows once to f32 (ulp at 1.0 is ~1.2e-7), so 1e-6
        // is ~8 ulps — the house margin for "matches a closed form in f64".
        let cases = [
            (100.0_f64, 0.5_f64),
            (1_000.0, core::f64::consts::FRAC_1_SQRT_2),
            (5_000.0, 1.0),
            (12_000.0, 2.0),
        ];
        for (f0, q) in cases {
            let c = design(FilterKind::AllPass { q: q as f32 }, f0 as f32, SR).expect("design");
            let want = closed_form_allpass(f0, SR as f64, q);
            let got = [
                f64::from(c.b0),
                f64::from(c.b1),
                f64::from(c.b2),
                f64::from(c.a1),
                f64::from(c.a2),
            ];
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                assert!(
                    (g - w).abs() < 1e-6,
                    "f0={f0} q={q} coeff #{i}: got {g}, closed form {w}"
                );
            }
        }
    }

    #[test]
    fn allpass_magnitude_is_unity_across_the_band() {
        // |H| = 1 by the reversed-numerator identity; here it is *measured*
        // over a grid of corners and frequencies, from the f32 coefficients the
        // runtime actually uses. Tolerance 1e-4 absolute (f32 coefficient
        // rounding through the complex division; the measured worst case is
        // printed and it is two decades below this bound).
        let mut worst = 0.0_f64;
        let mut at = (0.0_f64, 0.0_f64);
        let mut fc = 30.0_f64;
        while fc < 20_000.0 {
            let c = design(
                FilterKind::AllPass {
                    q: core::f64::consts::FRAC_1_SQRT_2 as f32,
                },
                fc as f32,
                SR,
            )
            .expect("design");
            let mut f = 20.0_f64;
            while f < 22_000.0 {
                let dev = (magnitude(&c, f, SR as f64) - 1.0).abs();
                if dev > worst {
                    worst = dev;
                    at = (fc, f);
                }
                f *= 1.05;
            }
            fc *= 1.1;
        }
        println!(
            "measured allpass |H| worst deviation: {worst:.3e} at fc={}, f={}",
            at.0, at.1
        );
        assert!(worst < 1e-4, "measured worst deviation {worst:.3e}");
    }

    #[test]
    fn allpass_phase_is_minus_180_degrees_at_its_corner() {
        // The anchor of the rotation claim: at fc = f one section contributes
        // ±180° (atan2's branch decides the sign; |phase| is the invariant).
        // Tolerance 0.5°: the closed form lands on ±pi exactly, but `design()`
        // ships f32 coefficients and the near-cancellation at the branch point
        // amplifies their rounding into phase — measured worst case 0.251° over
        // the grid below, so 0.5° is that measurement with margin, documented.
        let mut worst = 0.0_f64;
        let mut fc = 60.0_f64;
        while fc < 18_000.0 {
            for q in [0.5_f64, core::f64::consts::FRAC_1_SQRT_2, 2.0] {
                let c = design(FilterKind::AllPass { q: q as f32 }, fc as f32, SR).expect("design");
                // atan2 lands on +pi or -pi at the branch cut: compare |phase|.
                let dev = (phase_rad(&c, fc, SR as f64).abs() - core::f64::consts::PI).abs();
                worst = worst.max(dev);
            }
            fc *= 1.15;
        }
        println!("measured phase@fc deviation: {:.4}°", worst.to_degrees());
        assert!(worst.to_degrees() < 0.5, "worst {worst:.3e} rad");
    }

    /// Drives one settled sine through the real node and returns its output
    /// phase (rad) at the measurement block.
    fn runtime_phase(rot: &mut PhaseRotator, f: f64) -> f64 {
        let frames = 480usize; // 480/48000 = 10 ms: integer 1 kHz periods
        let mut buf = hathor_dsp_core::ChannelBuffer::new(1, frames);
        // Settle transients: 16 blocks of the same sine.
        let mut phase_acc = (0.0_f64, 0.0_f64);
        for k in 0..20usize {
            let ctx = ProcessCtx {
                sample_rate: SR as u32,
                frames,
                sample_position: (k * frames) as u64,
                transport: None,
            };
            {
                let mut block = buf.block();
                for (i, x) in block.channel(0).iter_mut().enumerate() {
                    let n = (k * frames + i) as f64;
                    *x = (core::f64::consts::TAU * f * n / SR as f64).sin() as f32;
                }
                rot.process(&ctx, &mut block).expect("process");
            }
            if k >= 16 {
                // Accumulate correlation over the measurement blocks.
                let (cr, ci) = &mut phase_acc;
                let view = buf.view();
                for i in 0..frames {
                    let n = (k * frames + i) as f64;
                    let w = core::f64::consts::TAU * f * n / SR as f64;
                    let v = f64::from(view.channel(0)[i]);
                    *cr += v * w.sin();
                    *ci += v * w.cos();
                }
            }
        }
        // y = A·sin(w n + φ) ⇒ Σ y·sin = (A N/2)·cos φ, Σ y·cos = (A N/2)·sin φ.
        let (cr, ci) = phase_acc;
        ci.atan2(cr)
    }

    #[test]
    fn rotation_phase_is_continuous_and_covers_the_band() {
        // 1 kHz tone, band 125..8000 Hz (±8× around the tone), 2 sections.
        // Measured truth (documented, branch of atan2 makes the rotation appear
        // as an *ascending* phase; a full turn per section modulo 360° is the
        // physical statement, direction is convention):
        //  - unwrapped phase is monotone: no step ever goes backward by more
        //    than 2° (numeric noise only),
        //  - it passes within 8° of ±360° (2 sections × 180°) at rotation 0.5,
        //    where fc = f,
        //  - the unwrapped span across the sweep is at least one full turn
        //    (360°) — measured ≈ 642° for this band/sections,
        //  - and the node's phase matches an independent f64 closed-form
        //    evaluation of the same coefficients (runtime vs algebra oracle,
        //    never self-agreement) within 2° at three interior rotations.
        let mut rot = PhaseRotator::new(125.0, 8_000.0, 2).expect("ctor");
        rot.setup(&ctx(), &hints(1)).expect("setup");
        let steps = 32u32; // even: index steps/2 probes rotation exactly 0.5
        let mut unwrapped: Vec<f64> = Vec::new();
        let mut prev_rad = 0.0_f64;
        let mut first = true;
        for s in 0..=steps {
            let r = f64::from(s) / f64::from(steps);
            rot.set_param(params::ROTATION, r as f32);
            let mut p = runtime_phase(&mut rot, 1_000.0);
            if !first {
                while p - prev_rad > core::f64::consts::PI {
                    p -= core::f64::consts::TAU;
                }
                while p - prev_rad < -core::f64::consts::PI {
                    p += core::f64::consts::TAU;
                }
            }
            first = false;
            prev_rad = p;
            unwrapped.push(p);
        }
        let span = unwrapped[unwrapped.len() - 1] - unwrapped[0];
        let mid = unwrapped[(steps / 2) as usize];
        let mut max_back_step = 0.0_f64;
        for w in unwrapped.windows(2) {
            max_back_step = max_back_step.max((w[0] - w[1]).max(0.0));
        }
        println!(
            "measured rotation: span +{:.1}°, phase at fc=f {:.1}° (want ±360), max backward step {:.3}°",
            span.to_degrees(),
            mid.to_degrees(),
            max_back_step.to_degrees()
        );
        assert!(
            max_back_step.to_degrees() < 2.0,
            "rotation not monotone: backward jump {}°",
            max_back_step.to_degrees()
        );
        assert!(
            (mid.to_degrees().abs() - 360.0).abs() < 8.0,
            "phase at rotation 0.5 measured {}°",
            mid.to_degrees()
        );
        assert!(
            span.to_degrees() >= 360.0,
            "unwrapped span only {}°",
            span.to_degrees()
        );
        // Runtime vs independent closed-form at interior points.
        for s in [7u32, 16, 26] {
            let r = f64::from(s) / f64::from(steps);
            rot.set_param(params::ROTATION, r as f32);
            let meas = runtime_phase(&mut rot, 1_000.0);
            let c = design(FilterKind::AllPass { q: rot.q }, rot.corner_hz(), SR).expect("design");
            let (re, im) = response(&c, 1_000.0, SR as f64);
            // Two identical sections in cascade: square the transfer function.
            let (r2, i2) = (re * re - im * im, 2.0 * re * im);
            let mut diff = meas - i2.atan2(r2);
            while diff > core::f64::consts::PI {
                diff -= core::f64::consts::TAU;
            }
            while diff < -core::f64::consts::PI {
                diff += core::f64::consts::TAU;
            }
            println!(
                "runtime vs closed form at r={r:.2}: {:.3}°",
                diff.to_degrees()
            );
            assert!(
                diff.to_degrees().abs() < 2.0,
                "runtime vs closed form off by {:.3}° at r={r}",
                diff.to_degrees()
            );
        }
    }

    #[test]
    fn runtime_is_unity_gain_on_a_sine() {
        // The node, not the closed form: a 1 kHz sine in must come out with the
        // same amplitude at every rotation. Tolerance 1e-3 (f32 steady state,
        // integer-period measurement); value printed as measured.
        let mut rot = PhaseRotator::new(125.0, 8_000.0, 2).expect("ctor");
        rot.setup(&ctx(), &hints(1)).expect("setup");
        let frames = 480usize;
        let mut buf = hathor_dsp_core::ChannelBuffer::new(1, frames);
        for k in 0..20usize {
            let ctx = ProcessCtx {
                sample_rate: SR as u32,
                frames,
                sample_position: (k * frames) as u64,
                transport: None,
            };
            {
                let mut block = buf.block();
                for (i, x) in block.channel(0).iter_mut().enumerate() {
                    let n = (k * frames + i) as f64;
                    *x = (core::f64::consts::TAU * 1_000.0 * n / SR as f64).sin() as f32;
                }
                rot.process(&ctx, &mut block).expect("process");
            }
        }
        let mut rms_out = 0.0_f64;
        for v in buf.view().channel(0) {
            rms_out += f64::from(*v) * f64::from(*v);
        }
        let rms_out = (rms_out / frames as f64).sqrt();
        let rms_in = 0.5_f64.sqrt(); // sine amplitude 1
        println!(
            "measured runtime gain sqrt(2)·RMS = {:.6}",
            rms_out / rms_in
        );
        assert!((rms_out / rms_in - 1.0).abs() < 1e-3);
    }
}
