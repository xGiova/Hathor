# Module: `hathor-host-graph`

> Status: **written, tested 12+1** (M0 linear chain landed and guard-proved; the real graph is
> M5). Dependencies: `hathor-dsp-core`.

## Purpose

Put `Node`s in sequence and make them cooperate: in M0 a **linear chain**
(input → node₁ → node₂ → … → output) that demonstrates the contract end-to-end; in M5 the
**graph** with sidechain, sends and PDC. The M0 chain is deliberately dumb: it exists to run
`hathor-audio-probe` on a real chain (biquad + saturator + meter) and to measure the per-node cost,
not to anticipate the mixer.

## How it works

### M0 — `Chain`

```rust
pub struct Chain {
    nodes: Vec<Slot>,          // Slot { node: Box<dyn Node>, prepared: bool }; order = insertion order
    setup_ctx: ProcessCtx,     // synthesized by Chain::new (see below) — nodes get setup through it
    hints: GraphHints,         // max_channels × max_frames: the declared shape every block must fit
    scratch: ChannelBuffer,    // exists from M0; process never touches it until M5 needs rotation
}
impl Chain {
    pub fn new(sample_rate: u32, channels: usize, max_frames: usize) -> Self;
    pub fn push(&mut self, node: Box<dyn Node>) -> Result<(), NodeError>;  // calls setup immediately
    pub fn process(&mut self, ctx: &ProcessCtx, io: &mut Block<'_>) -> Result<(), NodeError>;
    pub fn latency_samples(&self) -> usize;      // Σ over nodes, RECOMPUTED per read
    pub fn max_latency_samples(&self) -> usize;  // same law on the parameter-driven maxima
}
```

* **No cached latency field — LEAD DECISION, not drift.** The original sketch kept a
  `latency_samples: usize` on the struct, updated at `push`. The implemented getters recompute
  the sum on every read: the moment a node's latency becomes parameter-driven (linear-phase
  mode, look-ahead length), an incrementally cached sum is *stale data wearing a trustworthy
  face*, and a wrong sum silently mis-compensates every downstream path. Invariant 3's "the
  sum, always" applies to the value's truth, not just its formula; the getters are O(nodes)
  on chains of ≤ tens of nodes, read at UI/setup rate — a cost that does not exist next to the
  risk it deletes.

* **In place in M0, scratch door established**: every node transforms `io` in place; nodes that
  will need input≠output use `Chain::with_scratch(|block| …)` — the closure pattern M5
  generalizes into `with(id, |blk| …)`. The scratch buffer is allocated once in `new` and the
  door never allocates (the test `the_scratch_door_is_shaped_and_never_allocates` proves it
  under the guard). In-place would be enough today; the pattern is *established*, not
  anticipated, so the benchmark measures the real configuration — actual rotation of the two
  buffers arrives with M5.
* `push` calls `setup(&self.setup_ctx, &self.hints)` immediately — the two values are
  **synthesized by `Chain::new(sample_rate, channels, max_frames)`**: `ProcessCtx::bare(sample_rate,
  max_frames)` (so a node sizes its history for the *worst* block from day one) and `GraphHints
  { max_channels: channels, max_frames }`. Whoever adds a node while the stream runs does it
  from the control thread. If `setup` fails the node is **not added** (it drops on the way out:
  a half-prepared node is exactly what `process` refuses — `prepared: bool` per slot).
* **Latency**: `Σ node.latency_samples()`. In a series chain latencies add up; in the M5 graph
  the sum is *per path* and the compensation aligns the paths (architecture §4). The method
  exists from M0 because `hathor-audio-probe` prints it: seeing "chain: 0 samples" today and "chain:
  64 samples" when you add a linear-phase EQ is the PDC lesson.

### M5 — the graph (what changes, written now so M0 is not wrong)

* `NodeSlot { id, kind, latency_samples, io: [BufferId; N] }` in an index arena (architecture
  §4): no pointers, topology modifiable from the UI without invalidating anything, buffer
  access through `with(id, |blk| ...)` (closures, [docs/it/rust.md](../it/rust.md) §3).
* Sidechain = an **explicit edge** with a role (`key input`), not a "magic second input": the
  node declares `fn key_inputs() -> &[BufferId]`.
* Dry/wet and sends: merge nodes with coefficients, same edges.
* Modification with the stream running: a command queue (rtrb) towards the audio thread, which
  applies it between blocks. Never mutate the topology *during* `process`.

### M6 — the channel strip: owner of `G_cal` and the variance vector (planned)

The strip is a `Chain` plus host policy — the policy is specified in
[hathor-calibration-variance.md](hathor-calibration-variance.md); what *this* module owns:

* **One `GainStagingNode` at chain input**, applying the strip's `G_cal` multiply
  (calibration reference × user trim × drift bias, coefficient computed at UI/setup rate,
  ramped with `SmoothedParam` at frame rate). `latency_samples() = 0` and it tells the truth:
  the multiply is memoryless.
* **The per-channel variance vector** (`ChannelDrift`, hashed from `channel_id` + the
  project's console seed at track creation, persisted with the track) is *host-side data*:
  the strip applies it to the **parameters** it feeds downstream (`f0 × (1 + eq_freq_drift)`
  before any `design()`) and to `G_cal`. It never enters `hathor-dsp-filters` or any L2
  crate — L2 does not know what a channel is (architecture §1).
* **Latency compensation ownership, restated for level-sensitive inserts**: a node with
  look-ahead (auto-makeup, the M4 limiter class, any future NN variant that buffers ahead)
  *declares* `latency_samples`/`max_latency_samples`; the **M5 engine compensates** along
  the path (architecture §4), the M5 arena pre-allocates compensation lines at
  `max_latency_samples`. The strip never shifts time itself. This split is the precondition
  for ever enabling a look-ahead makeup module on a mixer strip.

## Invariants

1. `Chain::process` does not allocate. Test: `AudioSection` around 10,000 cycles with a chain
   of 3 real nodes → counter 0 (it is literally the M0 acceptance test of `hathor-audio-probe`,
   exercised here as well in unit tests).
2. Processing order = `push` order. No implicit reordering in M0 (topological sort arrives in
   M5 with the edges; anticipating it here is complexity without a consumer).
3. The chain's `latency_samples()` = the sum, always, even when lying would be convenient. A new
   node that forgets to declare its latency breaks the PDC *downstream*: the test
   `chain_reports_summed_latency` exists for this.
4. An `Err` from a node stops the chain and propagates: no implicit "skip the node and carry
   on" (the failure policy — mute, bypass, report — belongs to the M5 engine, not to the chain).
5. (M6) The strip's `G_cal` multiplies **exactly once per block, at chain input**: no node in
   the chain applies a second console-scale factor (contract and rationale:
   [hathor-calibration-variance.md](hathor-calibration-variance.md)). A test with the strip
   bypassed nulls dry, which is how the invariant gets enforced rather than asserted.

## Rust concepts to put into practice

`Box<dyn Node>` and the vtable ([docs/it/rust.md](../it/rust.md) §6: the cost is per *block*, not per sample) ·
index arena vs pointers (M5) · the `with(id, f)` closure as the only door to shared buffers ·
`Result` propagated with `?` through the chain.

## DSP concepts it embodies

PDC and declared latency ([docs/it/dsp.md](../it/dsp.md) §5 + architecture §4) · implicit gain staging (the
order of the nodes *is* the sound: EQ before or after the compressor is not the same thing —
the M0 chain makes this experimentable in `hathor-audio-probe` with two configurations) ·
from M6, **explicit** gain staging too: the strip's `G_cal` gives the stream a defined
absolute level for amplitude-sensitive consumers ([hathor-calibration-variance.md](hathor-calibration-variance.md)) · in-place
processing and why the double buffer exists.

## Deferred (and why)

* **Oversized-block handling is a stopgap** → M5: today `process` refuses a block that exceeds
  the declared shape *before any node sees it* — `NodeError::Internal("block exceeds the chain's
  declared shape")`, because nodes allocated their state against `GraphHints` and a bigger block
  must never reach them. An honest wall, not a policy: the re-setup story is the ledger,
  TD-21 (and TD-11 for the channel/sample-rate change question).
* **Dynamic topology, sidechain, sends** → M5: without a host there is nobody driving them; the
  APIs would be guesses. The shape of the arena is already decided (architecture §4), the code
  is not.
* **Parallelism between independent branches** (two graph branches on two cores) → post-M5:
  first measure what one series branch costs on this CPU (the M0 benchmark says it), then decide
  whether it is needed. Anticipating it = synchronization complexity without data.
* **Per-sample automation** → M8 (synth): mixer automation travels at block rate (64 samples)
  and is indistinguishable to the ear for mix parameters; for a synth filter's cutoff it is not,
  and there per-sample `LinearRamp` will be used.

## Exercises (once it exists)

1. Build a chain LP 100 Hz → HP 100 Hz with the same `Coeffs`: is the result ~silence? Measure
   the null residue with `hathor-dsp-measure`. Then invert the order: does anything change? (No — and you
   will understand *why* no is a property of LTI filters, not of the chain.)
2. Put in the chain a node declaring `latency_samples() = 64` and verify that
   `Chain::latency_samples()` reports it. Then imagine: what must the engine do so that the
   recording stays in time? (It is the question that opens architecture §4.)
3. Measure `process` on an empty chain vs 1 node vs 8 nodes: the delta per node is the cost of
   the vtable + the block setup. Compare it with the 1.33 ms budget: how many "empty" nodes would
   fit? (Spoiler: tens of thousands — the real cost is the DSP inside, not the dispatch. That is
   why `dyn Node` per block is the right choice.)
