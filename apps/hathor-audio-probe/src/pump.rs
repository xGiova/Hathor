//! The realtime task: tone → biquad chain → analysis on the scratch door → counters.
//!
//! ## What one chunk does, in order
//!
//! 1. fill the planar rows (generated tone, or the deinterleaved driver buffer when
//!    `monitor_input` is set),
//! 2. run the three-biquad [`Chain`] in place on a [`Block`] over those rows,
//! 3. measure the *exact* output levels from that block (before any padding enters),
//! 4. copy the block into the graph's **scratch door** ([`Chain::with_scratch`], the
//!    only door CONVENTION §3.4 allows) and feed the FFT window from it, so the
//!    analysis consumes the chain's output through the pattern M1's analyzer will
//!    use — one [`RealFftPlan`] per block once the window is full,
//! 5. interleave planar → the driver's own buffer,
//! 6. publish cost, levels and Σ|X|² through atomics and the ring (in `render`).
//!
//! ## Why the chunking rule is not negotiable
//!
//! The driver hands us a buffer of *any* size (`frames_available` on WASAPI; the
//! engine period, typically 480 frames at 48 kHz). The G1 gate is about a **64-frame**
//! block, so every callback is pumped as ⌈n/64⌉ chunks, the last one possibly
//! shorter. The chain is declared `max_frames = 64`, so handing it a 480-frame block
//! would be a [`NodeError`] from its shape guard, not a slower measurement: chunking
//! is what makes "measured at 64" true *on top of* "the driver asked for 480". The
//! number of chunks per callback is a reported fact, never inferred from a constant.
//!
//! ## What is timed, and what that word means here
//!
//! One `std::time::Instant` pair **outside-to-outside around a whole chunk**: tone
//! fill, chain, door, transform, copies. That is the honest unit — it is what the
//! driver thread actually spends per block of work — and it is *wallclock*, not CPU
//! cycles: it includes the timer read itself and any preemption inside the window.
//! The report says so; nothing here claims a cycle count.

use std::sync::Arc;
use std::time::Instant;

use hathor_dsp_core::alloc_guard::{self, AudioSection, AudioSectionError};
use hathor_dsp_core::buffer::Block;
use hathor_dsp_core::{denormal, Node, NodeError, ProcessCtx, Shape};
use hathor_dsp_fft::{Complex, RealFftPlan};
use hathor_dsp_filters::{BiquadNode, FilterKind};
use hathor_dsp_measure::{peak_db, rms_db};
use hathor_host_graph::Chain;
use rtrb::Consumer;

use crate::cli::Tone;
use crate::cost_summary::{budget_ns as chunk_budget_ns, warn_ns};
use crate::counters::Counters;
use crate::fft_window::FftWindow;
use crate::interleave::{deinterleave, interleave};
use crate::probe_error::ProbeError;
use crate::timing_ring::{BlockCostRing, DEFAULT_CAPACITY};
use crate::tone::ToneGen;

/// The three biquads the probe runs: listenable, cheap, and stateful.
///
/// Fixed, not random: two magnitude filters with Q and one gain filter with a dB
/// parameter, cascaded. Chosen so the 440 Hz tone actually gets through — a first
/// draft had LP 120 Hz → Peak 3 kHz → HP 4 kHz, and a unit test caught that the low
/// and high passes have *no* overlapping passband there: the probe would have played
/// a −59 dBFS whisper and the level check would have been meaningless. With LP 8 kHz
/// and HP 100 Hz both skirts are far from 445 Hz (a 2nd-order Butterworth is
/// 1/√(1+(f/fc)⁴), so each costs well under 0.01 dB), and the +6 dB peak at 1 kHz
/// lands on the tone's low shoulder. Two runs on different days stay comparable
/// because none of this depends on the device.
pub const CHAIN_SPEC: [(FilterKind, f32, f32, f32); 3] = [
    (FilterKind::LowPass { q: 0.707 }, 8_000.0, 0.707, 0.0),
    (
        FilterKind::Peak {
            q: 1.0,
            gain_db: 6.0,
        },
        1_000.0,
        1.0,
        6.0,
    ),
    (FilterKind::HighPass { q: 0.707 }, 100.0, 0.707, 0.0),
];

/// `Chain` is `!Send`: its nodes are `Box<dyn Node>` and `dyn Node` is neither
/// `Send` nor `Sync`. cpal's callback must be `Send`, so the pump owns the chain
/// through this wrapper and gives the borrow checker a written argument.
pub struct OwnedChain {
    inner: Chain,
}

// SAFETY: a chain crosses a thread boundary **only by ownership**. Every holder of
// an `OwnedChain` owns it outright: cpal's closure owns the pump, takes `&mut` to
// the chain for exactly one chunk, and cpal serialises callbacks on its single
// stream thread, so there is never more than one live borrower and no shared
// reference escapes the callback. The payload is `BiquadNode` — `Copy` f32 state in
// a fixed array, no handles, no thread-locals, no allocator metadata — so there is
// nothing thread-affine to break. The compile-time check below keeps the claim
// re-auditable: if a node type with thread affinity is ever put in this chain, the
// `assert_owned_send::<…>` lines are what fail, not a comment.
unsafe impl Send for OwnedChain {}

const _: () = {
    fn assert_owned_send<T: Send>() {}
    /// The wrapper is only as safe as its payload's moveability.
    fn check() {
        assert_owned_send::<BiquadNode>();
        assert_owned_send::<OwnedChain>();
    }
    let _ = check;
};

impl OwnedChain {
    /// Wraps a fully built chain.
    pub fn new(inner: Chain) -> Self {
        Self { inner }
    }

    /// Shared access, for report fields (`len`, `node(i)`, `shape()`).
    pub fn chain(&self) -> &Chain {
        &self.inner
    }

    /// The only mutable view: taken by the callback for one chunk at a time.
    pub fn chain_mut(&mut self) -> &mut Chain {
        &mut self.inner
    }
}

/// The realtime payload: what cpal's closure owns.
pub struct Pump {
    chain: OwnedChain,
    core: RenderCore,
    counters: Arc<Counters>,
    ring: BlockCostRing,
    ftz_installed: bool,
    rate: u32,
    /// Wallclock budget for one full chunk, and the 0.40× warn threshold.
    budget_ns: u64,
    warn_ns: u64,
    chunk_frames: u32,
}

/// The render half: everything that touches samples, plus the analysis state.
struct RenderCore {
    io: Vec<f32>,
    window: FftWindow,
    fft: RealFftPlan,
    bins: Vec<Complex>,
    tone: ToneGen,
    channels: usize,
    max_frames: usize,
    /// Σ|X|² over every transform. This is the *use* that stops the release
    /// optimiser from deleting the FFT — a probe measuring an elided transform would
    /// be worse than no probe. It also goes non-finite if a bin does, which the
    /// report prints as exactly that.
    energy: f64,
    position: u64,
}

/// What one chunk produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChunkReport {
    /// True when a transform ran (the window was full).
    pub fft_ran: bool,
    /// Peak output level in dBFS over the chunk's exact shape.
    pub peak_db: f32,
    /// RMS output level in dBFS over the chunk's exact shape.
    pub rms_db: f32,
    /// The running Σ|X|² after this chunk.
    pub energy: f64,
}

impl Default for ChunkReport {
    fn default() -> Self {
        Self {
            fft_ran: false,
            peak_db: f32::NEG_INFINITY,
            rms_db: f32::NEG_INFINITY,
            energy: 0.0,
        }
    }
}

impl Pump {
    /// Builds everything that may allocate: the chain (whose nodes `setup()` here),
    /// the graph's scratch buffer, the FFT plan/window/bin store, the sine table and
    /// the ring. Call it on a thread that is **not** inside an [`AudioSection`].
    ///
    /// LEGAL ALLOCATION: setup thread — every `Vec`/`Box` below happens before the
    /// stream exists and none of it is touched again by `render`.
    pub fn setup(
        rate: u32,
        channels: u16,
        chunk_frames: u32,
        fft_size: usize,
        tone: Tone,
    ) -> Result<(Self, Consumer<u64>), ProbeError> {
        Self::setup_with_ring(
            rate,
            channels,
            chunk_frames,
            fft_size,
            tone,
            DEFAULT_CAPACITY,
        )
    }

    /// [`Pump::setup`] with an explicit ring capacity (the tests use small rings).
    ///
    /// LEGAL ALLOCATION: setup thread, as above.
    pub fn setup_with_ring(
        rate: u32,
        channels: u16,
        chunk_frames: u32,
        fft_size: usize,
        tone: Tone,
        ring_capacity: usize,
    ) -> Result<(Self, Consumer<u64>), ProbeError> {
        if channels == 0 {
            return Err(ProbeError::Device(
                "a chain needs at least one channel".to_string(),
            ));
        }
        let chunk = usize::try_from(chunk_frames).unwrap_or(usize::MAX);
        if chunk == 0 {
            return Err(ProbeError::Device(
                "the block shape must be at least one frame".to_string(),
            ));
        }
        let ch = usize::from(channels);
        let mut chain = Chain::new(rate, ch, chunk);
        for (kind, freq, q, gain_db) in CHAIN_SPEC {
            let node = BiquadNode::new(kind, freq, q, gain_db);
            let label = node.name().to_string();
            // LEGAL ALLOCATION: setup thread — one boxed node each.
            chain.push(Box::new(node)).map_err(|e| {
                ProbeError::Stream(format!("node {label} refused setup: {}", node_err(e)))
            })?;
        }
        // LEGAL ALLOCATION: setup thread — the planar working buffer, sized once.
        let io = vec![0.0f32; ch * chunk];
        let window = FftWindow::new(fft_size);
        let fft = RealFftPlan::new(fft_size);
        // LEGAL ALLOCATION: setup thread — the bin store, sized once.
        let bins = vec![Complex::ZERO; fft.bin_count()];
        let core = RenderCore {
            io,
            window,
            fft,
            bins,
            tone: ToneGen::new(tone, rate),
            channels: ch,
            max_frames: chunk,
            energy: 0.0,
            position: 0,
        };
        let counters = Arc::new(Counters::new());
        let (ring, consumer) = BlockCostRing::with_capacity(ring_capacity, Arc::clone(&counters));
        let budget = chunk_budget_ns(chunk_frames, rate);
        Ok((
            Self {
                chain: OwnedChain::new(chain),
                core,
                counters,
                ring,
                ftz_installed: false,
                rate,
                budget_ns: budget,
                warn_ns: warn_ns(budget),
                chunk_frames,
            },
            consumer,
        ))
    }

    /// The counters this pump writes into (shared with the control thread).
    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.counters)
    }

    /// The block shape the pump was built for.
    pub fn chunk_frames(&self) -> u32 {
        self.chunk_frames
    }

    /// Channel count.
    pub fn channels(&self) -> usize {
        self.core.channels
    }

    /// Sample rate the pump processes at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The chain's declared shape, straight from the graph.
    pub fn shape(&self) -> Shape {
        self.chain.chain().shape()
    }

    /// Node names, in push order (control thread only: it formats).
    pub fn chain_names(&self) -> Vec<String> {
        let chain = self.chain.chain();
        (0..chain.len())
            .filter_map(|i| chain.node(i).map(|n| n.name().to_string()))
            .collect()
    }

    /// The chain's declared latency, for the report.
    pub fn chain_latency(&self) -> usize {
        self.chain.chain().latency_samples()
    }

    /// The frequency the probe actually plays, after table quantisation.
    pub fn actual_tone_hz(&self) -> f32 {
        self.core.tone.actual_hz()
    }

    /// The frequency that was asked for.
    pub fn asked_tone_hz(&self) -> f32 {
        self.core.tone.asked_hz()
    }

    /// True when the generator emits digital silence.
    pub fn tone_is_silent(&self) -> bool {
        self.core.tone.is_silent()
    }

    /// The chunk budget in ns, as computed at setup.
    pub fn budget_ns(&self) -> u64 {
        self.budget_ns
    }

    /// The 0.40× warn threshold in ns.
    pub fn warn_ns(&self) -> u64 {
        self.warn_ns
    }

    /// Runs the audio task over `out`, the driver's buffer. The buffer is always
    /// fully written: processed samples, then silence after the first failure.
    ///
    /// `monitor_input` deinterleaves what the driver left in the buffer instead of
    /// generating a tone — the path a "is what I hear what I rendered" check would
    /// use. It costs one extra pass, so the live probe does not ask for it.
    pub fn render(&mut self, out: &mut [f32], monitor_input: bool) -> Result<(), ProbeError> {
        self.ensure_ftz();
        let section = match AudioSection::enter() {
            Ok(s) => s,
            Err(e) => {
                self.counters.note_stream_error();
                fill_silence(out);
                return Err(ProbeError::Stream(section_err(e).to_string()));
            }
        };
        // Disjoint field borrows: this is the whole reason the chain and the render
        // state are separate fields, so `with_scratch` can hold the door while the
        // window and bin store come from elsewhere.
        let Pump {
            chain,
            core,
            counters,
            ring,
            ftz_installed: _,
            rate,
            budget_ns,
            warn_ns,
            chunk_frames: _,
        } = self;
        let ch = core.channels;
        // Whole frames only. A buffer whose length is not a multiple of `ch` has a
        // ragged tail no planar block can represent: it is silenced (see below) and
        // the frames we *did* process are what `note_callback` reports.
        let total_frames = out.len() / ch;
        if total_frames == 0 {
            fill_silence(out);
            counters.note_callback(0, section.allocs_here());
            drop(section);
            return Ok(());
        }
        let mut done = 0usize;
        let mut failure: Option<&'static str> = None;
        while done < total_frames {
            let frames = core.max_frames.min(total_frames - done);
            let off = done * ch;
            let ctx = ProcessCtx {
                sample_rate: *rate,
                frames,
                sample_position: core.position,
                transport: None,
            };
            let t0 = Instant::now();
            let outcome = pump_chunk(
                chain.chain_mut(),
                core,
                &mut out[off..off + frames * ch],
                frames,
                &ctx,
                monitor_input,
            );
            let cost_ns = t0.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            counters.note_chunk(cost_ns, frames as u32, *warn_ns, *budget_ns);
            ring.publish(cost_ns);
            match outcome {
                Ok(rep) => {
                    counters.note_levels(rep.peak_db, rep.rms_db);
                    counters.note_energy(rep.energy);
                    counters.note_fft(rep.fft_ran);
                }
                Err(label) => {
                    failure = Some(label);
                    break;
                }
            }
            done += frames;
        }
        if failure.is_some() {
            fill_silence(&mut out[done * ch..]);
        }
        fill_silence_ragged(out, ch, total_frames);
        // Read the count *before* the section closes: `Drop` clears it, so this is
        // the only way the callback's own number is observable (`allocs_last_section`
        // would read it after the fact — the guard's tests use that, we use both).
        let allocs = section.allocs_here();
        counters.note_callback(total_frames as u32, allocs);
        drop(section);
        match failure {
            None => Ok(()),
            Some(label) => Err(ProbeError::Node(label)),
        }
    }

    /// Installs FTZ/DAZ once, on the calling (driver) thread.
    ///
    /// The core's helper sets MXCSR's FTZ/DAZ, which is **per thread** (Windows
    /// preserves it per context): doing it on the control thread would protect
    /// nothing. Once per callback would be two more CSR accesses per block for no
    /// additional safety, so the pump remembers it did it.
    fn ensure_ftz(&mut self) {
        if self.ftz_installed {
            return;
        }
        self.ftz_installed = true;
        if denormal::disable_denormals() {
            self.counters.note_ftz_installed();
        }
    }

    /// Silence the output: what the callback does once the control thread has asked
    /// the run to end. The stream still has to be fed *something*, and counting work
    /// we did not do would corrupt the report.
    pub fn render_stopped(&mut self, out: &mut [f32]) {
        fill_silence(out);
    }

    /// The table step behind [`Pump::actual_tone_hz`]: printed with it, so the
    /// quantisation ("440 Hz became 445.3125 Hz") is checkable from the report alone.
    pub fn tone_step(&self) -> usize {
        self.core.tone.step()
    }

    /// Frames currently in the FFT window. In `offline` this is read after the loop and
    /// proves the analysis window reached full size; in `run` the pump is on the driver
    /// thread by then, which is why the live report reads the transform counters instead.
    pub fn window_filled(&self) -> usize {
        self.core.window.filled()
    }

    /// The window's declared size, in frames.
    pub fn window_size(&self) -> usize {
        self.core.window.size()
    }

    /// Cold-starts the analysis window. `offline` calls this before its loop so the
    /// first cycle's "window still filling" state is a fact of the run, not an
    /// accident of construction; it costs one memset and happens outside the measured
    /// region.
    pub fn reset_analysis_window(&mut self) {
        self.core.window.reset();
    }

    /// The same chunk of work **without the instruments**, for [`crate::bench`]: no
    /// `AudioSection` (the bench owns the section — nesting one here would be refused
    /// by the guard), no counters, no ring push. Those costs are their own rows, and
    /// folding them into the headline number would make it a mixture instead of a
    /// measurement. The chunk's own results are summed into the returned `f64` so the
    /// optimiser cannot delete the analysis it is being asked to time.
    ///
    /// One chunk, at the pump's own shape: `out` must hold at least `chunk_frames ×
    /// channels` floats; a shorter buffer is pumped to its whole frames, which is what
    /// the live path does with a driver buffer.
    pub fn bench_chunk(&mut self, out: &mut [f32]) -> Result<f64, ProbeError> {
        self.ensure_ftz();
        let ch = self.core.channels;
        let frames = self.core.max_frames.min(out.len() / ch);
        if frames == 0 {
            return Ok(0.0);
        }
        let ctx = ProcessCtx {
            sample_rate: self.rate,
            frames,
            sample_position: self.core.position,
            transport: None,
        };
        let len = frames * ch;
        let rep = pump_chunk(
            self.chain.chain_mut(),
            &mut self.core,
            &mut out[..len],
            frames,
            &ctx,
            false,
        )
        .map_err(ProbeError::Node)?;
        Ok(rep.energy + f64::from(rep.peak_db) + f64::from(rep.rms_db))
    }

    /// The ring's declared capacity (the report prints it next to `ring_drops`).
    pub fn ring_capacity(&self) -> usize {
        self.ring.capacity()
    }
}

/// One chunk. The chain and the render core arrive as **separate arguments**: that
/// is the borrow split that makes step 4 compile, because the scratch-door closure
/// holds the door (from `chain`) while the window, plan and bins (from `core`)
/// receive it. Folded into one struct this is the classic `&&mut self` dead end.
fn pump_chunk(
    chain: &mut Chain,
    core: &mut RenderCore,
    out: &mut [f32],
    frames: usize,
    ctx: &ProcessCtx,
    monitor_input: bool,
) -> Result<ChunkReport, &'static str> {
    let ch = core.channels;
    let len = ch * frames;
    if monitor_input {
        deinterleave(out, &mut core.io[..len], ch, frames);
    } else {
        core.tone_fill(frames);
    }
    let mut report = ChunkReport::default();
    {
        let mut io = Block::from_slice(&mut core.io[..len], Shape::new(ch, frames));
        chain.process(ctx, &mut io).map_err(node_err)?;
        // Exact levels, measured on the block with the chunk's own shape: no padded
        // zeros in the RMS, which is what a door-sized measurement would add.
        report.peak_db = peak_db(&io);
        report.rms_db = rms_db(&io);
    }
    let mut ran = false;
    let RenderCore {
        io,
        window,
        fft,
        bins,
        energy,
        ..
    } = core;
    chain.with_scratch(|door| {
        door.zero();
        // Hoisted: `door.frames()` is an immutable read and `channels_mut()` holds a
        // mutable borrow of the same block for the loop — the borrow checker is right
        // that they cannot interleave, and the frame count does not change inside it.
        let door_frames = door.frames();
        for (c, dst) in door.channels_mut().take(ch).enumerate() {
            let src = &io[c * frames..(c + 1) * frames];
            let dst = &mut dst[..frames.min(door_frames)];
            dst.copy_from_slice(src);
        }
        // The transform reads the door's channel 0, never the planar rows directly:
        // the analyzer is downstream of the graph's own buffer.
        window.push(&door.channel_ref(0)[..frames]);
        if window.ready() {
            fft.process(window.samples(), bins);
            for b in bins.iter() {
                *energy += f64::from(b.norm_sqr());
            }
            ran = true;
        }
    });
    interleave(&core.io[..len], out, ch, frames);
    core.position += frames as u64;
    report.fft_ran = ran;
    report.energy = core.energy;
    Ok(report)
}

impl RenderCore {
    /// `frames` samples per channel into the planar rows (row `c` starts at `c·frames`).
    fn tone_fill(&mut self, frames: usize) {
        let len = self.channels * frames;
        for row in self.io[..len].chunks_mut(frames) {
            self.tone.fill(row);
        }
    }
}

/// Turns a node error into its message: `&'static str` in, `&'static str` out, so
/// the failure path allocates nothing either — a failing block is exactly the moment
/// the guard is still watching.
pub fn node_err(e: NodeError) -> &'static str {
    match e {
        NodeError::BufferTooSmall { .. } => "block claims more frames than it holds",
        NodeError::InvalidParam(s) | NodeError::Internal(s) => s,
    }
}

/// Same for the guard's own refusal.
pub fn section_err(e: AudioSectionError) -> &'static str {
    match e {
        AudioSectionError::AlreadyInside => "AudioSection entered twice on one thread",
    }
}

/// Zeroes a slice. Allocation-free; the last thing a failing callback does.
pub fn fill_silence(out: &mut [f32]) {
    out.iter_mut().for_each(|s| *s = 0.0);
}

/// Zeroes the ragged tail of a buffer whose length is not a multiple of `ch`.
fn fill_silence_ragged(out: &mut [f32], ch: usize, whole_frames: usize) {
    let start = whole_frames * ch;
    if start < out.len() {
        fill_silence(&mut out[start..]);
    }
}

/// The guard's tally for the last *closed* section: the report cross-checks it
/// against the pump's own `allocs_here` reading, so one broken counter cannot hide a
/// broken contract.
pub fn last_section_allocs() -> u64 {
    alloc_guard::allocs_last_section()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_summary::budget_ns;
    use std::sync::MutexGuard;

    /// Guard state is process-global — the panic-mode flag and both atomics — so every
    /// test that touches it in this binary must own it alone. That lock is
    /// **`crate::guard_serial()`, the one lock for the whole binary**, and not a private
    /// one: this module used to keep its own `static LOCK`, which serialised pump tests
    /// against each other and against nothing else. `offline_run`'s rehearsal and the
    /// negative control below then ran as siblings, and the process-wide total they both
    /// read was written by the other — a flaky zero, and (because `execute` for `offline`
    /// sets *strict* mode while other tests set *measure*) the mode a rehearsal believes
    /// it is enforcing could be flipped mid-loop. Poison is ignored on purpose (a failing
    /// assertion leaves the data sound) — the same deviation the core's guard tests
    /// document. **Every test that opens a section — directly or through `Pump::render` —
    /// takes it for the whole of its body**, so no test's zero is produced while a sibling
    /// flips the global mode or resets the shared counters.
    fn serial_guard() -> MutexGuard<'static, ()> {
        crate::guard_serial()
    }

    fn pump(rate: u32, ch: u16, chunk: u32, fft: usize, tone: Tone) -> (Pump, Consumer<u64>) {
        Pump::setup_with_ring(rate, ch, chunk, fft, tone, 4_096).expect("setup")
    }

    #[test]
    fn setup_builds_a_three_node_stereo_chain_at_the_requested_shape() {
        let (pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        drop(consumer);
        assert_eq!(pump.chain_names().len(), 3, "LP, peak, HP");
        assert_eq!(pump.shape().channels, 2);
        assert_eq!(pump.shape().frames, 64);
        assert_eq!(pump.chunk_frames(), 64);
        assert_eq!(pump.rate(), 48_000);
        assert_eq!(pump.channels(), 2);
        assert_eq!(pump.budget_ns(), budget_ns(64, 48_000));
        assert_eq!(pump.chain_latency(), 0, "minimum-phase biquads declare 0");
    }

    #[test]
    fn setup_refuses_zero_channels_and_zero_frames() {
        let e = Pump::setup(48_000, 0, 64, 1_024, Tone::Sine(440.0)).err();
        assert!(e.is_some(), "a chain with no channel is not a chain");
        let e = Pump::setup(48_000, 2, 0, 1_024, Tone::Sine(440.0)).err();
        assert!(e.is_some(), "a zero-frame block is not a block");
    }

    #[test]
    fn the_tone_is_reported_at_the_frequency_it_actually_plays() {
        let (pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        drop(consumer);
        assert!((pump.asked_tone_hz() - 440.0).abs() < 1e-6);
        assert!(
            (pump.actual_tone_hz() - 445.3125).abs() < 1e-3,
            "the table quantises 440 to bin 38; the pump must report that, not 440"
        );
    }

    #[test]
    fn a_clean_run_moves_zero_allocations_and_counts_everything() {
        let _serial = serial_guard();
        // Measure mode: a violation is *counted*, never unwound through an extern
        // "C" callback. `run` uses exactly this mode and FAILS on a non-zero count.
        alloc_guard::set_panic_on_audio_alloc(false);
        alloc_guard::reset_counters();
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        let mut out = [0.0f32; 128];
        for _ in 0..100 {
            pump.render(&mut out, false).expect("render");
        }
        let v = pump.counters().view();
        drop(pump);
        drop(consumer);
        assert_eq!(v.callbacks, 100);
        assert_eq!(v.chunks, 100, "one 64-frame callback is one chunk");
        assert_eq!(v.frames, 6_400);
        assert_eq!(
            v.allocs_sum, 0,
            "the contract: zero allocations in the callback"
        );
        assert_eq!(v.allocs_max_per_callback, 0);
        assert_eq!(
            alloc_guard::allocs_total(),
            0,
            "the guard's own process-wide tally agrees with the pump's per-section read"
        );
    }

    #[test]
    fn the_guard_is_live_in_this_binary_negative_control() {
        let _serial = serial_guard();
        alloc_guard::set_panic_on_audio_alloc(false);
        alloc_guard::reset_counters();
        let before = alloc_guard::allocs_total();
        {
            let section = AudioSection::enter().expect("enter");
            // LEGAL ALLOCATION: deliberate, inside a test that proves detection works.
            let leaked: Vec<f32> = vec![0.0; 4];
            assert_eq!(section.allocs_here(), 1, "the Vec was counted");
            drop(leaked);
        }
        // The witness is the *thread-local* count above plus the process-wide total, not
        // `allocs_last_section()`: every section closed by any thread stores into that one
        // atomic, so an unlocked sibling's alloc-free `Pump::render` can overwrite it with
        // 0 between my section closing and my reading it. Under the shared lock the total
        // has exactly one possible source, which is why it can be asserted as an equality.
        assert_eq!(
            alloc_guard::allocs_total(),
            before + 1,
            "a zero in the other test is meaningful only because this one is not"
        );
        alloc_guard::reset_counters();
    }

    #[test]
    fn a_process_wide_reading_belongs_to_no_single_thread() {
        // ROOT CAUSE OF THE FLAKY `offline_run` ASSERTION, kept as a test so it cannot be
        // re-misused: `allocs_total()` counts *every* thread's sections. A sibling test
        // that allocates inside a section — the negative control directly above, on
        // purpose — raises the number this test reads, and the pump's own contract was
        // untouched. So `guard_allocs == 0` is a statement about the process, and it is
        // only evidence about the code under test while the code under test owns the
        // process: hence `crate::guard_serial()` on every guard-touching test in this
        // binary, and hence this test, which demonstrates the contamination rather than
        // trusting it not to happen.
        let _serial = serial_guard();
        alloc_guard::set_panic_on_audio_alloc(false);
        alloc_guard::reset_counters();
        let before = alloc_guard::allocs_total();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let section = AudioSection::enter().expect("child enter");
                // LEGAL ALLOCATION: deliberate, in a child thread, to prove the sharing.
                let v: Vec<f32> = vec![0.0; 4];
                assert_eq!(section.allocs_here(), 1, "the child saw its own allocation");
                drop(v);
                // The child's own thread-local view says nothing about the parent, and the
                // parent's TLS stays 0: the only thing they share is the process-wide total.
                assert_eq!(
                    section.allocs_here(),
                    1,
                    "the free() did not count as a second allocation"
                );
            });
        });
        let after = alloc_guard::allocs_total();
        alloc_guard::reset_counters();
        assert!(
            after > before,
            "the child's section allocated inside its own window; the parent's reading of \
             the SAME process-wide counter moved from {before} to {after} — proof that a \
             process-wide total is not a per-test ground truth"
        );
    }

    #[test]
    fn a_480_frame_callback_is_pumped_as_seven_full_chunks_and_one_short_one() {
        let _serial = serial_guard();
        alloc_guard::set_panic_on_audio_alloc(false);
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        // What WASAPI shared mode actually hands over: the engine period, not 64.
        let mut out = [0.0f32; 480 * 2];
        pump.render(&mut out, false).expect("render");
        let v = pump.counters().view();
        drop(pump);
        drop(consumer);
        assert_eq!(v.callbacks, 1, "one driver callback");
        assert_eq!(v.chunks, 8, "⌈480/64⌉ = 8 DSP chunks: 7×64 + 32");
        assert_eq!(v.frames, 480, "the short tail is still counted in frames");
        assert_eq!(v.cb_frames_min, 480);
        assert_eq!(v.cb_frames_max, 480);
        assert_eq!(v.allocs_sum, 0, "chunking must not allocate");
    }

    #[test]
    fn the_output_is_real_signal_and_the_analysis_consumed_it() {
        let _serial = serial_guard();
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        let mut out = [0.0f32; 128];
        // 20 chunks = 1280 frames > the 1024-frame window: at least one transform.
        for _ in 0..20 {
            pump.render(&mut out, false).expect("render");
            assert!(
                out.iter().all(|s| s.is_finite()),
                "a non-finite sample means the chain or FTZ failed"
            );
        }
        let v = pump.counters().view();
        drop(pump);
        drop(consumer);
        // The tone is 0.2 amplitude = −14 dBFS, and the rebuilt chain passes it within
        // a few dB. A bound this tight is the point: it was a loose −60 dB that let the
        // original no-overlapping-passband chain look fine for as long as it did.
        let peak = Counters::from_bits(v.peak_bits);
        assert!(
            peak > -25.0 && peak < -3.0,
            "peak {peak:.1} dBFS: expected near −14 for a 0.2-amplitude tone through \
             LP 8k / Peak 1k+6dB / HP 100"
        );
        assert!(
            v.fft_runs > 0,
            "the transform ran at least once: {}",
            v.fft_runs
        );
        assert_eq!(
            v.fft_runs + v.fft_warm_skips,
            v.chunks,
            "every chunk answered"
        );
        // The pump pushes the chunk *then* asks the window, so the 16th push
        // (1024/64) is the first that is full: 15 chunks of warm-up, 5 transforms out
        // of 20. Exact numbers, because "at least some" would pass a pump that only
        // ever analysed one block in twenty.
        assert_eq!(
            v.fft_warm_skips, 15,
            "1024 frames / 64-frame chunks = 16 pushes to fill the window"
        );
        assert_eq!(v.fft_runs, 5, "chunks 16..20 analysed");
        let energy = Counters::energy_from_bits(v.energy_bits);
        assert!(energy > 0.0 && energy.is_finite(), "Σ|X|² = {energy}");
    }

    #[test]
    fn a_silence_tone_still_runs_the_chain_and_the_transform() {
        let _serial = serial_guard();
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Silence);
        let mut out = [7.0f32; 128];
        for _ in 0..20 {
            pump.render(&mut out, false).expect("render");
        }
        assert!(out.iter().all(|s| *s == 0.0), "silence is silence");
        let v = pump.counters().view();
        drop(pump);
        drop(consumer);
        assert_eq!(v.chunks, 20, "the chain ran on data anyway");
        assert!(v.fft_runs > 0, "and the transform consumed the door");
        assert!(
            Counters::from_bits(v.peak_bits) == f32::NEG_INFINITY,
            "digital silence has no peak, not a 0 dB one"
        );
    }

    #[test]
    fn monitor_input_reads_the_buffer_the_driver_left_behind() {
        let _serial = serial_guard();
        let (mut pump, consumer) = pump(48_000, 2, 4, 4, Tone::Silence);
        // Interleaved ramp: L = 0,1,2,3 · R = 10,11,12,13.
        let mut out = [0.0f32, 10.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0];
        pump.render(&mut out, true).expect("render");
        let v = pump.counters().view();
        drop(pump);
        drop(consumer);
        assert_eq!(v.chunks, 1, "4 frames at a 4-frame shape");
        assert!(
            out.iter().all(|s| s.is_finite()),
            "the chain consumed the input and produced finite output"
        );
    }

    #[test]
    fn a_ragged_buffer_tail_is_silenced_and_only_whole_frames_are_counted() {
        let _serial = serial_guard();
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        // 129 samples: 64 whole frames + one orphan.
        let mut out = [9.0f32; 129];
        pump.render(&mut out, false).expect("render");
        let v = pump.counters().view();
        drop(pump);
        drop(consumer);
        assert_eq!(v.cb_frames_max, 64, "the orphan is not a frame");
        assert_eq!(out[128], 0.0, "nothing uninitialised is handed to a DAC");
    }

    #[test]
    fn an_empty_buffer_is_silently_fine() {
        let _serial = serial_guard();
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        let mut nothing: [f32; 0] = [];
        pump.render(&mut nothing, false).expect("render");
        let v = pump.counters().view();
        drop(pump);
        drop(consumer);
        assert_eq!(
            v.callbacks, 1,
            "a callback happened, it just had nothing in it"
        );
        assert_eq!(v.chunks, 0, "no work was invented");
    }

    #[test]
    fn every_chunk_publishes_exactly_one_timing_sample() {
        let _serial = serial_guard();
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        let mut drain = crate::timing_ring::TimingDrain::new(consumer, 256);
        let mut out = [0.0f32; 480 * 2];
        for _ in 0..5 {
            pump.render(&mut out, false).expect("render");
        }
        let moved = drain.drain();
        let v = pump.counters().view();
        drop(pump);
        assert_eq!(
            moved as u64, v.chunks,
            "8 chunks × 5 callbacks, no sample lost"
        );
        assert_eq!(v.ring_drops, 0);
        assert!(
            drain.into_samples().iter().all(|s| *s > 0),
            "all costs are real"
        );
    }

    #[test]
    fn a_stopped_pump_writes_silence_and_counts_nothing() {
        let _serial = serial_guard();
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        let counters = pump.counters();
        let mut out = [3.0f32; 128];
        pump.render_stopped(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(
            counters.view().chunks,
            0,
            "the stop path must not invent work"
        );
        assert!(!counters.stopped(), "stopping is the control thread's call");
        counters.request_stop();
        assert!(counters.stopped());
        drop(pump);
        drop(consumer);
    }

    #[test]
    fn the_pump_is_send_because_cpal_moves_it_to_the_driver_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Pump>();
        assert_send::<OwnedChain>();
    }

    #[test]
    fn node_and_section_errors_map_to_static_messages() {
        assert_eq!(
            node_err(NodeError::BufferTooSmall { have: 8, need: 64 }),
            "block claims more frames than it holds"
        );
        assert_eq!(node_err(NodeError::Internal("boom")), "boom");
        assert_eq!(
            section_err(AudioSectionError::AlreadyInside),
            "AudioSection entered twice on one thread"
        );
    }

    #[test]
    fn entering_the_section_twice_is_reported_not_panicked() {
        let _serial = serial_guard();
        alloc_guard::set_panic_on_audio_alloc(false);
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        let outer = AudioSection::enter().expect("outer");
        let mut out = [0.0f32; 128];
        let e = pump.render(&mut out, false).expect_err("refused");
        assert!(
            matches!(e, ProbeError::Stream(_)),
            "a nested section is a stream/usage error: {e}"
        );
        assert_eq!(
            out, [0.0f32; 128],
            "a refused callback must still be silence"
        );
        drop(outer);
        let v = pump.counters().view();
        drop(pump);
        drop(consumer);
        assert_eq!(v.stream_errors, 1, "and it must be counted");
    }

    #[test]
    fn ftz_is_installed_once_and_reported() {
        let _serial = serial_guard();
        let (mut pump, consumer) = pump(48_000, 2, 64, 1_024, Tone::Sine(440.0));
        let mut out = [0.0f32; 128];
        pump.render(&mut out, false).expect("render");
        pump.render(&mut out, false).expect("render");
        assert!(
            pump.ftz_installed,
            "the pump remembers it did the CSR write"
        );
        assert!(
            pump.counters().view().ftz_set,
            "and the report can say FTZ/DAZ was on"
        );
        drop(pump);
        drop(consumer);
    }
}
