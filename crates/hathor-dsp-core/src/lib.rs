//! # The contract of the audio path
//!
//! This crate does NOT contain DSP: it contains the *rules* that DSP must respect.
//! They are the rules that make Rust useful here instead of decorative.
//!
//! ## Rules (breaking one = a click in a session in four months, not an error today)
//!
//! 1. **No allocating in the callback.** Ever. Not `format!`, `vec!`, `to_string`,
//!    `Box::new`, `HashMap` inserts either. System allocators have global locks: one
//!    300 µs contention at a 64/48k buffer (1.33 ms budget) eats 22% of your cycle.
//!    [`alloc_guard`] makes the rule *mechanical*: [`alloc_guard::CheckedAllocator`]
//!    panics if you allocate inside an [`alloc_guard::AudioSection`].
//! 2. **No locking in the callback.** No `Mutex`/`RwLock`. UI→audio goes through
//!    lock-free channels ([`param`] for continuous values, a ring buffer for events).
//! 3. **No I/O, no syscall, no sleep in the callback.** A page fault or a `ReadFile`
//!    under an audio driver's call chain is a dropout.
//! 4. **No `unwrap`/`panic` on the hot path** except via the allocation guard (which
//!    *wants* to blow up in debug). `NodeError` uses `&'static str`: an error
//!    `String` would be an allocation in an emergency branch.
//! 5. **Every node declares its latency.** [`Node::latency_samples`] exists from day
//!    1 even though all our nodes return 0: retrofitting PDC (Plugin Delay
//!    Compensation) onto a graph that does not know latency is a rewrite, not a
//!    patch.
//! 6. **Planar layout, channels separated** ([`buffer`]). SIMD lanes run along the
//!    frames *of the same channel*. Recursive filters (biquads) do NOT vectorize
//!    along frames: there is a state dependency. They vectorize across *channels*,
//!    and that requires a different layout (see `hathor_dsp_filters::biquad::BiquadLanes8`).
//! 7. **Denormals OFF on the audio thread** ([`denormal`]). An IIR filter sliding
//!    into denormals becomes 10-100× slower in a non-deterministic way: it is the
//!    "project slows down after 20 minutes" bug you will never reproduce.
//!
//! ## What is NOT here (deliberately)
//!
//! No `no_std`: `no_std` is not what makes an audio thread safe, the discipline on
//! rules 1-3 is. No graph/edge routing either: that lives in `host-graph`, which
//! builds the first linear and honest `Chain` on top of this.

#![warn(missing_docs)]
// The "no unwrap" bans apply to production code, not to tests: in a test an
// `.expect()` with a message documents better than a match.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::exit)
)]

pub mod alloc_guard;
pub mod buffer;
pub mod denormal;
pub mod node;
pub mod param;

pub use alloc_guard::{AudioSection, AudioSectionError, CheckedAllocator};
pub use buffer::{Block, ChannelBuffer, Shape};
pub use node::{GraphHints, Node, NodeError, ProcessCtx, Transport};
pub use param::{ParamId, SmoothedParam};

/// Version of the node contract. Future state/serialization formats will use it to
/// reject blobs produced by a different contract.
pub const ABI_CONTRACT_VERSION: u32 = 1;

/// Block budget in microseconds: a 64-frame block at 48 kHz recurs every
/// `64 / 48000 s = 1333.33 µs`. The realtime rule of thumb (M0 gate): the
/// worst-case block processing time must stay well below this (< 40%).
pub const BUDGET_64_48K_US: f64 = 64.0 * 1_000_000.0 / 48_000.0;
