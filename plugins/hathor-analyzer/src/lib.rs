//! Hathor Analyzer: a SPAN-like multiresolution spectrum analyzer as a CLAP
//! plugin, headless v0 (no GUI — the design-before-code rule stands, and a
//! CLAP plugin without a GUI is legitimate and loadable).
//!
//! ## What this crate is
//!
//! The first consumer of [`hathor_clap`](hathor_clap): it implements the safe
//! [`Plugin`](hathor_clap::Plugin) trait over
//! [`FftBank`](hathor_dsp_fft::bank::FftBank) and hands the host the CLAP
//! entry table. Behavior v0 is deliberately small so every claim is testable:
//!
//! * main stereo in → `0.5·(L+R)` mono → bank; audio out is a **bit-exact
//!   pass-through** (the analyzer observes, never alters — gate-tested).
//! * 9 params (floor, 4× average time, 4× decay per tier) mapped onto
//!   [`BankParams`](hathor_dsp_fft::bank::BankParams); changing a param while
//!   active is remembered and takes effect at the next `activate` — a
//!   **decision, labeled**: the bank rebuilds exactly once, in `activate`
//!   (main-thread, allocation-declared), never inside `process`.
//! * state = the 9 params only; smoothing is transient (CONVENTION §8.2).
//! * declared latency 0; no hard realtime requirement.
//!
//! ## Unsafe confinement
//!
//! The plugin logic (`analyzer.rs`) is pure safe Rust: it never touches a raw
//! pointer, never allocates in `process` (scratch buffers are sized in
//! `activate`), and pointer ownership during a callback belongs to the wrapper
//! (see `hathor_clap`'s module headers). The only `unsafe` outside of tests is
//! the `clap_entry!` expansion below — wrapper-generated code, audited inside
//! `hathor-clap` itself.
//!
//! ## The exported symbol
//!
//! `hathor_clap::clap_entry!` (fixed for exactly this in task-5) emits the
//! `#[no_mangle] pub static clap_entry` **by value** — what a host's
//! `dlsym(handle, "clap_entry")` must land on. Nothing is hand-wired in this
//! crate anymore; the re-export below only makes the generated static
//! reachable as `hathor_analyzer::clap_entry` for the tests.
//!
//! ## Honest status
//!
//! The lifecycle proof here is the mock-host proxy (tests/lifecycle.rs); the
//! binary carries stronger evidence (task-4 report: `dumpbin /exports` shows
//! the symbol at ordinal 1). Real loading in Reaper is an OWNER step — the
//! artifact recipe is in the report to the lead. Host-behavior assumptions
//! (buffer lifetimes, call order, the in-place pair promise) are *expected per
//! spec, NOT VERIFIED in real hosts*.

#![warn(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::exit)
)]

mod analyzer;

pub use analyzer::{
    AnalyzerPlugin, ParamSpec, PARAM_AVG_TIERS, PARAM_COUNT, PARAM_DECAY_TIERS, PARAM_FLOOR_DB,
    SPECS,
};

// One macro call per crate: the wrapper generates the entry module, exports
// `#[no_mangle] pub static clap_entry` by value, and wires the one-plugin
// factory to `AnalyzerPlugin::DESCRIPTOR` through audited wrapper code only.
hathor_clap::clap_entry!(entry_gen, AnalyzerPlugin);

pub use entry_gen::clap_entry;
