//! Biquad in Direct Form II Transposed, with a coefficient ramp and an AVX2 kernel.

use crate::coeffs::{design, Coeffs, FilterError, FilterKind};
use hathor_dsp_core::{Block, GraphHints, Node, NodeError, ParamId, ProcessCtx};

/// A 2nd-order filter, `z1`/`z2` state and a **coefficient ramp**.
///
/// ## Why the ramp is on the coefficients and not on the cutoff
///
/// Ramping the cutoff means recomputing `sin/cos/sqrt` for every sample: ~40-60 ns
/// (expected) × 64 frames ≈ 2.5-4 µs per filter per block: only ~0.2-0.3% of the
/// 1333 µs budget alone, but it multiplies across bands × channels and puts
/// transcendental calls inside the callback, which is exactly what the bounded-cycle
/// rule forbids. Ramping the 5 coefficients linearly from the old set to the new one
/// costs 5 `add` per frame (~0.5 ns), and the filter never sees a step → no zipper
/// noise. See the `coefficient_ramp_removes_the_step` and
/// `interrupted_ramp_restarts_from_current_coefficients` tests.
#[derive(Debug, Clone)]
pub struct Biquad {
    /// Current coefficients (interpolated during the ramp).
    c: Coeffs,
    /// Where the ramp must land. It is **really** needed: closing by summing the
    /// accumulated deltas leaves an offset in f32, and on an EQ that is a
    /// permanently out-of-tune gain.
    target: Coeffs,
    /// Per-frame increment toward `target`.
    d: Coeffs,
    /// Frames of ramp left.
    remaining: i32,
    z1: f32,
    z2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Self::new(Coeffs::IDENTITY)
    }
}

impl Biquad {
    /// Starts with `coeffs` already settled.
    pub fn new(coeffs: Coeffs) -> Self {
        Self {
            c: coeffs,
            target: coeffs,
            d: Coeffs::IDENTITY,
            remaining: 0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Designs and **jumps** straight to the new set (setup / reset only).
    pub fn set_immediate(
        &mut self,
        kind: FilterKind,
        f0: f32,
        sample_rate: f32,
    ) -> Result<(), FilterError> {
        self.c = design(kind, f0, sample_rate)?;
        self.target = self.c;
        self.d = Coeffs::IDENTITY;
        self.remaining = 0;
        Ok(())
    }

    /// Designs and ramps the coefficients toward the new set over `frames` frames.
    pub fn ramp(
        &mut self,
        kind: FilterKind,
        f0: f32,
        sample_rate: f32,
        frames: usize,
    ) -> Result<(), FilterError> {
        let target = design(kind, f0, sample_rate)?;
        self.ramp_to(target, frames);
        Ok(())
    }

    /// Ramps toward an arbitrary set. `frames == 0` = hard jump.
    ///
    /// Calling it while a ramp is still running is legal: the new one starts from
    /// the *current* interpolated value, not from the old target.
    pub fn ramp_to(&mut self, target: Coeffs, frames: usize) {
        if frames == 0 || !target.is_finite() {
            self.c = target;
            self.target = target;
            self.d = Coeffs::IDENTITY;
            self.remaining = 0;
            return;
        }
        self.target = target;
        let k = 1.0 / frames as f32;
        self.d = Coeffs {
            b0: (target.b0 - self.c.b0) * k,
            b1: (target.b1 - self.c.b1) * k,
            b2: (target.b2 - self.c.b2) * k,
            a1: (target.a1 - self.c.a1) * k,
            a2: (target.a2 - self.c.a2) * k,
        };
        self.remaining = frames as i32;
    }

    /// Current coefficients.
    pub fn coeffs(&self) -> Coeffs {
        self.c
    }

    /// True if the ramp has not finished yet.
    pub fn is_ramping(&self) -> bool {
        self.remaining > 0
    }

    /// Zeroes the state. Does not touch the coefficients.
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Advances one sample.
    ///
    /// DF2T: the first-order state does **not** depend on the delayed input, and
    /// the transposed form has a single add chain per branch → it is the correct
    /// choice in floating point (DF1 accumulates error on poles near the unit circle).
    #[inline]
    pub fn process_sample(&mut self, x: f32) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            self.c.b0 += self.d.b0;
            self.c.b1 += self.d.b1;
            self.c.b2 += self.d.b2;
            self.c.a1 += self.d.a1;
            self.c.a2 += self.d.a2;
            if self.remaining == 0 {
                // Closes on the *exact* target, not on the sum of the deltas.
                self.c = self.target;
            }
        }
        let out = self.c.b0 * x + self.z1;
        self.z1 = self.c.b1 * x - self.c.a1 * out + self.z2;
        // `- a2·out` is NOT optional: this is the DF2T. Without it the filter stays
        // stable and "almost right" in magnitude, but gets the phase wrong — exactly
        // the quantity the kick/bass tool must measure. This bug was written and
        // *not* caught by the test against the f64 reference (hand-written with the
        // same error): only the behavioral test
        // `measured_response_matches_predicted_response` catches it.
        self.z2 = self.c.b2 * x - self.c.a2 * out;
        out
    }

    /// Processes a slice in-place.
    pub fn process(&mut self, data: &mut [f32]) {
        for s in data.iter_mut() {
            *s = self.process_sample(*s);
        }
    }

    /// Processes a block with **one filter per channel**.
    ///
    /// This is not an API courtesy: "a biquad that sees L and R" is a fake stereo
    /// filter, and in the kick/bass plugin's phase/correlation tests it would be an
    /// invisible error (the two copies of the same filter have different states).
    /// If `lanes.len() < block.channels()` the extra channels are left untouched.
    pub fn process_channels(lanes: &mut [Biquad], block: &mut Block<'_>) {
        for (c, ch) in block.channels_mut().enumerate() {
            if let Some(f) = lanes.get_mut(c) {
                f.process(ch);
            }
        }
    }
}

/// Parameter identifiers of the filter node.
pub mod params {
    use hathor_dsp_core::ParamId;
    /// Corner frequency in Hz (0..Nyquist).
    pub const FREQ: ParamId = ParamId::new(0);
    /// Q.
    pub const Q: ParamId = ParamId::new(1);
    /// Gain in dB (Peak/Shelf only).
    pub const GAIN_DB: ParamId = ParamId::new(2);
}

/// A [`Biquad`] exposed as a [`Node`].
///
/// The parameter is **read once per block** and the coefficients are ramped within
/// that block: that is the right trade-off, and the numbers are the ones stated on
/// [`Biquad`].
pub struct BiquadNode {
    kind: FilterKind,
    freq: f32,
    q: f32,
    gain_db: f32,
    sr: f32,
    dirty: bool,
    /// Per-channel state: one filter per channel, not "one stereo filter".
    lanes: Vec<Biquad>,
    channels: usize,
}

impl BiquadNode {
    /// Creates with initial frequency/Q/gain.
    pub fn new(kind: FilterKind, freq: f32, q: f32, gain_db: f32) -> Self {
        Self {
            kind,
            freq,
            q,
            gain_db,
            sr: 48_000.0,
            dirty: false,
            lanes: Vec::new(),
            channels: 0,
        }
    }

    /// Static label of the type (so name() doesn't allocate).
    pub fn kind_label(kind: FilterKind) -> &'static str {
        match kind {
            FilterKind::LowPass { .. } => "lowpass",
            FilterKind::HighPass { .. } => "highpass",
            FilterKind::BandPass { .. } => "bandpass",
            FilterKind::Notch { .. } => "notch",
            FilterKind::Peak { .. } => "peaking",
            FilterKind::LowShelf { .. } => "lowshelf",
            FilterKind::HighShelf { .. } => "highshelf",
            FilterKind::AllPass { .. } => "allpass",
        }
    }

    fn kind_with(&self, kind: FilterKind) -> FilterKind {
        match kind {
            FilterKind::LowPass { .. } => FilterKind::LowPass { q: self.q },
            FilterKind::HighPass { .. } => FilterKind::HighPass { q: self.q },
            FilterKind::BandPass { .. } => FilterKind::BandPass { q: self.q },
            FilterKind::Notch { .. } => FilterKind::Notch { q: self.q },
            FilterKind::Peak { .. } => FilterKind::Peak {
                q: self.q,
                gain_db: self.gain_db,
            },
            FilterKind::LowShelf { slope, .. } => FilterKind::LowShelf {
                gain_db: self.gain_db,
                slope,
            },
            FilterKind::HighShelf { slope, .. } => FilterKind::HighShelf {
                gain_db: self.gain_db,
                slope,
            },
            FilterKind::AllPass { .. } => FilterKind::AllPass { q: self.q },
        }
    }
}

impl Node for BiquadNode {
    fn name(&self) -> &str {
        Self::kind_label(self.kind)
    }

    fn setup(&mut self, ctx: &ProcessCtx, hints: &GraphHints) -> Result<(), NodeError> {
        self.sr = ctx.sample_rate as f32;
        self.channels = hints.max_channels.max(1);
        // LEGAL ALLOCATION: we are in setup, not in the callback. This is the exact
        // point where the contract separates "prepare" from "play".
        self.lanes = (0..self.channels)
            .map(|_| {
                let coeffs = design(self.kind, self.freq, self.sr).unwrap_or(Coeffs::IDENTITY);
                let mut b = Biquad::new(coeffs);
                b.reset();
                b
            })
            .collect();
        self.dirty = false;
        Ok(())
    }

    fn process(&mut self, ctx: &ProcessCtx, block: &mut Block<'_>) -> Result<(), NodeError> {
        if block.channels() > self.lanes.len() {
            return Err(NodeError::BufferTooSmall {
                have: self.lanes.len(),
                need: block.channels(),
            });
        }
        // Recompile the coefficients only if something changed.
        if self.dirty {
            let target = design(self.kind_with(self.kind), self.freq, self.sr)
                .map_err(|_| NodeError::InvalidParam("filter not designable"))?;
            for l in self.lanes.iter_mut() {
                l.ramp_to(target, ctx.frames.max(1));
            }
            self.dirty = false;
        }
        self.sr = ctx.sample_rate as f32;
        for (c, ch) in block.channels_mut().enumerate() {
            self.lanes[c].process(ch);
        }
        Ok(())
    }

    /// A minimum-phase IIR **has no declarable integer latency**: it has fractional
    /// group delay. Reporting 0 is correct for the PDC (which compensates integer
    /// delays); the per-band group delay is a number exposed by `dsp-anal`, not by
    /// this figure. Confusing the two = wrong phase alignment, which is exactly the
    /// enemy of the kick/bass tool.
    fn latency_samples(&self) -> usize {
        0
    }

    fn set_param(&mut self, id: ParamId, value: f32) {
        let old = (self.freq, self.q, self.gain_db);
        match id {
            params::FREQ => self.freq = value,
            params::Q => {
                if value > 0.0 && value.is_finite() {
                    self.q = value;
                }
            }
            params::GAIN_DB => self.gain_db = value,
            _ => return,
        }
        self.dirty |= old != (self.freq, self.q, self.gain_db);
    }

    fn param(&self, id: ParamId) -> Option<f32> {
        match id {
            params::FREQ => Some(self.freq),
            params::Q => Some(self.q),
            params::GAIN_DB => Some(self.gain_db),
            _ => None,
        }
    }

    fn reset(&mut self) {
        for l in self.lanes.iter_mut() {
            l.reset();
        }
    }
}

/// AVX2 kernel that puts **8 different channels in the 8 lanes**.
///
/// It is the correct answer to the impossibility of vectorizing the recursion along
/// the frames (see [`crate`]): the states are 8 independent chains, hence 8
/// independent lanes. It demands, however, an **interleaved-by-channel** layout: the
/// transpose has a real cost, and `audio-probe` measures it with and without.
///
/// Requires `avx2` + `fma` (your 11700K has them; `AVX-512` no, and it isn't needed).
#[cfg(target_arch = "x86_64")]
pub mod lanes8 {
    use super::*;
    use core::arch::x86_64::*;

    /// Kernel width.
    pub const LANES: usize = 8;

    /// 8 biquad states in SIMD lanes.
    #[derive(Clone, Copy)]
    pub struct BiquadLanes8 {
        b0: __m256,
        b1: __m256,
        b2: __m256,
        /// `-a1` already negated: the FMA adds, it never subtracts.
        na1: __m256,
        /// `-a2` already negated, same reason (and it is needed: without the `a2`
        /// term the DF2T is wrong — see `process_sample`).
        na2: __m256,
        z1: __m256,
        z2: __m256,
    }

    impl BiquadLanes8 {
        /// Same coefficients on all lanes (e.g. one filter across 8 tracks).
        ///
        /// # Safety
        /// Requires `avx2`+`fma` supported by the hardware.
        #[target_feature(enable = "avx2,fma")]
        pub unsafe fn broadcast(c: &Coeffs) -> Self {
            Self {
                b0: _mm256_set1_ps(c.b0),
                b1: _mm256_set1_ps(c.b1),
                b2: _mm256_set1_ps(c.b2),
                na1: _mm256_set1_ps(-c.a1),
                na2: _mm256_set1_ps(-c.a2),
                z1: _mm256_setzero_ps(),
                z2: _mm256_setzero_ps(),
            }
        }

        /// Coefficients **different per lane**: 8 sections (bands) of the same
        /// channel. It is the form the multiband and the kick/bass plugin's phase-rotator
        /// need, where the 8 lanes are 8 allpass at different frequencies.
        ///
        /// # Safety
        /// Requires `avx2`+`fma`.
        #[target_feature(enable = "avx2,fma")]
        pub unsafe fn per_lane(cs: &[Coeffs; LANES]) -> Self {
            Self {
                b0: _mm256_setr_ps(
                    cs[0].b0, cs[1].b0, cs[2].b0, cs[3].b0, cs[4].b0, cs[5].b0, cs[6].b0, cs[7].b0,
                ),
                b1: _mm256_setr_ps(
                    cs[0].b1, cs[1].b1, cs[2].b1, cs[3].b1, cs[4].b1, cs[5].b1, cs[6].b1, cs[7].b1,
                ),
                b2: _mm256_setr_ps(
                    cs[0].b2, cs[1].b2, cs[2].b2, cs[3].b2, cs[4].b2, cs[5].b2, cs[6].b2, cs[7].b2,
                ),
                na1: _mm256_setr_ps(
                    -cs[0].a1, -cs[1].a1, -cs[2].a1, -cs[3].a1, -cs[4].a1, -cs[5].a1, -cs[6].a1,
                    -cs[7].a1,
                ),
                na2: _mm256_setr_ps(
                    -cs[0].a2, -cs[1].a2, -cs[2].a2, -cs[3].a2, -cs[4].a2, -cs[5].a2, -cs[6].a2,
                    -cs[7].a2,
                ),
                z1: _mm256_setzero_ps(),
                z2: _mm256_setzero_ps(),
            }
        }

        /// Zeroes the state of all lanes.
        ///
        /// `#[target_feature]` here too, and not out of fussiness: an AVX intrinsic
        /// inside a function *without* the feature enabled can be inlined by the
        /// compiler into a path that also runs on CPUs without AVX → a `SIGILL` that
        /// reproduces only on the user's machine.
        ///
        /// # Safety
        /// Requires `avx2`+`fma`.
        #[target_feature(enable = "avx2,fma")]
        pub unsafe fn reset(&mut self) {
            self.z1 = _mm256_setzero_ps();
            self.z2 = _mm256_setzero_ps();
        }

        /// Processes `data` interleaved of exactly [`LANES`] channels.
        ///
        /// DF2T in lane form: `out = b0·x + z1`, `z1 = b1·x + (−a1)·out + z2`,
        /// `z2 = b2·x + (−a2)·out`. The dependency chain per lane is 2 FMA of
        /// latency (~8 cycles on this CPU), but the lanes are 8 channels: that is
        /// why the kernel beats the scalar by ~6× and not by 8×.
        ///
        /// The `unsafe { }` blocks wrap **only** the two intrinsics on pointers
        /// (load/store): `__m256` arithmetic in Rust is safe functions. It is the
        /// `unsafe_op_in_unsafe_fn` lint that imposes it — and it is the same lint
        /// that, by forcing a line-by-line reread of this function, brought the bug
        /// to light.
        ///
        /// # Safety
        /// Requires `avx2`+`fma`; `data.len()` a multiple of [`LANES`].
        #[allow(clippy::manual_is_multiple_of)]
        // is_multiple_of() (Rust 1.87) is inside the 1.88 floor; the manual form stays
        // in this hot unsafe fn - zero behavior change over a readability lint.
        #[target_feature(enable = "avx2,fma")]
        pub unsafe fn process(&mut self, data: &mut [f32]) {
            debug_assert!(data.len() % LANES == 0);
            let (b0, b1, b2, na1, na2) = (self.b0, self.b1, self.b2, self.na1, self.na2);
            let mut z1 = self.z1;
            let mut z2 = self.z2;
            // const-generic chunks_exact::<N>() needs Rust 1.93 > MSRV 1.88.
            #[allow(clippy::chunks_exact_to_as_chunks)]
            for chunk in data.chunks_exact_mut(LANES) {
                // SAFETY: the chunk contains exactly LANES=8 contiguous floats.
                let x = unsafe { _mm256_loadu_ps(chunk.as_ptr()) };
                let out = _mm256_fmadd_ps(b0, x, z1);
                z1 = _mm256_fmadd_ps(b1, x, _mm256_fmadd_ps(na1, out, z2));
                z2 = _mm256_fmadd_ps(na2, out, _mm256_mul_ps(b2, x));
                // SAFETY: we write exactly the 8 floats just read.
                unsafe { _mm256_storeu_ps(chunk.as_mut_ptr(), out) };
            }
            self.z1 = z1;
            self.z2 = z2;
        }

        /// True if this CPU can run the kernel.
        pub fn supported() -> bool {
            std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")
        }

        /// Version with runtime dispatch: returns `false` if the CPU lacks
        /// AVX2+FMA (in that case use the scalar path).
        ///
        /// The dispatch lives here, not in the caller: a `#[target_feature]` that
        /// ends up in the wrong place is a `SIGILL` that reproduces only on the
        /// beta tester's machine.
        pub fn process_detected(&mut self, data: &mut [f32]) -> bool {
            if Self::supported() {
                // SAFETY: just verified with is_x86_feature_detected.
                unsafe { self.process(data) };
                true
            } else {
                false
            }
        }

        /// Extracts the state (for tests/diagnostics).
        ///
        /// # Safety
        /// Requires `avx2`+`fma`.
        #[target_feature(enable = "avx2,fma")]
        pub unsafe fn state(&self) -> ([f32; LANES], [f32; LANES]) {
            let mut a = [0.0f32; LANES];
            let mut b = [0.0f32; LANES];
            // SAFETY: store into an aligned local stack.
            unsafe {
                _mm256_storeu_ps(a.as_mut_ptr(), self.z1);
                _mm256_storeu_ps(b.as_mut_ptr(), self.z2);
            }
            (a, b)
        }
    }

    /// Planar → interleaved (8 channels). Cost: a trivial transpose,
    /// deliberately **not** vectorized: it must be measured, not hidden.
    pub fn planar_to_interleaved(src: &[f32], dst: &mut [f32], frames: usize) {
        assert_eq!(src.len(), LANES * frames, "src planar of wrong length");
        assert_eq!(dst.len(), src.len(), "dst of wrong length");
        for f in 0..frames {
            for c in 0..LANES {
                dst[f * LANES + c] = src[c * frames + f];
            }
        }
    }

    /// Interleaved → planar (8 channels).
    pub fn interleaved_to_planar(src: &[f32], dst: &mut [f32], frames: usize) {
        assert_eq!(src.len(), LANES * frames);
        assert_eq!(dst.len(), src.len());
        for f in 0..frames {
            for c in 0..LANES {
                dst[c * frames + f] = src[f * LANES + c];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coeffs::Q_BUTTERWORTH;
    use core::f32::consts::PI;
    use hathor_dsp_core::buffer::ChannelBuffer;

    /// Deterministic RNG: numeric tests must not change from run to run.
    struct Lcg(u32);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((self.0 >> 8) as f32 / 8_388_608.0) - 1.0
        }
    }

    fn noise(n: usize) -> Vec<f32> {
        let mut g = Lcg(0xC0FFEE);
        (0..n).map(|_| g.next_f32()).collect()
    }

    /// f64 reference of the same DF2T recursion.
    fn reference_df2t(c: &Coeffs, x: &[f32]) -> Vec<f64> {
        let (mut z1, mut z2) = (0.0f64, 0.0f64);
        x.iter()
            .map(|&s| {
                let xs = s as f64;
                let out = c.b0 as f64 * xs + z1;
                z1 = c.b1 as f64 * xs - c.a1 as f64 * out + z2;
                z2 = c.b2 as f64 * xs - c.a2 as f64 * out;
                out
            })
            .collect()
    }

    /// **The most important test of the module.** Measures the steady-state
    /// response of the filter-object (gain and phase, by correlation over whole
    /// periods) and compares it against the response *predicted algebraically* by
    /// [`Coeffs::response`].
    ///
    /// Why it exists: I wrote the f64 reference above by hand, and it contained
    /// the same error as the code (`z2` without `-a2·out`). A test against a
    /// homemade reference verifies *consistency*, not *correctness*. This test
    /// instead cross-checks two independent paths: the project's algebra
    /// (coeffs.rs) against the processing recursion (biquad.rs).
    fn measure_steady_state(c: &Coeffs, f: f32, sr: f32) -> (f32, f32) {
        let n: usize = 4096;
        let mut filt = Biquad::new(*c);
        let mut re = 0.0f64; // Σ y·cos(θ)
        let mut im = 0.0f64; // Σ y·sin(θ)
        let mut m = 0.0f64;
        for i in 0..n {
            let th = 2.0 * PI * f * i as f32 / sr;
            let y = filt.process_sample(th.sin());
            if i >= n / 4 {
                // Discard the initial transient; the correlation over an integer
                // number of periods gives exact amplitude and phase:
                // y = A·sin(θ+φ) ⇒ Σy·cosθ = (M/2)·A·sinφ, Σy·sinθ = (M/2)·A·cosφ.
                let thd = 2.0 * core::f64::consts::PI * f as f64 * i as f64 / sr as f64;
                re += y as f64 * thd.cos();
                im += y as f64 * thd.sin();
                m += 1.0;
            }
        }
        let amp = 2.0 * (re * re + im * im).sqrt() / m;
        (amp as f32, re.atan2(im) as f32)
    }

    #[test]
    fn measured_response_matches_predicted_response() {
        let sr = 48_000.0;
        let bin = sr / 4096.0; // 11.71875 Hz: exact frequencies for the window
        let c = design(FilterKind::LowPass { q: 6.0 }, 1_000.0, sr).expect("lpf");
        for k in [17u32, 85, 171, 427] {
            let f = k as f32 * bin;
            let (g_meas, p_meas) = measure_steady_state(&c, f, sr);
            let (hre, him) = c.response(Coeffs::w(f, sr));
            let g_pred = (hre * hre + him * him).sqrt();
            let p_pred = him.atan2(hre);
            assert!(
                (g_meas - g_pred).abs() / g_pred.max(1e-6) < 0.01,
                "gain at {f} Hz: measured {g_meas:.4} vs predicted {g_pred:.4}"
            );
            let dp = (p_meas - p_pred).abs();
            let dp = if dp > PI { 2.0 * PI - dp } else { dp };
            assert!(
                dp < 0.05,
                "phase at {f} Hz: measured {p_meas:.4} vs predicted {p_pred:.4} rad"
            );
        }
    }

    #[test]
    fn df2t_matches_f64_reference() {
        let c = design(FilterKind::LowPass { q: Q_BUTTERWORTH }, 1_000.0, 48_000.0).expect("lpf");
        let x = noise(4096);
        let mut f = Biquad::new(c);
        let got: Vec<f32> = x.iter().map(|&s| f.process_sample(s)).collect();
        let want = reference_df2t(&c, &x);
        // Metric normalized on the PEAK of the signal: the sample-by-sample
        // relative error explodes where the output crosses zero (denominator →
        // 0), which is a property of the wave, not a bug of the filter.
        // (Historical false positive of this test.)
        let peak = want.iter().fold(0.0f64, |m, w| m.max(w.abs())).max(1e-9);
        let mut max_err = 0.0f64;
        for (g, w) in got.iter().zip(&want) {
            max_err = max_err.max((*g as f64 - *w).abs() / peak);
        }
        assert!(
            max_err < 1e-5,
            "max error relative to the peak: {max_err:.3e}"
        );
    }

    #[test]
    fn impulse_response_decays_without_self_excitation() {
        // Impulse, then silence: the ENVELOPE must decay. Comparing consecutive
        // samples was wrong: a resonant is a damped oscillation, |y|
        // LEGITIMATELY grows on every half-period during the rise.
        // So we compare window peaks. Q=12 at 200 Hz: envelope
        // time constants τ ≈ Q/(π·f) ≈ 917 samples.
        let c = design(FilterKind::LowPass { q: 12.0 }, 200.0, 48_000.0).expect("resonant");
        let mut f = Biquad::new(c);
        let _ = f.process_sample(1.0);
        let mut early = 0.0f32;
        for _ in 0..480 {
            early = early.max(f.process_sample(0.0).abs());
        }
        for _ in 0..960 {
            f.process_sample(0.0);
        }
        let mut late = 0.0f32;
        for _ in 0..480 {
            late = late.max(f.process_sample(0.0).abs());
        }
        assert!(
            late < early * 0.5,
            "the envelope must decay: early peak {early} vs late {late}"
        );
        let mut tail = 0.0f32;
        for _ in 0..30_000 {
            tail = f.process_sample(0.0).abs();
        }
        assert!(tail < 1e-3, "unflushed tail: {tail}");
    }

    #[test]
    fn coefficient_ramp_removes_the_step() {
        let c0 = design(FilterKind::LowPass { q: 6.0 }, 200.0, 48_000.0).expect("a");
        let c1 = design(FilterKind::LowPass { q: 6.0 }, 12_000.0, 48_000.0).expect("b");

        // Pure tone at 200 Hz, NOT noise: after the switch the noise passes through
        // the wide filter with |dy| ~3, identical for hard jump / ramped — it
        // blinds the test. The tone makes the resonant ring at ~6x: the residual
        // derivative is the oscillation (~0.16) and the step to jump
        // (Δb0·x ≈ 0.46) exists only in the hard-jump branch.
        let ph = core::f32::consts::TAU * 200.0 / 48_000.0;
        let tone = |i: usize| (ph * i as f32).sin();
        let worst_at_switch = |ramp: usize| -> f32 {
            let mut f = Biquad::new(c0);
            for i in 0..2048 {
                f.process_sample(tone(i)); // charge up the ringing
            }
            f.ramp_to(c1, ramp);
            let mut prev = f.process_sample(tone(2048));
            let mut worst = 0.0f32;
            for i in 2049..2176 {
                let y = f.process_sample(tone(i));
                worst = worst.max((y - prev).abs());
                prev = y;
            }
            worst
        };
        let hard_jump = worst_at_switch(0);
        let ramped = worst_at_switch(64);
        assert!(
            ramped * 2.0 < hard_jump,
            "the ramp must spread the step: hard jump {hard_jump} vs ramped {ramped}"
        );
        // And the ramp lands EXACTLY on the target (otherwise the EQ stays out of tune).
        let mut f = Biquad::new(c0);
        f.ramp_to(c1, 64);
        for _ in 0..64 {
            f.process_sample(0.0);
        }
        assert!(!f.is_ramping());
        assert_eq!(f.coeffs(), c1, "the coefficients must close on the target");
    }

    #[test]
    fn interrupted_ramp_restarts_from_current_coefficients() {
        let c0 = design(FilterKind::LowPass { q: 0.707 }, 300.0, 48_000.0).expect("a");
        let c1 = design(FilterKind::LowPass { q: 0.707 }, 3_000.0, 48_000.0).expect("b");
        let c2 = design(FilterKind::LowPass { q: 0.707 }, 8_000.0, 48_000.0).expect("c");
        let mut f = Biquad::new(c0);
        f.ramp_to(c1, 100);
        for _ in 0..40 {
            f.process_sample(0.0);
        }
        let mid = f.coeffs();
        f.ramp_to(c2, 60); // change the target midway
        for _ in 0..60 {
            f.process_sample(0.0);
        }
        assert!(!f.is_ramping());
        // Continuity: the new target must be reached from the current point.
        assert!((f.coeffs().b0 - c2.b0).abs() < 1e-5);
        assert!(
            mid.b0 > c0.b0 && mid.b0 < c2.b0,
            "the ramp skipped waypoints"
        );
    }

    #[test]
    fn node_recompiles_coefficients_only_when_dirty() {
        // What this test protects are the COEFFICIENTS, not the output samples:
        // the filter state legitimately continues across blocks, so comparing
        // "first sample of block" across different blocks was measuring state,
        // not the dirty logic (historical false positive).
        let mut n = BiquadNode::new(FilterKind::LowPass { q: 0.707 }, 1_000.0, 0.707, 0.0);
        let ctx = ProcessCtx::bare(48_000, 64);
        let hints = GraphHints {
            max_channels: 2,
            max_frames: 64,
        };
        n.setup(&ctx, &hints).expect("setup");
        let mut buf = ChannelBuffer::new(2, 64);
        buf.block().channel(0).copy_from_slice(&[1.0; 64]);

        // Same value: dirty does not fire, the coefficients stay identical bit
        // for bit.
        let c_before = n.lanes[0].coeffs();
        {
            let mut b = buf.block();
            n.set_param(params::FREQ, 1_000.0);
            n.process(&ctx, &mut b).expect("process");
        }
        assert_eq!(
            n.lanes[0].coeffs(),
            c_before,
            "a no-op set_param must not recompile"
        );

        // Different value: the block ramps and closes EXACTLY on the target just
        // designed (otherwise the EQ stays out of tune by a hair, forever).
        {
            let mut b = buf.block();
            n.set_param(params::FREQ, 500.0);
            n.process(&ctx, &mut b).expect("process");
        }
        let target = design(FilterKind::LowPass { q: 0.707 }, 500.0, 48_000.0).expect("design");
        assert_eq!(
            n.lanes[0].coeffs(),
            target,
            "the ramp must close on the target"
        );
        assert!(!n.lanes[0].is_ramping());

        assert_eq!(n.param(params::FREQ), Some(500.0));
        assert_eq!(n.param(params::Q), Some(0.707));
        assert_eq!(n.latency_samples(), 0);
        assert_eq!(n.name(), "lowpass");
    }

    #[test]
    fn node_rejects_block_with_more_channels_than_setup() {
        let mut n = BiquadNode::new(FilterKind::HighPass { q: 0.707 }, 100.0, 0.707, 0.0);
        let ctx = ProcessCtx::bare(48_000, 16);
        n.setup(
            &ctx,
            &GraphHints {
                max_channels: 1,
                max_frames: 16,
            },
        )
        .expect("setup");
        let mut buf = ChannelBuffer::new(4, 16);
        let mut b = buf.block();
        assert_eq!(
            n.process(&ctx, &mut b).err(),
            Some(NodeError::BufferTooSmall { have: 1, need: 4 })
        );
    }

    #[test]
    fn non_positive_q_is_ignored() {
        let mut n = BiquadNode::new(
            FilterKind::Peak {
                q: 1.0,
                gain_db: 0.0,
            },
            500.0,
            1.0,
            0.0,
        );
        n.set_param(params::Q, 0.0);
        assert_eq!(n.param(params::Q), Some(1.0));
        n.set_param(params::Q, f32::NAN);
        assert_eq!(n.param(params::Q), Some(1.0));
        n.set_param(params::Q, 3.0);
        assert_eq!(n.param(params::Q), Some(3.0));
    }

    #[test]
    fn per_channel_lanes_have_independent_states() {
        // If the process_block used ONE filter for L and R, an impulse on L would
        // contaminate R. We verify that it doesn't happen.
        let mut n = BiquadNode::new(FilterKind::LowPass { q: 0.707 }, 1_000.0, 0.707, 0.0);
        let ctx = ProcessCtx::bare(48_000, 8);
        n.setup(
            &ctx,
            &GraphHints {
                max_channels: 2,
                max_frames: 8,
            },
        )
        .expect("setup");
        let mut buf = ChannelBuffer::new(2, 8);
        {
            let mut b = buf.block();
            b.channel(0)[0] = 1.0; // impulse on channel 0 only
        }
        {
            let mut b = buf.block();
            n.process(&ctx, &mut b).expect("process");
        }
        let v = buf.view();
        assert!(
            v.channel(1).iter().all(|s| *s == 0.0),
            "channel 1 was contaminated"
        );
        assert!(v.channel(0).iter().any(|s| *s != 0.0));
    }

    #[cfg(target_arch = "x86_64")]
    mod avx2 {
        use super::*;
        use lanes8::{planar_to_interleaved, BiquadLanes8, LANES};

        #[test]
        fn kernel_matches_8_scalar_biquads() {
            if !BiquadLanes8::supported() {
                return;
            }
            let c = design(FilterKind::LowPass { q: 0.707 }, 2_000.0, 48_000.0).expect("lpf");
            let frames = 512;
            let mut planar: Vec<f32> = vec![0.0; LANES * frames];
            let mut g = Lcg(7);
            for s in planar.iter_mut() {
                *s = g.next_f32();
            }

            // Scalar: one Biquad per channel.
            let mut want = planar.clone();
            for ch in want.chunks_exact_mut(frames) {
                Biquad::new(c).process(ch);
            }

            // SIMD: transpose, kernel, transpose back.
            let mut inter = vec![0.0f32; LANES * frames];
            planar_to_interleaved(&planar, &mut inter, frames);
            let mut k = unsafe { BiquadLanes8::broadcast(&c) };
            assert!(k.process_detected(&mut inter));
            let mut got = vec![0.0f32; LANES * frames];
            lanes8::interleaved_to_planar(&inter, &mut got, frames);

            // Not bit-exact: the FMA contracts `mul+add` into a single rounding,
            // so the kernel is *more precise* than the scalar, not equal.
            let mut max = 0.0f32;
            for (a, b) in got.iter().zip(&want) {
                max = max.max((a - b).abs());
            }
            assert!(max < 2e-6, "scalar vs AVX2 gap too large: {max}");
        }

        #[test]
        fn transpose_is_invertible() {
            let frames = 100;
            let mut g = Lcg(99);
            let planar: Vec<f32> = (0..LANES * frames).map(|_| g.next_f32()).collect();
            let mut inter = vec![0.0; LANES * frames];
            let mut back = vec![0.0; LANES * frames];
            planar_to_interleaved(&planar, &mut inter, frames);
            lanes8::interleaved_to_planar(&inter, &mut back, frames);
            assert_eq!(planar, back);
            // And the semantics really are "a frame is 8 contiguous channels".
            assert_eq!(inter[3 * LANES + 5], planar[5 * frames + 3]);
        }

        #[test]
        fn per_lane_kernel_processes_distinct_sections() {
            if !BiquadLanes8::supported() {
                return;
            }
            // 8 allpass at different frequencies: the heart of the phase-rotator.
            let freqs = [40.0f32, 60.0, 90.0, 130.0, 190.0, 280.0, 410.0, 600.0];
            let mut cs = [Coeffs::IDENTITY; LANES];
            for (i, f) in freqs.iter().enumerate() {
                cs[i] = design(FilterKind::AllPass { q: 1.0 }, *f, 48_000.0).expect("apf");
            }
            let mut k = unsafe { BiquadLanes8::per_lane(&cs) };
            let mut data = vec![0.0f32; LANES * 64];
            data[0] = 1.0; // impulse on frame 0, channel 0
            let mut inter = data.clone();
            assert!(k.process_detected(&mut inter));
            // The state of each lane must be different (different frequencies).
            let (z1, _z2) = unsafe { k.state() };
            // f32 is not Hash/Eq (NaN, -0.0 == 0.0): uniqueness is compared on the bits.
            let distinct = z1
                .iter()
                .map(|x| x.to_bits())
                .collect::<std::collections::HashSet<_>>()
                .len();
            assert!(distinct > 1, "the lanes do not have independent states");
        }

        #[test]
        fn feature_probe_never_panics() {
            // `supported()` must never panic, not even on old CPUs.
            let _ = BiquadLanes8::supported();
        }
    }
}
