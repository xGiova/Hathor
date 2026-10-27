//! Host-side graph: the M0 linear [`Chain`] of [`hathor_dsp_core::Node`]s.
//!
//! This crate puts nodes in sequence and makes them cooperate. In M0 the sequence
//! is deliberately dumb — input → node₁ → node₂ → … → output — to prove the
//! contract end-to-end and to let `hathor-audio-probe` measure a real chain; the
//! graph with edges, sidechain and PDC arrives in M5 (see
//! `docs/modules/hathor-host-graph.md`, whose M6 channel-strip section is future
//! work and intentionally not implemented here).
//!
//! The three laws this crate defends:
//!
//! 1. **[`Chain::process`] is zero-alloc, zero-lock, forever.** Topology changes
//!    (`push`, parameter writes) happen on the control thread, before or between
//!    blocks; the processing loop touches only pre-built state. The unit tests
//!    prove it with [`hathor_dsp_core::AudioSection`] around 10,000 cycles.
//! 2. **Declared latency is the honest sum** of the nodes' `latency_samples()`:
//!    in a series chain latencies accumulate. A node that lies about its latency
//!    breaks PDC downstream, so the sum is recomputed from the nodes on every
//!    read — a cached number could go stale the moment a parameter moves a node's
//!    latency.
//! 3. **Parameter fan-out by [`hathor_dsp_core::ParamId`]**: one write reaches
//!    every node in the chain; each node routes the id internally (recognize or
//!    ignore, per the `Node` contract). The chain never inspects values.
//!
//! The `dyn Node` choice (vs generics) is the documented deviation in STYLE.md §D.2:
//! a user-defined graph is heterogeneous at runtime and the ~5 ns (expected) vtable
//! cost is amortized over the whole block.

#![warn(missing_docs)]
// The "no unwrap" bans apply to production code, not to tests: in a test an
// `.expect()` with a message documents better than a match.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::exit)
)]

mod chain;

pub use chain::Chain;
