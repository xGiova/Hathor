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
//! ## 4. The bank is the FFT, four times, smoothed (M1)
//!
//! [`FftBank`] is the SPAN-style analyzer core: the doc's normative table — 256/1024/
//! 4096/16384-sample tiers, each refreshing at N/4 — each with its ring, precomputed
//! Hann, and `RealFftPlan`, their squared magnitudes spread over one **shared log
//! axis** (default 1024 bins, 20 Hz–24 kHz) whose mapping weights are decided at
//! setup. A bin's level is the max over the tiers — the mechanism that keeps the
//! junction between tiers continuous in dB, tested against known sines at
//! 100/500/4k/24k. `process` stays transcendental-free: the dB conversion the display
//! wants is `render_db`, a display-thread call, not a callback one.
//!
//! ## 5. The masking meter reads the bank twice (M2)
//!
//! [`MaskingMeter`] takes two level spectra off that shared axis (signal A and B)
//! and computes what a first-class hearing approximation says is *buried*: masked
//! thresholds with asymmetric exponential skirts and power-add, five read-only dB
//! views including the SPAN-style A↔B difference. The model is chosen and
//! documented in `mask.rs` — an approximation, not a psychoacoustic standard; its
//! acceptance is agreement with listening in M3. The math layer is fully testable
//! today: it is pinned against an independent f64 re-derivation of its own doc
//! formula, and its hot loop keeps the bank's law (setup allocates the skirt
//! tables, `update` is pure indexed add/mul).
//!
//! ## What is NOT here (deliberately)
//!
//! No inverse real-FFT (M4 writes it with its first consumer — unconsumed code is
//! untested code), no radix-4/split-radix (deferred to after the M0 benchmark), no
//! SIMD (the probe measures the bank's real cost first), no `Node` wrapper (the M1
//! plugin wiring owns that; the bank's `process` is built to slot into it), and no
//! psychoacoustic standard in the meter (the M2 model is an approximation on
//! purpose; M3's listening pass decides).

#![warn(missing_docs)]
// The "no unwrap" bans apply to production code, not to tests: in a test an
// `.expect()` with a message documents better than a match (same carve-out as core).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod bank;
pub mod complex;
pub mod mask;
pub mod plan;
pub mod real;

#[cfg(test)]
mod testsupport;

pub use bank::{
    BankParams, FftBank, TierTuning, AXIS_F_HI, AXIS_F_LO, DEFAULT_BINS, TIER_BANDS_HZ, TIER_SIZES,
};
pub use complex::Complex;
pub use mask::{MaskingMeter, MaskingParams};
pub use plan::{bin_freq, freq_bin, FftPlan};
pub use real::RealFftPlan;
