//! # `hathor-dsp-measure`: the numbers you can decide on
//!
//! This crate turns two audio signals into answers: "how loud", "how in
//! phase", "by how many samples off", "what survives the null". It is the
//! measurement bench the kick↔bass plugin consults constantly and the
//! acceptance yardstick of the plugins — the same functions that answer the
//! user answer the tests.
//!
//! ## Design principles (from the spec, kept in code)
//!
//! * **Pure functions, no allocation per call.** Results are `f32`/`i32` or
//!   small `Copy` structs; analysis buffers belong to the caller. The tests
//!   prove it mechanically with [`hathor_dsp_core::alloc_guard`] — same
//!   doctrine as the realtime contract: a verifiable rule is worth ten
//!   documented ones.
//! * **dBFS with reference ±1.0, silence is `f32::NEG_INFINITY`** (invariant
//!   2, [`db`]) — never −999, never a magic floor: `max` must keep meaning
//!   "the loudest honest reading".
//! * **Silence is legal** ([`phase::correlation`] returns `0.0`, not `NaN`,
//!   invariant 3), **corruption is not** (`NaN` in → `NaN` out everywhere: a
//!   broken signal that reads like a quiet one hides the bug).
//! * **Sign convention for lag: positive = `b` delayed relative to `a`**
//!   (invariant 4). A flipped sign makes the user "fix" alignment twice as
//!   fast in the wrong direction; the convention is nailed by tests.
//! * **Two numbers, two questions**: similarity ([`phase::correlation`]) and
//!   alignment ([`phase::time_align_lag`]) are different measurements; the UI
//!   shows both, never one derived from the other.
//!
//! ## What is NOT here (deliberately)
//!
//! No FFT cross-correlation yet (M3: needs `hathor-dsp-fft`'s inverse; the
//! naive scan in [`phase`] stays as its reference forever — two independent
//! paths, the `−a2·out` lesson). No loudness BS.1770 (M4), no masking meter
//! (M3), no sweet-spot *classification* (M6: the band lives in the
//! channel-strip contract; this crate supplies honest dBFS via
//! [`tracker::RmsTracker`] and nothing more).
//!
//! Teaching track: [docs/it/dsp.md](../../../docs/it/dsp.md) §5 (correlation
//! as wideband phase, the null test as the most honest measurement) and §6
//! (STFT for masking). Module spec:
//! [docs/modules/hathor-dsp-measure.md](../../../docs/modules/hathor-dsp-measure.md).

#![warn(missing_docs)]
// The "no unwrap" bans apply to production code, not to tests: in a test an
// `.expect()` with a message documents better than a match.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::exit)
)]

pub mod db;
pub mod levels;
pub mod phase;
pub mod tracker;

pub use db::{db_to_lin, lin_to_db};
pub use levels::{peak_db, rms_db};
pub use phase::{correlation, null_residue_db, time_align_lag};
pub use tracker::RmsTracker;

#[cfg(test)]
mod guard_tests {
    //! Zero-allocation proof (invariant 1) with the core guard, and a
    //! negative control showing the guard is *live* in this binary — without
    //! the control, "0 allocations" could just mean "the guard is asleep".
    //! Established pattern from `hathor_dsp_core::alloc_guard`'s tests
    //! (`AudioSection` + serialization mutex + `black_box`); catch_unwind is
    //! allowed here and only here, STYLE §D.1.

    use super::*;
    use core::hint::black_box;
    use hathor_dsp_core::alloc_guard::{
        allocs_last_section, in_audio_section, AudioSection, CheckedAllocator,
    };
    use hathor_dsp_core::buffer::ChannelBuffer;

    #[global_allocator]
    static TEST_ALLOC: CheckedAllocator = CheckedAllocator::new();

    // The guard's counters and PANIC_MODE are process-global; cargo test runs
    // threads in parallel, so every test touching them takes this lock
    // (CONVENTION §4.5). `into_inner` on the poison: `Mutex<()>` carries no
    // data (STYLE §D.4).
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn no_measurement_function_allocates_inside_an_audio_section() {
        let _serial = serial();
        // Setup allocations happen BEFORE entering the section (rule 2).
        let mut buf = ChannelBuffer::new(2, 256);
        {
            let mut b = buf.block();
            for (c, ch) in b.channels_mut().enumerate() {
                for (i, s) in ch.iter_mut().enumerate() {
                    *s = ((i + c * 7) % 97) as f32 * 0.013 - 0.42;
                }
            }
        }
        let a: Vec<f32> = (0..256).map(|i| (i as f32 * 0.05).sin()).collect();
        let b: Vec<f32> = a.iter().map(|x| 0.5 * x + 0.01).collect();
        let mut tracker = RmsTracker::new(0.3);
        tracker.set_time(48_000, 0.3);
        {
            let section = AudioSection::enter().expect("enter audio section");
            let blk = buf.block();
            // black_box: the optimizer must not erase the work — work erased
            // is allocation erased, and the test would prove nothing.
            black_box(&blk);
            black_box(&a);
            black_box(&b);
            let p = black_box(peak_db(&blk));
            let r = black_box(rms_db(&blk));
            let c = black_box(correlation(&a, &b));
            let l = black_box(time_align_lag(&a, &b, 16));
            let n = black_box(null_residue_db(&a, &b, -1.0, true));
            let d = black_box(db_to_lin(black_box(l as f32)));
            tracker.process_block(&blk);
            let t = black_box(tracker.rms_db());
            assert_eq!(
                section.allocs_here(),
                0,
                "invariant 1 violated: a measurement allocated inside the audio section"
            );
            assert!(
                p.is_finite() && r.is_finite() && t.is_finite(),
                "sanity: the readings must also be real numbers (p={p}, r={r}, t={t})"
            );
            assert!(
                c.is_finite() && n.is_finite() && d.is_finite(),
                "sanity: pair measurements on real inputs (c={c}, n={n}, d={d})"
            );
        }
        assert!(!in_audio_section(), "the section must be released on Drop");
    }

    #[test]
    fn the_guard_is_live_in_this_binary() {
        let _serial = serial();
        // Negative control for the test above: the same allocator, in the
        // same section, panics on a Vec push. If this ever stops panicking,
        // `allocs_here == 0` proves nothing.
        // Do NOT "fix" the vec_init_then_push lint with `vec![..]`: Box
        // allocations are nounwind on failure — the guard's panic inside them
        // escapes catch_unwind and kills the thread (proved by
        // `hathor-dsp-core/examples/guard_probe.rs`).
        #[allow(clippy::vec_init_then_push)]
        let r = std::panic::catch_unwind(|| {
            let _section = AudioSection::enter().expect("enter audio section");
            let mut v: Vec<u8> = Vec::new();
            v.push(black_box(1));
        });
        assert!(
            r.is_err(),
            "the CheckedAllocator must panic inside a section"
        );
        assert!(
            !in_audio_section(),
            "the Drop must release the thread even during unwinding"
        );
        assert!(
            allocs_last_section() >= 1,
            "the violation must be counted, not just panicked"
        );
    }
}
