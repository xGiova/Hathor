//! The FFT, written by us — the heart the analyzer (M1), the null test (M3) and the
//! convolution reverb (M4) will beat with.
//!
//! The project law this crate exists to serve is the allocation one: every table is
//! sized at setup and **nothing in `process` allocates, locks, or calls `sin`**.
//! Three structural decisions carry that law, and each has its own test file or
//! section:
//!
//! ## 1. A plan is a bundle of tables, not a bundle of code paths
//!
//! [`FftPlan::new`] precomputes the twiddles (`e^{-2πik/n}`) and the bit-reversal
//! permutation — in f64, narrowed once — so [`FftPlan::process`] is pure indexed
//! add/mul: bounded cycles, no transcendentals, no allocation. `tests/alloc_contract.rs`
//! makes the claim *mechanical*: it runs the kernels inside the core's
//! `AudioSection`, where a single allocation aborts the section — same guard as every
//! other module, zero exceptions taken.
//!
//! ## 2. One kernel, two directions
//!
//! The inverse is `conj → forward → conj → 1/n` (the conjugation trick, as specified
//! in [docs/modules/hathor-dsp-fft.md](../../../docs/modules/hathor-dsp-fft.md)).
//! A second inverse kernel would be a second implementation to go *identically
//! wrong* — the `−a2·out` lesson from the filters crate, applied by construction
//! instead of re-learned.
//!
//! ## 3. A real signal is half a complex one
//!
//! [`RealFftPlan`] packs `N` real frames into `N/2` complex samples, runs the
//! half-size kernel, and unpacks through hermitian symmetry. The DC and Nyquist bins
//! are computed by hand from their closed forms: that pair is where the classic
//! "almost right" unpacking dies, and the packed-vs-full-complex test is the
//! specification that pins it.
//!
//! ## Verification posture
//!
//! Every numerical claim is checked against an *independent* reference: rustfft
//! (a dev-dependency — never linked into the product), analytic closed forms
//! (impulse, Parseval, the Hann three-line spectrum), or both. `matches_rustfft`
//! tolerances are the spec's 1e-4; where f32 accumulation makes a tolerance a
//! judgement, the test says what it measured.
//!
//! ## What is NOT here (deliberately)
//!
//! No inverse real-FFT (M4 writes it with its first consumer — unconsumed code is
//! untested code), no radix-4/split-radix (deferred to after the M0 benchmark), no
//! SIMD (the probe measures the bank's real cost first), no multi-rate bank (M1).

#![warn(missing_docs)]
// The "no unwrap" bans apply to production code, not to tests: in a test an
// `.expect()` with a message documents better than a match (same carve-out as core).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod complex;
pub mod plan;
pub mod real;

#[cfg(test)]
mod testsupport;

pub use complex::Complex;
pub use plan::{bin_freq, freq_bin, FftPlan};
pub use real::RealFftPlan;
