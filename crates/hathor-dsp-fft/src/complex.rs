//! The complex sample: two `f32`s and nothing else.
//!
//! ## Why a struct when `(f32, f32)` would compile
//!
//! The module doc recorded this decision
//! ([docs/modules/hathor-dsp-fft.md](../../../docs/modules/hathor-dsp-fft.md),
//! "Rust concepts"): a named `Copy` struct says `re`/`im` where a tuple says `.0`/`.1`,
//! and a butterfly written as `a + w * b` reads like the algebra it implements. The
//! layout is `#[repr(C)]`, so it is bit-identical to `[f32; 2]` and an AVX kernel can
//! `transmute` slices of it without a repr surprise — if the M1 measurements ever
//! justify one (they do not yet, so there is none).
//!
//! ## Why operators and not methods
//!
//! `Add`/`Sub`/`Mul` on `Copy` values inline to the same 4-6 scalar instructions the
//! hand-written butterfly would compile to; the operator form removes the one failure
//! mode this type could still contribute: a `mul_add`-style sign slip between the
//! `re·re − im·im` terms, visible in the expression instead of buried in a method.

/// A complex number on the FFT's working scale: `f32`.
///
/// `f32` is not an accuracy concession: the analyzer chain lives or dies on ~1e-4
/// relative (see the module doc's test plan), and halving the memory traffic of the
/// twiddle tables matters more at n = 16384 than the extra mantissa bits would.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Complex {
    /// Real part.
    pub re: f32,
    /// Imaginary part.
    pub im: f32,
}

impl Complex {
    /// The additive identity.
    pub const ZERO: Self = Self { re: 0.0, im: 0.0 };

    /// Builds from parts.
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }

    /// Mirror across the real axis. `const fn` so the inverse transform's
    /// conj-sweep is one load-store pass with no call overhead even in debug.
    pub const fn conj(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }

    /// Squared magnitude. The square root is left to the caller: the analyzer wants
    /// `|X|²` (power) or `20·log10|X|` (dB), and a `sqrt` it does not need would be
    /// paid for in the callback.
    pub fn norm_sqr(self) -> f32 {
        self.re * self.re + self.im * self.im
    }

    /// `e^{i·angle}` from an f64 angle: **setup only**. The tables of [`crate::FftPlan`]
    /// and [`crate::RealFftPlan`] are built in f64 and narrowed here once, so the
    /// process-time lookup inherits f64-accurate twiddles at f32 storage cost — the
    /// cheapest accuracy upgrade in the crate, and unavailable in `process` by design
    /// (transcendentals have no bounded-cycle guarantee).
    pub(crate) fn from_angle_rad(angle: f64) -> Self {
        let (sin, cos) = angle.sin_cos();
        Self {
            re: cos as f32,
            im: sin as f32,
        }
    }
}

impl core::ops::Add for Complex {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            re: self.re + rhs.re,
            im: self.im + rhs.im,
        }
    }
}

impl core::ops::Sub for Complex {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            re: self.re - rhs.re,
            im: self.im - rhs.im,
        }
    }
}

impl core::ops::Mul for Complex {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        Self {
            re: self.re * rhs.re - self.im * rhs.im,
            im: self.re * rhs.im + self.im * rhs.re,
        }
    }
}

impl core::ops::Mul<f32> for Complex {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self {
            re: self.re * k,
            im: self.im * k,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiplication_is_the_algebra_not_a_convention() {
        // (1+i)·(1+i) = 2i. The sign of the re term is the one bug this file could
        // hide, so pin it with a hand-checkable case.
        let z = Complex::new(1.0, 1.0);
        assert_eq!(z * z, Complex::new(0.0, 2.0));
        // i·(−i) = 1.
        assert_eq!(
            Complex::new(0.0, 1.0) * Complex::new(0.0, -1.0),
            Complex::new(1.0, 0.0)
        );
    }

    #[test]
    fn conj_is_an_involutive_mirror() {
        let z = Complex::new(0.75, -0.25);
        assert_eq!(z.conj().conj(), z);
        assert_eq!(z + z.conj(), Complex::new(1.5, 0.0));
    }

    #[test]
    fn norm_sqr_avoids_the_sqrt() {
        let z = Complex::new(3.0, 4.0);
        assert_eq!(z.norm_sqr(), 25.0);
    }

    #[test]
    fn repr_c_layout_is_two_contiguous_f32() {
        // The future AVX kernel transmutes &[Complex] to &[f32]; the layout promise
        // is part of this type's API, so it is asserted, not commented.
        let z = Complex::new(1.0, 2.0);
        // SAFETY: `z` is alive for the whole block, `#[repr(C)]` two f32s are 8
        // contiguous readable bytes, and the slice borrows `z` immutably.
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts((&z as *const Complex).cast(), 8) };
        let re = f32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let im = f32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!((re, im), (1.0, 2.0), "re must be the first field in memory");
    }

    #[test]
    fn real_scaling_distributes_over_both_parts() {
        let z = Complex::new(-2.0, 6.0);
        assert_eq!(z * 0.5, Complex::new(-1.0, 3.0));
    }
}
