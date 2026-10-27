//! The M0 `Chain`: nodes in a row, in honest order.

use hathor_dsp_core::{
    Block, ChannelBuffer, GraphHints, Node, NodeError, ParamId, ProcessCtx, Shape,
};

/// Message carried by [`NodeError::Internal`] when the incoming block exceeds the
/// shape the chain was built for: nodes allocated their state against the declared
/// `GraphHints`, so a bigger block must never reach them.
const BLOCK_EXCEEDS_SHAPE: &str = "block exceeds the chain's declared shape";

/// A node in the chain plus the preparation state the spec requires: a node that
/// was not `setup`-ed in this chain is a bug and `process` refuses it. It is not
/// reachable through the public API — [`Chain::push`] sets it — but it is *checked*
/// on every block, so a future refactor cannot silently lose the guarantee.
struct Slot {
    node: Box<dyn Node>,
    prepared: bool,
}

/// A linear chain of nodes: input → node₁ → node₂ → … → output, processed in
/// insertion order with no implicit reordering (topological sort arrives with the
/// M5 edges; anticipating it here is complexity without a consumer).
///
/// Threading contract: build and mutate the topology on the control thread (before
/// the stream starts, or from the engine between blocks); call [`Chain::process`]
/// on the audio thread. `process` allocates nothing, locks nothing, and never
/// touches the topology.
///
/// ```
/// use hathor_host_graph::Chain;
///
/// let chain = Chain::new(48_000, 2, 64);
/// assert!(chain.is_empty());
/// assert_eq!(chain.latency_samples(), 0);
/// ```
pub struct Chain {
    /// Processing order = insertion order. Pushed to only by `push`, on the
    /// control thread.
    nodes: Vec<Slot>,
    /// The context handed to each node at `setup`: `frames` = `max_frames`, so a
    /// node sizes its history for the worst block from day one.
    setup_ctx: ProcessCtx,
    /// The shape every block must fit into. Nodes allocated their lanes against
    /// these hints; a block outside them is a contract violation, not data.
    hints: GraphHints,
    /// The ping-pong double buffer. In M0 every node transforms in place and the
    /// chain processes `io` directly, but the buffer and the [`Chain::with_scratch`]
    /// door exist now — the M5 pattern is "established, not anticipated", and the
    /// benchmark measures the real configuration (module doc §M0).
    scratch: ChannelBuffer,
}

impl Chain {
    /// Creates an empty chain for a stream at `sample_rate` carrying at most
    /// `channels` channels and `max_frames` frames per block.
    ///
    /// Allocations (the node vector's growth, the scratch buffer) happen here, on
    /// the setup thread — never in `process`.
    pub fn new(sample_rate: u32, channels: usize, max_frames: usize) -> Self {
        Self {
            nodes: Vec::new(),
            setup_ctx: ProcessCtx::bare(sample_rate, max_frames),
            hints: GraphHints {
                max_channels: channels,
                max_frames,
            },
            // LEGAL ALLOCATION: setup thread. This is the one and only time the
            // scratch buffer reserves memory.
            scratch: ChannelBuffer::new(channels, max_frames),
        }
    }

    /// Appends `node` and calls its `setup` immediately with the chain's context
    /// and hints: whoever adds a node while the stream runs does it from the
    /// control thread, and the `GraphHints` reach it from the engine.
    ///
    /// If `setup` fails the node is *not* added (and drops on the way out): a
    /// half-prepared node is exactly the thing the chain refuses to process.
    pub fn push(&mut self, mut node: Box<dyn Node>) -> Result<(), NodeError> {
        node.setup(&self.setup_ctx, &self.hints)?;
        self.nodes.push(Slot {
            node,
            prepared: true,
        });
        Ok(())
    }

    /// Processes one cycle: every node, in push order, in place on `io`.
    ///
    /// An `Err` from a node stops the chain and propagates unchanged — no implicit
    /// "skip the node and carry on": the failure policy (mute, bypass, report)
    /// belongs to the M5 engine, not to the chain (invariant 4).
    ///
    /// Errors raised before the first node sees the block:
    /// * [`NodeError::Internal`] if `io` exceeds the chain's declared shape (the
    ///   nodes sized their state against it);
    /// * [`NodeError::BufferTooSmall`] if `ctx.frames` claims more frames than `io`
    ///   actually holds.
    ///
    /// Zero allocations, zero locks, zero syscalls — asserted by the guard tests.
    pub fn process(&mut self, ctx: &ProcessCtx, io: &mut Block<'_>) -> Result<(), NodeError> {
        if io.channels() > self.hints.max_channels || io.frames() > self.hints.max_frames {
            return Err(NodeError::Internal(BLOCK_EXCEEDS_SHAPE));
        }
        if ctx.frames > io.frames() {
            return Err(NodeError::BufferTooSmall {
                have: io.frames(),
                need: ctx.frames,
            });
        }
        for slot in &mut self.nodes {
            if !slot.prepared {
                return Err(NodeError::Internal("node in the chain was never set up"));
            }
            slot.node.process(ctx, io)?;
        }
        Ok(())
    }

    /// The chain's latency in frames: **the sum** of the nodes' declared
    /// latencies, recomputed from the nodes on every call, so a parameter-driven
    /// latency change can never be reported stale. In a series chain latencies
    /// accumulate; the M5 engine will sum *per path* and compensate. Minimum-phase
    /// IIRs honestly declare 0 — a lying node breaks PDC downstream, and
    /// `chain_reports_summed_latency` is what bites it (TD-14).
    pub fn latency_samples(&self) -> usize {
        self.nodes.iter().map(|s| s.node.latency_samples()).sum()
    }

    /// The sum of the nodes' [`Node::max_latency_samples`] — the compensation the
    /// M5 arena must pre-allocate along this path. The same honest-summing law of
    /// [`Chain::latency_samples`], applied to the parameter-driven maximum.
    pub fn max_latency_samples(&self) -> usize {
        self.nodes
            .iter()
            .map(|s| s.node.max_latency_samples())
            .sum()
    }

    /// Parameter fan-out: write `value` under `id` to **every** node in the chain.
    /// Each node routes the id itself (recognize or ignore, per the `Node`
    /// contract, where the write must be idempotent: the UI may send the same
    /// value a hundred times per second). The chain assigns no meaning to ids or
    /// values and never allocates on the way.
    pub fn set_param(&mut self, id: ParamId, value: f32) {
        for slot in &mut self.nodes {
            slot.node.set_param(id, value);
        }
    }

    /// Reads the current value of `id` from the **first** node that knows it
    /// (insertion order). If two nodes share an id, the head of the chain answers;
    /// if none knows it, `None`.
    pub fn param(&self, id: ParamId) -> Option<f32> {
        self.nodes.iter().find_map(|s| s.node.param(id))
    }

    /// Resets the state of every node (filter history, envelopes) without
    /// reallocating. Broadcast, like the parameter fan-out.
    pub fn reset(&mut self) {
        for slot in &mut self.nodes {
            slot.node.reset();
        }
    }

    /// Number of nodes in the chain.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True if the chain is the empty pass-through: still zero-alloc, still honest
    /// about its 0 samples of latency.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Read access to the node at `index` (push order): names for the meters,
    /// read-only parameters, declared latencies. `None` out of bounds.
    pub fn node(&self, index: usize) -> Option<&(dyn Node + 'static)> {
        self.nodes.get(index).map(|slot| &*slot.node)
    }

    /// Mutable access to the node at `index`, for control-thread surgery (a
    /// node-specific API beyond the `ParamId` fan-out, a re-`setup` the chain
    /// itself does not do). **Never** call it while the audio thread may be inside
    /// [`Chain::process`].
    pub fn node_mut(&mut self, index: usize) -> Option<&mut (dyn Node + 'static)> {
        self.nodes.get_mut(index).map(|slot| &mut *slot.node)
    }

    /// The declared capacity every block must fit into (channels × max frames).
    pub fn shape(&self) -> Shape {
        Shape::new(self.hints.max_channels, self.hints.max_frames)
    }

    /// The only door to the ping-pong scratch buffer — the closure pattern M5
    /// generalizes into `with(id, |blk| ...)`. In M0 every node is in place and
    /// `process` never touches the scratch; the door exists so the double-buffer
    /// pattern is *established* now, and taking it never allocates.
    ///
    /// The closure must not return references into the scratch buffer: the block
    /// is re-borrowed per call, not handed over.
    pub fn with_scratch<R>(&mut self, f: impl FnOnce(&mut Block<'_>) -> R) -> R {
        let mut block = self.scratch.block();
        f(&mut block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hathor_dsp_core::alloc_guard::{allocs_last_section, allocs_total, reset_counters};
    use hathor_dsp_core::AudioSection;
    use hathor_dsp_core::CheckedAllocator;
    use hathor_dsp_core::Shape;
    use hathor_dsp_filters::biquad::params as bp;
    use hathor_dsp_filters::{BiquadNode, FilterKind};
    use std::sync::{Mutex, MutexGuard};

    // We install the guard in the test binary: the tests below verify the real
    // behaviour, not a fiction (same pattern as dsp-core's alloc_guard tests).
    #[global_allocator]
    static TEST_ALLOC: CheckedAllocator = CheckedAllocator::new();

    // Every test that touches the process-global guard state takes this lock:
    // `cargo test` is parallel and the counters are not. The `into_inner` on a
    // poison of `Mutex<()>` holds no data (STYLE.md §D.4).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Deterministic RNG: a numerical test that is not reproducible is not a test.
    struct Lcg(u32);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((self.0 >> 8) as f32 / 8_388_608.0) - 1.0
        }
    }

    /// Appends its digit to channel 0, frame 0: the chain [1, 2, 3] turns 0.0 into
    /// 123.0 — an order signature that a commutative operation (a gain product)
    /// could never provide.
    struct Digit(u32);
    impl Node for Digit {
        fn name(&self) -> &str {
            "digit"
        }
        fn process(&mut self, _ctx: &ProcessCtx, block: &mut Block<'_>) -> Result<(), NodeError> {
            let cur = block.channel_ref(0)[0];
            block.channel(0)[0] = cur * 10.0 + self.0 as f32;
            Ok(())
        }
    }

    /// A pass-through node declaring `lat` samples of latency: the TD-14 foil.
    /// All the filter nodes declare 0, so without a nonzero declarer the "sum"
    /// could silently degrade into a max (64 ≠ 76) and no test would notice.
    struct LatencyStub {
        lat: usize,
    }
    impl Node for LatencyStub {
        fn name(&self) -> &str {
            "latency-stub"
        }
        fn process(&mut self, _ctx: &ProcessCtx, _block: &mut Block<'_>) -> Result<(), NodeError> {
            Ok(())
        }
        fn latency_samples(&self) -> usize {
            self.lat
        }
    }

    /// Returns `Err` from every `process`: the node that prevents skip-and-carry-on.
    struct Failing;
    impl Node for Failing {
        fn name(&self) -> &str {
            "failing"
        }
        fn process(&mut self, _ctx: &ProcessCtx, _block: &mut Block<'_>) -> Result<(), NodeError> {
            Err(NodeError::InvalidParam("stub node fails"))
        }
    }

    /// Returns `Err` from `setup`: must never end up in the chain.
    struct BadSetup;
    impl Node for BadSetup {
        fn name(&self) -> &str {
            "bad-setup"
        }
        fn setup(&mut self, _ctx: &ProcessCtx, _hints: &GraphHints) -> Result<(), NodeError> {
            Err(NodeError::InvalidParam("stub refuses setup"))
        }
        fn process(&mut self, _ctx: &ProcessCtx, _block: &mut Block<'_>) -> Result<(), NodeError> {
            Ok(())
        }
    }

    /// Routes exactly one `ParamId`, recognizing nothing else: the fan-out witness.
    struct Recorder {
        id: ParamId,
        value: f32,
    }
    impl Recorder {
        fn new(id: u32, value: f32) -> Self {
            Self {
                id: ParamId::new(id),
                value,
            }
        }
    }
    impl Node for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn process(&mut self, _ctx: &ProcessCtx, _block: &mut Block<'_>) -> Result<(), NodeError> {
            Ok(())
        }
        fn set_param(&mut self, id: ParamId, value: f32) {
            if id == self.id {
                self.value = value;
            }
        }
        fn param(&self, id: ParamId) -> Option<f32> {
            (id == self.id).then_some(self.value)
        }
    }

    /// `process` raises its flag, `reset` drops it, and the flag is readable as
    /// the node's only parameter: how the tests observe "which nodes ran" and
    /// "which nodes were reset" without downcasting `dyn Node`.
    struct DirtyFlag {
        id: ParamId,
        dirty: bool,
    }
    impl DirtyFlag {
        fn new(id: u32) -> Self {
            Self {
                id: ParamId::new(id),
                dirty: false,
            }
        }
    }
    impl Node for DirtyFlag {
        fn name(&self) -> &str {
            "dirty-flag"
        }
        fn process(&mut self, _ctx: &ProcessCtx, _block: &mut Block<'_>) -> Result<(), NodeError> {
            self.dirty = true;
            Ok(())
        }
        fn param(&self, id: ParamId) -> Option<f32> {
            (id == self.id).then_some(if self.dirty { 1.0 } else { 0.0 })
        }
        fn reset(&mut self) {
            self.dirty = false;
        }
    }

    fn stereo64() -> Chain {
        Chain::new(48_000, 2, 64)
    }

    fn ctx64() -> ProcessCtx {
        ProcessCtx::bare(48_000, 64)
    }

    fn lp(freq: f32) -> BiquadNode {
        BiquadNode::new(FilterKind::LowPass { q: 0.707 }, freq, 0.707, 0.0)
    }

    fn hp(freq: f32, q: f32) -> BiquadNode {
        BiquadNode::new(FilterKind::HighPass { q }, freq, q, 0.0)
    }

    #[test]
    fn chain_processes_nodes_in_push_order() {
        let mut chain = stereo64();
        chain.push(Box::new(Digit(1))).expect("push 1");
        chain.push(Box::new(Digit(2))).expect("push 2");
        chain.push(Box::new(Digit(3))).expect("push 3");
        assert_eq!(chain.len(), 3);

        let mut buf = ChannelBuffer::new(2, 64);
        {
            let mut io = buf.block();
            chain.process(&ctx64(), &mut io).expect("process");
        }
        assert_eq!(
            buf.view().channel(0)[0],
            123.0,
            "processing order must equal push order (invariant 2)"
        );
        // Accessors see the same order; no reordering behind the scenes.
        assert_eq!(
            chain.node(0).map(|n| n.name()),
            Some("digit"),
            "the head of the chain is the first pushed"
        );
        assert!(chain.node(3).is_none(), "no node beyond the last pushed");
    }

    #[test]
    fn empty_chain_is_bitwise_passthrough() {
        let mut chain = stereo64();
        let mut buf = ChannelBuffer::new(2, 64);
        {
            let mut io = buf.block();
            for (c, ch) in io.channels_mut().enumerate() {
                for (i, s) in ch.iter_mut().enumerate() {
                    *s = c as f32 * 1000.0 + i as f32;
                }
            }
        }
        let want0: Vec<f32> = buf.view().channel(0).to_vec();
        let want1: Vec<f32> = buf.view().channel(1).to_vec();
        {
            let mut io = buf.block();
            chain.process(&ctx64(), &mut io).expect("empty process");
        }
        assert_eq!(
            buf.view().channel(0),
            &want0[..],
            "an empty chain must not touch a single sample"
        );
        assert_eq!(buf.view().channel(1), &want1[..]);
        assert_eq!(chain.latency_samples(), 0);
        assert_eq!(chain.max_latency_samples(), 0);
    }

    #[test]
    fn chain_reports_summed_latency() {
        // The TD-14 law, bitten: real filter nodes (minimum-phase IIRs, all
        // declaring an honest 0) alternating with stubs declaring 64 and 12. If
        // someone "optimizes" the sum into a max, 64 ≠ 76 and this fails.
        let mut chain = stereo64();
        chain.push(Box::new(lp(1_000.0))).expect("lp");
        chain
            .push(Box::new(LatencyStub { lat: 64 }))
            .expect("stub 64");
        chain.push(Box::new(hp(100.0, 1.0))).expect("hp");
        chain
            .push(Box::new(LatencyStub { lat: 12 }))
            .expect("stub 12");

        assert_eq!(
            chain.node(0).expect("node 0").latency_samples(),
            0,
            "the biquads must declare 0: minimum-phase IIRs have no integer latency"
        );
        assert_eq!(
            chain.latency_samples(),
            76,
            "a series chain SUMS latencies: 0 + 64 + 0 + 12, always"
        );
        assert_eq!(
            chain.max_latency_samples(),
            76,
            "the max sum follows the same law (M5 pre-allocation)"
        );
    }

    #[test]
    fn node_error_stops_the_chain_without_skipping() {
        // Invariant 4: an Err propagates unchanged and the nodes after the failing
        // one do not run — "skip and carry on" is the M5 engine's policy, not ours.
        let mut chain = stereo64();
        chain.push(Box::new(Digit(1))).expect("push digit");
        chain.push(Box::new(Failing)).expect("push failing");
        chain.push(Box::new(Digit(3))).expect("push after failing");

        let mut buf = ChannelBuffer::new(2, 64);
        {
            let mut io = buf.block();
            assert_eq!(
                chain.process(&ctx64(), &mut io).err(),
                Some(NodeError::InvalidParam("stub node fails")),
                "the error must propagate unchanged"
            );
        }
        assert_eq!(
            buf.view().channel(0)[0],
            1.0,
            "the node after the failing one must not have processed"
        );
    }

    #[test]
    fn push_refuses_a_node_whose_setup_fails() {
        let mut chain = stereo64();
        assert_eq!(
            chain.push(Box::new(BadSetup)).err(),
            Some(NodeError::InvalidParam("stub refuses setup")),
            "the setup error must reach the caller"
        );
        assert!(
            chain.is_empty(),
            "a refused node must not stay in the chain"
        );
        assert_eq!(chain.latency_samples(), 0, "…nor its latency");

        // The chain is still healthy for the nodes that do set up.
        chain
            .push(Box::new(Digit(7)))
            .expect("a good node still pushes");
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn process_refuses_a_block_larger_than_the_declared_shape() {
        let mut chain = Chain::new(48_000, 2, 16);
        chain.push(Box::new(Digit(5))).expect("push");
        let ctx16 = ProcessCtx::bare(48_000, 16);

        // Too many channels: nodes sized their lanes for 2, they must not see 4.
        let mut wide = ChannelBuffer::new(4, 16);
        {
            let mut io = wide.block();
            assert_eq!(
                chain.process(&ctx16, &mut io).err(),
                Some(NodeError::Internal(BLOCK_EXCEEDS_SHAPE)),
                "a block outside the hints is refused before any node sees it"
            );
        }
        assert!(
            wide.view().channel(0).iter().all(|&s| s == 0.0),
            "the refused block must not reach the nodes"
        );

        // Too many frames: same law.
        let mut tall = ChannelBuffer::new(2, 32);
        {
            let mut io = tall.block();
            assert_eq!(
                chain.process(&ctx16, &mut io).err(),
                Some(NodeError::Internal(BLOCK_EXCEEDS_SHAPE)),
                "frames beyond max_frames are refused too"
            );
        }

        // ctx claims more frames than the block holds: BufferTooSmall.
        let mut small = ChannelBuffer::new(2, 16);
        {
            let mut io = small.block();
            assert_eq!(
                chain.process(&ProcessCtx::bare(48_000, 32), &mut io).err(),
                Some(NodeError::BufferTooSmall { have: 16, need: 32 }),
                "the ctx cannot lie about the block being bigger than it is"
            );
        }
        assert!(
            small.view().channel(0).iter().all(|&s| s == 0.0),
            "the refused block must not reach the nodes"
        );

        // The honest combination works.
        {
            let mut io = small.block();
            chain.process(&ctx16, &mut io).expect("in-shape process");
        }
        assert_eq!(small.view().channel(0)[0], 5.0, "and now the node ran");
    }

    #[test]
    fn set_param_fans_out_to_every_node_that_routes_the_id() {
        // One ParamId, multiple sinks: Q is the same id in both biquads, and the
        // chain must deliver it to both. Ids nobody routes change nothing.
        let mut chain = stereo64();
        chain.push(Box::new(lp(1_000.0))).expect("lp");
        chain.push(Box::new(hp(500.0, 1.0))).expect("hp");
        chain
            .push(Box::new(Recorder::new(7, 0.0)))
            .expect("recorder id 7");

        chain.set_param(bp::Q, 2.5);
        assert_eq!(
            chain.node(0).and_then(|n| n.param(bp::Q)),
            Some(2.5),
            "fan-out: the first biquad must see the shared id"
        );
        assert_eq!(
            chain.node(1).and_then(|n| n.param(bp::Q)),
            Some(2.5),
            "fan-out: so must the second — both route Q = 1"
        );
        assert_eq!(
            chain.node(2).and_then(|n| n.param(bp::Q)),
            None,
            "the recorder ignores ids it does not own"
        );

        chain.set_param(ParamId::new(7), -3.25);
        assert_eq!(
            chain.node(2).and_then(|n| n.param(ParamId::new(7))),
            Some(-3.25),
            "a chain-private id reaches its one owner"
        );
        assert_eq!(
            chain.node(0).and_then(|n| n.param(ParamId::new(7))),
            None,
            "and the biquads' own params must be untouched"
        );
        assert_eq!(
            chain.node(0).and_then(|n| n.param(bp::FREQ)),
            Some(1_000.0),
            "the cutoff the foreign id must not have moved"
        );

        // Idempotence: re-sending the same value is legal and lands the same.
        chain.set_param(bp::Q, 2.5);
        assert_eq!(chain.param(bp::Q), Some(2.5), "re-send must not skew");
    }

    #[test]
    fn param_read_reports_the_first_node_that_knows_the_id() {
        let mut chain = stereo64();
        chain
            .push(Box::new(Recorder::new(5, 1.0)))
            .expect("recorder head");
        chain
            .push(Box::new(Recorder::new(5, 2.0)))
            .expect("recorder tail");
        assert_eq!(
            chain.param(ParamId::new(5)),
            Some(1.0),
            "with a shared id the head of the chain answers"
        );
        assert_eq!(
            chain.param(ParamId::new(9)),
            None,
            "an id nobody routes reads None"
        );
    }

    #[test]
    fn chain_reset_reaches_every_node() {
        let mut chain = stereo64();
        chain.push(Box::new(DirtyFlag::new(11))).expect("flag 11");
        chain.push(Box::new(DirtyFlag::new(12))).expect("flag 12");

        let mut buf = ChannelBuffer::new(2, 64);
        {
            let mut io = buf.block();
            chain.process(&ctx64(), &mut io).expect("process");
        }
        assert_eq!(chain.param(ParamId::new(11)), Some(1.0), "node 1 ran");
        assert_eq!(chain.param(ParamId::new(12)), Some(1.0), "node 2 ran");

        chain.reset();
        assert_eq!(
            chain.param(ParamId::new(11)),
            Some(0.0),
            "reset must reach the first node"
        );
        assert_eq!(
            chain.param(ParamId::new(12)),
            Some(0.0),
            "…and the second: it is a broadcast, not a poke at the head"
        );
    }

    #[test]
    fn three_real_nodes_never_allocate_in_ten_thousand_cycles() {
        // The M0 acceptance test of hathor-audio-probe, exercised here as well
        // (invariant 1): chain of 3 real nodes, 10,000 cycles, guard counter 0.
        // If `process` ever allocates, the guard's panic fails this test in the
        // strict mode installed above — the failure is mechanical, not moral.
        let _serial = serial();
        let mut chain = stereo64();
        chain
            .push(Box::new(BiquadNode::new(
                FilterKind::LowPass { q: 0.707 },
                1_000.0,
                0.707,
                0.0,
            )))
            .expect("lp");
        chain
            .push(Box::new(BiquadNode::new(
                FilterKind::Peak {
                    q: 1.0,
                    gain_db: 6.0,
                },
                2_000.0,
                1.0,
                6.0,
            )))
            .expect("peak");
        chain.push(Box::new(hp(100.0, 0.707))).expect("hp");

        let mut buf = ChannelBuffer::new(2, 64);
        {
            let mut g = Lcg(0x5EED);
            let mut io = buf.block();
            for ch in io.channels_mut() {
                for s in ch.iter_mut() {
                    *s = g.next_f32();
                }
            }
        }

        reset_counters();
        let ctx = ctx64();
        let n = {
            let mut io = buf.block();
            let section = AudioSection::enter().expect("enter");
            std::hint::black_box(&mut io);
            for _ in 0..10_000 {
                chain.process(&ctx, &mut io).expect("process");
            }
            section.allocs_here()
        };
        assert_eq!(n, 0, "Chain::process must be zero-alloc (realtime rule 1)");
        assert_eq!(
            allocs_last_section(),
            0,
            "the published section count must agree with the TLS one"
        );
        assert_eq!(allocs_total(), 0, "no other thread was in a section");
        std::hint::black_box(&buf);
    }

    #[test]
    fn coefficient_automation_between_blocks_is_zero_alloc() {
        // The parameter path, bitten the same way: set_param per cycle (block-rate
        // automation, CONVENTION §5) plus the dirty-recompile branch inside
        // `process` — `design()` is pure math, so the whole loop must still be
        // allocation-free under the guard.
        let _serial = serial();
        let mut chain = stereo64();
        chain.push(Box::new(lp(1_000.0))).expect("lp");
        chain.push(Box::new(hp(100.0, 0.707))).expect("hp");

        let mut buf = ChannelBuffer::new(2, 64);
        {
            let mut g = Lcg(0xC0FFEE);
            let mut io = buf.block();
            for ch in io.channels_mut() {
                for s in ch.iter_mut() {
                    *s = g.next_f32();
                }
            }
        }

        reset_counters();
        let ctx = ctx64();
        let n = {
            let mut io = buf.block();
            let section = AudioSection::enter().expect("enter");
            for i in 0..10_000u32 {
                chain.set_param(bp::FREQ, 1_000.0 + (i % 2) as f32 * 500.0);
                chain.process(&ctx, &mut io).expect("process");
            }
            section.allocs_here()
        };
        assert_eq!(n, 0, "automation inside a chain must not allocate either");
        std::hint::black_box(&buf);
    }

    #[test]
    fn the_scratch_door_is_shaped_and_never_allocates() {
        // The double-buffer pattern is *established* now: the door opens on the
        // declared shape and opening it costs nothing, under the guard.
        let _serial = serial();
        let mut chain = stereo64();
        chain.push(Box::new(Digit(1))).expect("digit");
        assert_eq!(chain.shape(), Shape::new(2, 64), "declared capacity");
        let scratch_shape = chain.with_scratch(|s| s.shape());
        assert_eq!(
            scratch_shape,
            Shape::new(2, 64),
            "the scratch buffer carries the chain's shape"
        );

        let mut buf = ChannelBuffer::new(2, 64);
        reset_counters();
        let ctx = ctx64();
        let n = {
            let mut io = buf.block();
            let section = AudioSection::enter().expect("enter");
            for _ in 0..1_000 {
                chain.process(&ctx, &mut io).expect("process");
                chain.with_scratch(|s| {
                    s.zero();
                    std::hint::black_box(s.shape());
                });
            }
            section.allocs_here()
        };
        assert_eq!(n, 0, "taking the scratch door must be allocation-free");
    }
}
