//! Second-order filters.
//!
//! Two truths that this crate puts into code rather than into comments:
//!
//! ## 1. A biquad does NOT vectorize along frames
//!
//! `Direct Form II Transposed` has a recursive dependency:
//! `out = b0·x + z1;  z1 = b1·x − a1·out + z2;  z2 = b2·x − a2·out`. The output of
//! frame `k` feeds the state of frame `k+1`. The dependency chain is **one sample
//! long per frame**, so an 8-lane SIMD unit has nothing to put in the lanes when it
//! processes a single channel. Whoever "vectorizes a biquad" is really doing one
//! of these two things:
//!
//! * **lane = different channels** ([`biquad::BiquadLanes8`]): 8 independent states in
//!   8 lanes, same recursion. Works perfectly, but demands an *interleaved by
//!   channel* layout, and the transpose has a cost that is **measured** here, not
//!   ignored.
//! * **lane = different sections** (4 bands of the same channel in 4 lanes): useful in
//!   the multiband and in the per-band phase-rotator of the kick/bass plugin.
//!
//! ## 2. You don't smooth the parameter, you smooth the *coefficient*
//!
//! Moving the cutoff of a resonant in one block (64 frames at 48 kHz = 1.33 ms) can
//! cause zipper noise. The solution is not "I ramp the cutoff value" (that would mean
//! recomputing `sin/cos/sqrt` per frame: ~50 ns expected × 64 = ~3.2 µs per filter per
//! block — small alone, but it multiplies across bands × channels and drags
//! transcendental calls into the callback, breaking the bounded-cycle guarantee), but
//! **ramping the 5 coefficients linearly** from the old set to the new one: 5 `fma`
//! per frame, and the filter never sees a step. See [`biquad::Biquad::ramp_to`].

#![warn(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod biquad;
pub mod coeffs;

pub use biquad::{Biquad, BiquadNode};
pub use coeffs::{Coeffs, FilterError, FilterKind};
