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
//! (see `hathor_clap`'s module headers). The only `unsafe` in this crate is the
//! hand-wired `extern "C"` entry shims in [`clapabi`], which forward to
//! audited wrapper functions — each raw operation sits in an explicit
//! `unsafe { }` with a SAFETY comment (the crate denies
//! `unsafe_op_in_unsafe_fn`).
//!
//! ## Why the entry is hand-wired (wrapper gap, reported to the lead)
//!
//! `hathor_clap::clap_entry!` generates a module holding a `pub static
//! CLAP_ENTRY`. A host resolves the *symbol* `clap_entry` and reinterprets its
//! address as `const clap_plugin_entry` — the struct must exist **by value at
//! that symbol**, and Rust cannot copy one static into another's initializer.
//! So `clapabi` below builds the same table from the wrapper's public audited
//! pieces (`entry::init_impl`, `entry::deinit_impl`, `entry::factory_for`,
//! `plugin::create_instance`) and exports it by value. No wrapper logic is
//! bypassed or duplicated beyond that wiring; the gap and a suggested macro
//! shape were reported to the lead.
//!
//! ## Honest status
//!
//! The lifecycle proof here is the mock-host proxy (tests/lifecycle.rs): real
//! loading in Reaper is an OWNER step — the artifact recipe is in the report.
//! Host-behavior assumptions (buffer lifetimes, call order, the in-place pair
//! promise) are *expected per spec, NOT VERIFIED in real hosts*.

#![warn(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::exit)
)]

mod analyzer;
mod clapabi;

pub use analyzer::{
    AnalyzerPlugin, ParamSpec, PARAM_AVG_TIERS, PARAM_COUNT, PARAM_DECAY_TIERS, PARAM_FLOOR_DB,
    SPECS,
};
pub use clapabi::clap_entry;
