//! Hathor EQ: an 8-band parametric EQ as a CLAP plugin, headless v0 (no GUI —
//! the design-before-code rule stands; the plugin is legitimate and loadable
//! without one, exactly like the analyzer).
//!
//! ## What this crate is
//!
//! The second consumer of [`hathor_clap`](hathor_clap) and the first consumer
//! of [`hathor_dsp_filters`](hathor_dsp_filters) through a host face. 34
//! params (bypass + master gain + 8 bands x {type, freq, gain, Q}) over true
//! stereo — one filter STATE per channel, never a fake-stereo filter (the
//! filters crate says why; this crate obeys).
//!
//! * **Neutral by default.** Every band starts `Off`; the power-on graph is
//!   exact identity (biquads with `IDENTITY` coefficients), so "install and
//!   hear nothing" is a bit-exact, testable property, not a vibe.
//! * **Band types** are the RBJ menu: Off / LowPass / HighPass / Peak /
//!   Notch / LowShelf / HighShelf. Shelves use slope `S = 1.0` (maximally
//!   flat) — the Q param is documented-inert for them.
//! * **Coefficient doctrine** (the filters crate law): the host sets a param
//!   → the design (`sin/cos/sqrt`, f64) runs in `set_param_value`, NOT in
//!   `process`; `process` only walks the linear coefficient ramp. No
//!   transcendental is ever paid per sample, no allocation ever appears in
//!   the audio section.
//! * **Decision, labeled:** processing is a flat serial band loop over the
//!   16 [`Biquad`](hathor_dsp_filters::biquad::Biquad) lanes, not a
//!   `hathor-host-graph` `Chain`. The loop is the whole topology (fixed
//!   order, no side-chains) and the host-graph tests already prove the
//!   `Chain` machinery; wiring EQ through it now would add indirection, not
//!   proof. Revisit when a plugin needs reconfigurable routing.
//! * state = the 34 param values (272 bytes, LE); declared latency 0
//!   (minimum-phase biquads).
//!
//! ## Unsafe confinement
//!
//! The plugin logic (`eq.rs`) is pure safe Rust. The only `unsafe` outside of
//! tests is the `clap_entry!` expansion below — wrapper-generated, audited
//! inside `hathor-clap`.
//!
//! ## Honest status
//!
//! Mock-host proxy proves the lifecycle (tests/lifecycle.rs); the binary
//! carries the export (`dumpbin`, lead gate). Real Reaper load is an OWNER
//! step; host-behavior claims are *expected per spec, NOT VERIFIED in real
//! hosts*. The listening test ("does it sound like an EQ") is a human
//! acceptance item, not a CI one.

#![warn(missing_docs)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::exit,
        clippy::todo,
        clippy::panic_in_result_fn
    )
)]

mod eq;

pub use eq::{
    BandState, EqPlugin, ParamSpec, PARAMS_PER_BAND, PARAM_BAND_BASE, PARAM_BYPASS, PARAM_COUNT,
    PARAM_MASTER_GAIN, SPECS, TYPE_NAMES,
};

// One macro call per crate: the wrapper generates the entry module and exports
// `#[no_mangle] pub static clap_entry` by value, wired to `EqPlugin`.
hathor_clap::clap_entry!(entry_gen, EqPlugin);

pub use entry_gen::clap_entry;
