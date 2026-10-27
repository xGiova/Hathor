//! Denormals (FTZ/DAZ): the one line that prevents "the project that slows down
//! after 20 minutes".
//!
//! An IIR with feedback (biquad, one-pole, a reverb comb) approaching silence
//! produces numbers < 1.18e-38. On most x86 CPUs a denormal costs **from 10 to 100
//! times** a normal float. It is not a constant overhead: it depends on the values,
//! so the symptom is a load meter that climbs *at random* and a dropout you cannot
//! reproduce.
//!
//! The cure is a write to MXCSR: bit 15 (FTZ, flush outputs to zero) + bit 6 (DAZ,
//! treat denormal inputs as zero). It is **per-thread**: call it once at audio thread
//! start, not per block.
//!
//! Honesty note: FTZ/DAZ changes results (a 1e-40 becomes 0). For audio that is
//! irrelevant; for a numerical check in `--offline` with a bit-exact comparison
//! against a reference, turn it off and say so in the test.

/// MXCSR mask: FTZ (bit 15) | DAZ (bit 6).
const FTZ_DAZ: u32 = 0x8000 | 0x0040;

/// Enables FTZ+DAZ on the current thread. Returns `false` if the architecture is not
/// x86/x86-64 (in that case the call is a no-op).
///
/// Uses `asm!` instead of `_mm_getcsr/_mm_setcsr`, which in std are deprecated
/// precisely in favour of inline assembly. `stmxcsr`/`ldmxcsr` take an **m32 memory
/// operand**, not a register: hence the `[{}]` form with a pointer.
pub fn disable_denormals() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        let mut csr: u32 = 0;
        // SAFETY: we write/read 4 bytes in two local `u32`s that live for the whole
        // block; we do not touch the stack and assume nothing about aliased memory.
        unsafe {
            core::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack));
            let set = csr | FTZ_DAZ;
            core::arch::asm!("ldmxcsr [{}]", in(reg) &set, options(nostack));
        }
        true
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    false
}

/// True if FTZ+DAZ are active on the current thread.
pub fn denormals_disabled() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        let mut csr: u32 = 0;
        // SAFETY: 4 bytes written into a local `u32`.
        unsafe {
            core::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack));
        }
        (csr & FTZ_DAZ) == FTZ_DAZ
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    false
}

/// Counter: how many denormal samples a slice holds. Feeds the regression test
/// "does my reverb still produce denormals?"
pub fn count_denormals(data: &[f32]) -> usize {
    data.iter()
        .filter(|s| s.is_finite() && **s != 0.0 && s.abs() < f32::MIN_POSITIVE)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_to_zero_mode_produces_no_denormals() {
        if !disable_denormals() {
            return;
        }
        assert!(denormals_disabled());
        // Geometric decay: without FTZ it would reach denormals in a few iterations.
        let mut y = 1.0f32;
        for _ in 0..20_000 {
            y *= 0.5;
        }
        assert_eq!(
            count_denormals(&[y]),
            0,
            "with FTZ active `y` must be 0.0, was {y}"
        );
    }

    #[test]
    fn counter_detects_a_synthetic_denormal() {
        let d = f32::MIN_POSITIVE / 2.0;
        assert!(!d.is_normal() && d != 0.0);
        assert_eq!(count_denormals(&[d, 1.0, 0.0, f32::MIN_POSITIVE]), 1);
    }
}
