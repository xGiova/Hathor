//! Allocation guard: makes rule #1 *mechanical* instead of mnemonic.
//!
//! ## Why it exists
//!
//! The number one bug of an audio engine written by people coming from garbage
//! collected languages is allocating inside the callback. You do not see it: it
//! works in the studio, it works in the tests, and then it **clicks** live when the
//! system allocator takes a lock contended by the UI thread. In C++ you find out
//! with Valgrind in six months. Here you find out *mechanically*, at the first run:
//! the check is compiled into the behaviour itself.
//!
//! ## How it works
//!
//! [`CheckedAllocator`] wraps `System`. Every `alloc` asks a TLS `Cell` (with `const`
//! initialization, so it **allocates nothing and registers no destructor**) whether
//! the current thread is inside an [`AudioSection`]. If it is:
//!
//! * **strict** mode (default): panic. The TLS goes to `Reporting` *before* the
//!   panic, so the allocations of the panic machinery are allowed and there is no
//!   alloc→panic→format→alloc recursion.
//! * **measure** mode: no panic, only a counter. The probe needs it to say "your
//!   chain does 3 allocations per cycle" instead of dying.
//!
//! [`AudioSection::enter`] is released on `Drop`, even during unwinding, so a panic
//! halfway through the callback does not leave the thread marked as audio.
//!
//! ## Cost
//!
//! One TLS read (`Cell<u8>`) per `alloc`. On the *real* audio path there must be no
//! allocations at all, so the actual cost is zero: you pay it on setup `Vec`s, which
//! are outside the section. The number is measured by `audio-probe`, not promised by
//! me.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const S_IDLE: u8 = 0;
const S_AUDIO: u8 = 1;
const S_REPORTING: u8 = 2;

thread_local! {
    /// Thread state. `Cell<u8>` with `const` init: no lazy-init that allocates, no
    /// destructor registered. It is the only legitimate way to touch TLS *from
    /// inside* a global allocator.
    static STATE: Cell<u8> = const { Cell::new(S_IDLE) };
    /// Allocations that happened while the thread was marked as audio.
    static LOCAL_COUNT: Cell<u64> = const { Cell::new(0) };
}

/// Snapshot of the last closed section, read by the UI/diagnostic thread.
static LAST_SECTION_ALLOCS: AtomicU64 = AtomicU64::new(0);
/// Running total, useful for seeing whether a rare code path fires every 5 minutes.
static TOTAL_ALLOCS: AtomicU64 = AtomicU64::new(0);
/// `true` = panic; `false` = just count.
static PANIC_MODE: AtomicBool = AtomicBool::new(true);

/// Is the current thread inside an audio-critical section?
pub fn in_audio_section() -> bool {
    STATE.with(Cell::get) == S_AUDIO
}

/// Turn panic mode on/off (default: on).
pub fn set_panic_on_audio_alloc(on: bool) {
    PANIC_MODE.store(on, Ordering::Relaxed);
}

/// True if panic mode is active.
pub fn panic_mode() -> bool {
    PANIC_MODE.load(Ordering::Relaxed)
}

/// Allocations detected in the last closed audio section.
pub fn allocs_last_section() -> u64 {
    LAST_SECTION_ALLOCS.load(Ordering::Relaxed)
}

/// Total audio allocations since the process came to life.
pub fn allocs_total() -> u64 {
    TOTAL_ALLOCS.load(Ordering::Relaxed)
}

/// Zeroes the counters. **Only** from the setup/diagnostic thread, never inside a
/// section.
pub fn reset_counters() {
    LAST_SECTION_ALLOCS.store(0, Ordering::Relaxed);
    TOTAL_ALLOCS.store(0, Ordering::Relaxed);
}

/// RAII: marks the current thread as audio-critical for as long as it lives.
///
/// ```ignore
/// // inside the driver callback
/// let _guard = AudioSection::enter()?;   // 0 alloc, 0 lock
/// ```
pub struct AudioSection {
    /// Whether we had to overwrite an already active section (nesting).
    was_nested: bool,
}

impl AudioSection {
    /// Enter the critical section. Does not allocate.
    pub fn enter() -> Result<Self, AudioSectionError> {
        let prev = STATE.with(|s| {
            let p = s.get();
            s.set(S_AUDIO);
            p
        });
        if prev == S_AUDIO {
            return Err(AudioSectionError::AlreadyInside);
        }
        LOCAL_COUNT.with(|c| c.set(0));
        Ok(Self {
            was_nested: prev == S_REPORTING,
        })
    }

    /// Allocations of the section *in progress*.
    pub fn allocs_here(&self) -> u64 {
        LOCAL_COUNT.with(Cell::get)
    }
}

impl Drop for AudioSection {
    fn drop(&mut self) {
        let n = LOCAL_COUNT.with(|c| {
            let v = c.get();
            c.set(0);
            v
        });
        if n > 0 {
            TOTAL_ALLOCS.fetch_add(n, Ordering::Relaxed);
        }
        LAST_SECTION_ALLOCS.store(n, Ordering::Relaxed);
        // Always exit towards IDLE: if we were unwinding from a panic (S_REPORTING)
        // the thread is no longer in the callback, and the nesting (`was_nested`) is
        // a programming condition not to be propagated.
        let _ = self.was_nested;
        STATE.with(|s| s.set(S_IDLE));
    }
}

/// Usage errors of the section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSectionError {
    /// `enter()` twice on the same thread: the second one masks the first.
    AlreadyInside,
}

/// Allocator that enforces the contract. To be installed in the **binary**:
///
/// ```ignore
/// #[global_allocator]
/// static ALLOC: hathor_dsp_core::CheckedAllocator = hathor_dsp_core::CheckedAllocator::new();
/// ```
pub struct CheckedAllocator<A = std::alloc::System> {
    inner: A,
}

impl CheckedAllocator {
    /// Creates the allocator that delegates to `std::alloc::System`.
    pub const fn new() -> Self {
        Self {
            inner: std::alloc::System,
        }
    }
}

impl Default for CheckedAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl<A> CheckedAllocator<A> {
    /// Wraps an existing allocator.
    pub const fn with_inner(inner: A) -> Self {
        Self { inner }
    }
}

// `impl<A>` and not `impl CheckedAllocator`: the callers are inside
// `impl<A: GlobalAlloc> GlobalAlloc for CheckedAllocator<A>`, and in Rust a generic impl
// block does NOT see the methods defined in `impl CheckedAllocator<System>`. Classic
// mistake of people coming from Java's nominal generics.
impl<A> CheckedAllocator<A> {
    fn report(layout: &Layout) {
        LOCAL_COUNT.with(|c| c.set(c.get() + 1));
        if !PANIC_MODE.load(Ordering::Relaxed) {
            return;
        }
        // Mark BEFORE panicking: the panic's payload allocates, and with
        // STATE=REPORTING those allocations are legitimate. Without this swap the
        // guard would restart itself until stack overflow.
        let prev = STATE.with(|s| {
            let p = s.get();
            s.set(S_REPORTING);
            p
        });
        if prev == S_REPORTING {
            // We are already dying from an allocation: stop the recursion.
            return;
        }
        panic!(
            "allocation of {} bytes (align {}) on the audio thread \
             — see hathor_dsp_core::alloc_guard",
            layout.size(),
            layout.align()
        );
    }
}
unsafe impl<A: GlobalAlloc> GlobalAlloc for CheckedAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if STATE.with(Cell::get) != S_IDLE {
            Self::report(&layout);
        }
        unsafe { self.inner.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if STATE.with(Cell::get) != S_IDLE {
            Self::report(&layout);
        }
        unsafe { self.inner.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if STATE.with(Cell::get) != S_IDLE {
            Self::report(&layout);
        }
        unsafe { self.inner.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // The free is not a realtime problem (system allocators do it without taking
        // the big lock in the majority of cases) but counting it stays useful: a free
        // in the section = there was an allocation you forgot about.
        unsafe { self.inner.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // We install the guard in the test binary: the tests below verify the real
    // behaviour, not a fiction.
    #[global_allocator]
    static TEST_ALLOC: CheckedAllocator = CheckedAllocator::new();

    // RULE: every test that touches PROCESS-GLOBAL state (PANIC_MODE,
    // LAST_SECTION_ALLOCS, the counters) takes this lock. cargo test runs the tests on
    // parallel threads: the test that panics publishes n=1 in its Drop, and the Drop
    // of another section in flight overwrites it with 0 → intermittent false failures
    // (really seen, on the first run). `into_inner` on the poison: if a test panics
    // while holding the lock, the global state was already cleaned up by the
    // Drop/reset, so we carry on — we do not propagate a harmless failure.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn try_alloc_in_section() -> u64 {
        // catch_unwind: the panic happens inside the allocator, so it must be able to
        // allocate the payload without recursion.
        // Do NOT "fix" the vec_init_then_push lint with vec![..]: Box allocations
        // (exchange_malloc) are nounwind on failure — the guard's panic inside them
        // escapes catch_unwind and kills the thread (proved by
        // examples/guard_probe.rs: `push` catches, `vec![]` aborts).
        #[allow(clippy::vec_init_then_push)]
        let r = std::panic::catch_unwind(|| {
            let g = AudioSection::enter().expect("enter");
            let mut v: Vec<u8> = Vec::new();
            v.push(1); // grows through __rust_alloc: panics inside, unwind-safe
            std::hint::black_box(&v); // the allocation IS the subject under test
            let n = g.allocs_here();
            drop(v);
            n
        });
        match r {
            Ok(n) => n,
            Err(_) => panic_mode_probe(),
        }
    }

    fn panic_mode_probe() -> u64 {
        // In panic mode the unwind interrupts the section: the partial count is
        // published on the drop of the guard.
        allocs_last_section()
    }

    #[test]
    fn allocs_outside_section_are_not_counted() {
        let _serial = serial();
        reset_counters();
        let v: Vec<u32> = (0..1000).collect();
        assert_eq!(allocs_last_section(), 0);
        assert_eq!(v.len(), 1000);
    }

    #[test]
    fn audio_section_resets_state_after_panic() {
        let _serial = serial();
        PANIC_MODE.store(true, Ordering::Relaxed);
        reset_counters();
        // Unwind-safe alloc form and its reason: see `try_alloc_in_section`.
        #[allow(clippy::vec_init_then_push)]
        let _ = std::panic::catch_unwind(|| {
            let _g = AudioSection::enter().unwrap();
            let mut v: Vec<u8> = Vec::new();
            v.push(7);
            std::hint::black_box(&v);
        });
        // The Drop ran during unwinding: the thread is no longer marked as audio.
        assert!(!in_audio_section());
        assert!(allocs_last_section() >= 1, "the guard must count it");
    }

    #[test]
    fn measure_mode_counts_without_panic() {
        let _serial = serial();
        PANIC_MODE.store(false, Ordering::Relaxed);
        reset_counters();
        let n = {
            let g = AudioSection::enter().unwrap();
            let v: Vec<u8> = std::hint::black_box(vec![0u8; 10]);
            let n = g.allocs_here();
            drop(v);
            n
        };
        assert!(n >= 1, "expected at least one allocation, got {n}");
        assert!(!in_audio_section());
        PANIC_MODE.store(true, Ordering::Relaxed);
    }

    #[test]
    fn nested_enter_is_rejected() {
        let _serial = serial();
        let g = AudioSection::enter().unwrap();
        assert_eq!(
            AudioSection::enter().err(),
            Some(AudioSectionError::AlreadyInside)
        );
        drop(g);
        assert!(!in_audio_section());
    }

    #[test]
    fn ordinary_allocations_still_work() {
        let s = String::from("works");
        let v: Vec<i32> = (0..50).map(|x| x * x).collect();
        assert_eq!(&s, "works");
        assert_eq!(v[3], 9);
    }

    #[test]
    fn no_false_positive_in_a_section_without_allocations() {
        let _serial = serial();
        reset_counters();
        let mut buf: [f32; 64] = [0.0; 64];
        {
            let g = AudioSection::enter().unwrap();
            // Stack-only arithmetic: zero allocations expected.
            for (i, x) in buf.iter_mut().enumerate() {
                *x = i as f32 * 0.25;
            }
            let n = g.allocs_here();
            assert_eq!(n, 0, "false positive from the guard");
        }
        assert_eq!(buf[4], 1.0);
    }

    #[test]
    fn try_alloc_helper_reports_at_least_one_allocation() {
        let _serial = serial();
        assert!(try_alloc_in_section() >= 1);
    }
}
