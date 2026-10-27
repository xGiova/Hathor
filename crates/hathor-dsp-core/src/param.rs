//! Parameters: UI→audio atomics at run time, and click-free ramps.
//!
//! Two separate concepts that are often confused:
//!
//! * the **value coming from the UI** must travel without locks → [`AtomicParam`]
//!   (`AtomicU32` + bit-cast, which is guaranteed lock-free on x86). A `Mutex<f32>`
//!   on the parameter path is the classic false remedy: it works until someone holds
//!   the lock while the driver is calling.
//! * the **value the DSP uses** must never jump → [`SmoothedParam`] /
//!   [`LinearRamp`]. A cutoff step on a resonant filter produces a click: automation
//!   *must* be ramped, always, even when "you cannot hear it".

use core::sync::atomic::{AtomicU32, Ordering};

/// Compact parameter identifier. Stable numbers, not names: a `String` in a
/// `set_param` would be an allocation on the real path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParamId(pub u32);

impl ParamId {
    /// Const constructor.
    pub const fn new(n: u32) -> Self {
        Self(n)
    }
}

/// Parameter shared between a non-realtime thread (the UI) and the audio thread.
///
/// A 32-bit `store`/`load` of `AtomicU32` on x86 is a move: lock-free, and the value
/// is always "one that existed" (no tearing). The audio thread will read the value
/// *at the start* of the block, not mid-frame: that is how an automation becomes
/// deterministic and recordable.
pub struct AtomicParam {
    bits: AtomicU32,
    min: f32,
    max: f32,
}

impl AtomicParam {
    /// Creates with an initial value and range (the range is for the UI, not the DSP).
    pub fn new(value: f32, min: f32, max: f32) -> Self {
        Self {
            bits: AtomicU32::new(value.to_bits()),
            min,
            max,
        }
    }
    /// Writes (UI side). Release ordering: the reader sees every previous change.
    pub fn store(&self, value: f32) {
        self.bits.store(value.to_bits(), Ordering::Release);
    }
    /// Reads (audio side).
    pub fn load(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Acquire))
    }
    /// Minimum of the declared range.
    pub fn min(&self) -> f32 {
        self.min
    }
    /// Maximum of the declared range.
    pub fn max(&self) -> f32 {
        self.max
    }
}

/// A first-order low-pass filter on a parameter value.
///
/// `value += coef * (target - value)` per frame. With `time_s` →
/// `coef = 1 - exp(-1/(tau*fs))`: ~63% rise time in `time_s`, and no ringing (a
/// linear ramp on a resonant cutoff sounds like a "slow click", the one-pole does
/// not).
#[derive(Debug, Clone, Copy)]
pub struct SmoothedParam {
    value: f32,
    target: f32,
    coef: f32,
}

impl SmoothedParam {
    /// Starts already settled on `v`. The default coefficient is conservative
    /// (slow): whoever uses this type must call [`SmoothedParam::set_time`] in
    /// `setup()`, where the sample rate is known.
    pub fn new(v: f32) -> Self {
        Self {
            value: v,
            target: v,
            coef: 1e-3,
        }
    }

    /// Recomputes the coefficient. Call it in setup or when the sample rate changes,
    /// **not** per frame (it does an `exp`). `time_s` is the 63% rise time (the
    /// one-pole time constant τ): 99% is reached at ~6.9·τ.
    pub fn set_time(&mut self, sample_rate: u32, time_s: f32) {
        let tau = time_s.max(1e-6);
        self.coef = 1.0 - (-1.0 / (tau * sample_rate as f32)).exp();
    }

    /// New target (zero cost).
    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Advances one frame and returns the value to use.
    ///
    /// Includes the **anti-stall snap**: in f32, when the increment
    /// `coef·(target−value)` drops below the resolution of `value` (~ULP), the sum
    /// rounds to `value` and the one-pole *never arrives* — it stays parked at
    /// ~ULP/coef from the target forever. This is not theory: it is exactly the bug
    /// the `smoothed_param_converges_and_settles` test caught. When the increment is
    /// below the resolution we jump to the target: the gap is ~1e-6 relative,
    /// inaudible, and `settled()` becomes honest again.
    pub fn step(&mut self) -> f32 {
        let inc = self.coef * (self.target - self.value);
        if inc.abs() <= self.value.abs() * f32::EPSILON {
            self.value = self.target;
        } else {
            self.value += inc;
        }
        self.value
    }

    /// Current value without advancing.
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Skips the ramp: for `reset()` and for initialization, where a click does not
    /// exist yet.
    pub fn snap_to(&mut self, v: f32) {
        self.value = v;
        self.target = v;
    }

    /// True if the value has substantially arrived (to let the DSP sleep).
    pub fn settled(&self) -> bool {
        (self.value - self.target).abs() <= (self.target.abs() * 1e-6).max(1e-9)
    }
}

/// Linear ramp with a fixed step, block-accurate.
///
/// Needed where the one-pole is not enough: automation that must reach **exactly**
/// the target at a given frame (crossfade, precision mute, a `delay` that has to
/// recompute itself without transients). The cost is one `fma` per frame.
#[derive(Debug, Clone, Copy)]
pub struct LinearRamp {
    start: f32,
    target: f32,
    remaining: i64,
    step: f32,
}

impl LinearRamp {
    /// Initially fixed on `v`.
    pub const fn at(v: f32) -> Self {
        Self {
            start: v,
            target: v,
            remaining: 0,
            step: 0.0,
        }
    }

    /// Go to `target` in `frames` frames (0 = hard jump).
    pub fn glide_to(&mut self, target: f32, frames: usize) {
        self.start = self.value();
        self.target = target;
        if frames == 0 || (target - self.start).abs() < f32::EPSILON {
            self.remaining = 0;
            self.step = 0.0;
            self.start = target;
        } else {
            self.remaining = frames as i64;
            self.step = (target - self.start) / frames as f32;
        }
    }

    /// Advance by one frame.
    pub fn step(&mut self) -> f32 {
        if self.remaining <= 0 {
            return self.target;
        }
        self.remaining -= 1;
        self.start += self.step;
        if self.remaining == 0 {
            // Close exactly: no floating-point accumulation residue.
            self.start = self.target;
            self.value()
        } else {
            self.start
        }
    }

    /// Current value.
    pub fn value(&self) -> f32 {
        if self.remaining <= 0 {
            self.target
        } else {
            self.start
        }
    }

    /// True if it is done.
    pub fn done(&self) -> bool {
        self.remaining <= 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_param_round_trips_value() {
        let p = AtomicParam::new(0.5, 0.0, 1.0);
        assert_eq!(p.load(), 0.5);
        p.store(0.25);
        assert_eq!(p.load(), 0.25);
    }

    #[test]
    fn smoothed_param_converges_and_settles() {
        // τ = 5 ms: 99.9% needs ~6.9τ ≈ 35 ms ≈ 1660 frames. 4096 frames (85 ms)
        // also exercise the asymptotic-stall snap: below ULP/coef the one-pole
        // creeps, and `settled()` must still become true.
        let mut s = SmoothedParam::new(0.0);
        s.set_time(48_000, 0.005);
        s.set_target(1.0);
        let mut v = 0.0;
        for _ in 0..4096 {
            v = s.step();
        }
        assert!(v > 0.999, "expected ~1.0, got {v}");
        assert!(s.settled());
    }

    #[test]
    fn smoothed_param_first_step_does_not_jump() {
        // The first step from 0 -> 1 must not jump: it must be <= coef.
        let mut s = SmoothedParam::new(0.0);
        s.set_time(48_000, 0.010);
        s.set_target(1.0);
        let first = s.step();
        assert!(first > 0.0 && first < 0.1, "jump too large: {first}");
    }

    #[test]
    fn linear_ramp_lands_exactly_on_target() {
        let mut r = LinearRamp::at(0.0);
        r.glide_to(1.0, 4);
        let vals: Vec<f32> = (0..6).map(|_| r.step()).collect();
        assert_eq!(vals, vec![0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
        assert!(r.done());
    }

    #[test]
    fn linear_ramp_with_zero_frames_jumps_at_once() {
        let mut r = LinearRamp::at(0.2);
        r.glide_to(0.9, 0);
        assert_eq!(r.step(), 0.9);
    }

    #[test]
    fn linear_ramp_to_the_same_value_finishes_immediately() {
        let mut r = LinearRamp::at(0.5);
        r.glide_to(0.5, 64);
        assert!(r.done());
        assert_eq!(r.step(), 0.5);
    }
}
