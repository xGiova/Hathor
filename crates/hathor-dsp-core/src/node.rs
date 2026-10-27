//! The contract of a processable node.

use crate::buffer::Block;

/// Context passed to every processing cycle. `Copy`: it must never be built inside
/// the callback.
#[derive(Debug, Clone, Copy)]
pub struct ProcessCtx {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Frames contained in this cycle's `Block`.
    pub frames: usize,
    /// Sample position of the first frame of the block.
    pub sample_position: u64,
    /// Transport source, if the graph has one.
    pub transport: Option<Transport>,
}

impl ProcessCtx {
    /// Creates a minimal context.
    pub const fn bare(sample_rate: u32, frames: usize) -> Self {
        Self {
            sample_rate,
            frames,
            sample_position: 0,
            transport: None,
        }
    }

    /// Block duration in microseconds. The budget to measure the DSP against.
    pub fn block_budget_us(&self) -> f64 {
        self.frames as f64 * 1_000_000.0 / self.sample_rate as f64
    }

    /// Second control reading: 1.33 ms for a 64-frame block at 48 kHz.
    pub fn seconds_per_block(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }
}

/// Transport information visible to nodes (for synced delays, and the like).
#[derive(Debug, Clone, Copy)]
pub struct Transport {
    /// The graph is playing.
    pub playing: bool,
    /// BPM, if available.
    pub tempo_bpm: Option<f32>,
    /// Time signature numerator (for bar-length loops).
    pub beats_per_bar: u32,
}

/// Structural constraints the graph communicates to the nodes at setup, so that
/// nodes can allocate *here* and never again.
#[derive(Debug, Clone, Copy)]
pub struct GraphHints {
    /// Maximum number of channels the blocks will carry.
    pub max_channels: usize,
    /// Maximum number of frames per block.
    pub max_frames: usize,
}

/// Processing error. No `String`: allocating to report an error on the real path is
/// paradoxical (the error arrives exactly when the system is in trouble).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeError {
    /// The block received is smaller than the node requires.
    BufferTooSmall {
        /// Frames available.
        have: usize,
        /// Frames needed.
        need: usize,
    },
    /// Invalid parameter (e.g. a cutoff above Nyquist).
    InvalidParam(&'static str),
    /// Unrecoverable internal error, static message.
    Internal(&'static str),
}

/// A reusable block of DSP.
///
/// Implementations **must** respect the contract of the crate: no allocation, no
/// lock, no syscall in [`Node::process`].
pub trait Node {
    /// Human-readable name, for the meters and for graph debugging.
    fn name(&self) -> &str;

    /// Setup: allocate here, never in `process`.
    ///
    /// Default: `Ok(())`. A node that needs room for history (delay line, FFT
    /// overlap) computes its size from `hints` and allocates it here.
    fn setup(&mut self, _ctx: &ProcessCtx, _hints: &GraphHints) -> Result<(), NodeError> {
        Ok(())
    }

    /// In-place processing of a block. Returns latency/error, does not allocate.
    fn process(&mut self, ctx: &ProcessCtx, block: &mut Block<'_>) -> Result<(), NodeError>;

    /// **Latency the node introduces, in frames.** It must be reported from day one
    /// (rule 5 of the contract): PDC is built on this number, and a
    /// `linear-phase EQ` or a `lookahead compressor` lying here breaks the phase
    /// alignment between tracks — exactly the thing kick/bass tooling has to
    /// guarantee.
    fn latency_samples(&self) -> usize {
        0
    }

    /// *Maximum* possible latency as the parameters vary (e.g. a delay with an
    /// adjustable time). Default: equal to `latency_samples`.
    fn max_latency_samples(&self) -> usize {
        self.latency_samples()
    }

    /// Parameter setter. It must not allocate and must be idempotent: the UI can send
    /// the same value a hundred times per second.
    fn set_param(&mut self, _id: crate::param::ParamId, _value: f32) {}

    /// Read the current (smooth, already ramped) value of the parameter.
    fn param(&self, _id: crate::param::ParamId) -> Option<f32> {
        None
    }

    /// Reset state (filters, envelopes) without reallocating.
    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::ChannelBuffer;

    struct PassThru {
        lat: usize,
    }
    impl Node for PassThru {
        fn name(&self) -> &str {
            "passthru"
        }
        fn process(&mut self, _ctx: &ProcessCtx, _block: &mut Block<'_>) -> Result<(), NodeError> {
            Ok(())
        }
        fn latency_samples(&self) -> usize {
            self.lat
        }
    }

    #[test]
    fn block_budget_for_64_frames_at_48k() {
        let ctx = ProcessCtx::bare(48_000, 64);
        assert!((ctx.block_budget_us() - 1333.33).abs() < 0.01);
        assert!((ctx.seconds_per_block() - 64.0 / 48_000.0).abs() < 1e-12);
    }

    #[test]
    fn passthrough_node_processes_and_reports_latency() {
        let mut buf = ChannelBuffer::new(2, 8);
        let mut n = PassThru { lat: 4 };
        let ctx = ProcessCtx::bare(44_100, 8);
        let mut b = buf.block();
        assert!(n.process(&ctx, &mut b).is_ok());
        assert_eq!(n.max_latency_samples(), 4);
    }
}
