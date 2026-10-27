//! The realtime-contract proof for the FFT kernels (spec invariant 1):
//! `FftPlan::new`/`RealFftPlan::new` are the only allocators, `process`/`inverse`/
//! `RealFftPlan::process` never allocate.
//!
//! Method, per CONVENTION §2.5 and the core guard: the test binary installs
//! [`CheckedAllocator`], each run-phase test enters an [`AudioSection`] and asserts
//! the thread-local counter **stays zero** — no deliberate panic anywhere, so the
//! `catch_unwind`/Box-path trap (`crates/hathor-dsp-core/examples/guard_probe.rs`:
//! the guard's panic inside `vec![..]`'s exchange_malloc is nounwind and escapes the
//! catch) never applies here. The two control tests use *measure* mode (count, never
//! panic) to prove the harness itself can see allocations — a guard that counts
//! nothing would pass a zero-counter assertion forever.
//!
//! Release-trap discipline (the lesson M0 already paid once): every value whose
//! *computation* is under test must stay materialized — `black_box` at the call and
//! a real assertion on the results afterwards, so the release optimizer cannot
//! elide the very work the section was entered to guard.

use hathor_dsp_core::alloc_guard::{reset_counters, set_panic_on_audio_alloc};
use hathor_dsp_core::{AudioSection, CheckedAllocator};
use hathor_dsp_fft::{Complex, FftPlan, RealFftPlan};
use std::hint::black_box;

#[global_allocator]
static GUARDED: CheckedAllocator = CheckedAllocator::new();

// These tests flip process-global state (PANIC_MODE) and read the shared counters:
// serialized per the alloc_guard doc-test pattern. `into_inner` on a poisoned
// `Mutex<()>` is the documented deviation (STYLE §D.4): there is no data inside.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

/// Setup runs the constructor and its tables OUTSIDE the section, then hands the
/// caller memory to the guarded phase.
#[test]
fn fft_plan_process_does_not_allocate_inside_an_audio_section() {
    let _lock = serial();
    reset_counters();
    let n = 1024;
    let mut plan = FftPlan::new(n);
    let mut data = vec![Complex::ZERO; n];
    data[0] = Complex::new(1.0, 0.0); // impulse: the result is checkable in f32

    let section = AudioSection::enter().expect("enter AudioSection");
    plan.process(black_box(&mut data));
    let allocs = section.allocs_here();
    drop(section);

    black_box(&plan);
    assert_eq!(
        allocs, 0,
        "process allocated {allocs} times inside the section"
    );
    // The work was real even if the optimizer later sees only the assert: an
    // impulse must fill every bin.
    let re_sum: f64 = data.iter().map(|c| c.re as f64).sum();
    let im_max: f32 = data.iter().map(|c| c.im.abs()).fold(0.0f32, f32::max);
    assert!(
        (re_sum - n as f64).abs() < 1e-2 && im_max < 1e-4,
        "flat unit spectrum expected, re_sum {re_sum}, im_max {im_max}"
    );
}

#[test]
fn fft_plan_inverse_does_not_allocate_inside_an_audio_section() {
    let _lock = serial();
    reset_counters();
    let n = 256;
    let mut plan = FftPlan::new(n);
    let mut data = vec![Complex::ZERO; n];
    data[0] = Complex::new(1.0, 0.0);
    plan.process(&mut data); // forward, outside: setup-flavored

    let section = AudioSection::enter().expect("enter AudioSection");
    plan.inverse(black_box(&mut data));
    let allocs = section.allocs_here();
    drop(section);

    assert_eq!(
        allocs, 0,
        "inverse allocated {allocs} times inside the section"
    );
    // IFFT of the all-ones spectrum is the impulse at index 0.
    let energy_out: f32 = data[1..].iter().map(|c| c.norm_sqr()).fold(0.0, f32::max);
    assert!(
        (data[0].re - 1.0).abs() < 1e-4 && energy_out < 1e-8,
        "round trip lost the impulse: data[0] {:?}, tail max {energy_out:.3e}",
        data[0]
    );
}

#[test]
fn real_plan_process_does_not_allocate_inside_an_audio_section() {
    let _lock = serial();
    reset_counters();
    let n = 2048;
    let mut plan = RealFftPlan::new(n);
    let samples: Vec<f32> = (0..n)
        .map(|j| if j % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    let mut bins = vec![Complex::ZERO; n / 2 + 1];

    let section = AudioSection::enter().expect("enter AudioSection");
    plan.process(black_box(&samples), black_box(&mut bins));
    let allocs = section.allocs_here();
    drop(section);

    black_box(&plan);
    assert_eq!(allocs, 0, "real process allocated {allocs} times");
    // Alternating ±1 is pure Nyquist: bin N/2 carries N, everything else silence.
    let dc = bins[0].re;
    let nyq = bins[n / 2].re;
    let rest: f32 = bins[1..n / 2]
        .iter()
        .map(|c| c.norm_sqr())
        .fold(0.0, f32::max);
    assert!(
        nyq > (n as f32 - 1.0) && dc.abs() < 1.0 && rest < 1e-3,
        "expected Nyquist-line spectrum, got dc {dc}, nyq {nyq}, rest {rest:.3e}"
    );
}

/// Positive control 1 (measure mode): the *constructor* does allocate — if the
/// guard could not see these allocations, the three zero-counter assertions above
/// would be vacuous.
#[test]
fn constructor_allocations_are_visible_to_the_guard() {
    let _lock = serial();
    set_panic_on_audio_alloc(false);
    reset_counters();
    let n = {
        let section = AudioSection::enter().expect("enter AudioSection");
        let plan = black_box(FftPlan::new(256));
        let c = black_box(RealFftPlan::new(512));
        let n = section.allocs_here();
        drop(plan);
        drop(c);
        n
    };
    set_panic_on_audio_alloc(true);
    assert!(
        n >= 2,
        "two constructors must show up in the counter, got {n}"
    );
}

/// Positive control 2 (measure mode): an ordinary allocation inside a section *is*
/// counted. The `Vec::new` + `push` form is deliberate: `vec![..]` routes through
/// the nounwind Box path where the guard's own panic escapes `catch_unwind`
/// (examples/guard_probe.rs proves the difference) — measure mode never panics, but
/// staying on the unwind-safe form keeps the pattern copy-paste-safe for strict
/// tests elsewhere.
#[test]
fn the_guard_counts_an_ordinary_allocation() {
    let _lock = serial();
    set_panic_on_audio_alloc(false);
    reset_counters();
    let n = {
        let section = AudioSection::enter().expect("enter AudioSection");
        // Allow on the enclosing block (the guard_probe.rs form): the lint spans
        // let + push together; an attribute on the `let` alone does not reach it.
        #[allow(clippy::vec_init_then_push)]
        {
            let mut probe: Vec<u8> = Vec::new();
            probe.push(black_box(7));
            let counted = section.allocs_here();
            drop(probe);
            counted
        }
    };
    set_panic_on_audio_alloc(true);
    assert!(n >= 1, "the guard must see a grow-into allocation, got {n}");
}
