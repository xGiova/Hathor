//! The sliding analysis window the real FFT reads: `N` frames of channel 0, kept
//! live by shifting, never by allocating.
//!
//! ## Why a shift and not a circular buffer
//!
//! `RealFftPlan::process` wants its input as one contiguous `&[f32]` of exactly `N`
//! samples — a ring would need either two copies or a bit-reversal that understands
//! wrapped indices. A `copy_within` of one `N`-sized block per chunk is a memmove of
//! `N − chunk` floats: at `N = 1024`, `chunk = 64` that is ~3.75 KB per block, an
//! L1-resident `rep movsb` measured at well under 1 µs (**expected**, and `bench`
//! measures the transform around it, so the shift is inside the reported number).
//! M1's analyzer will want a proper overlap-add; until then the *honest* way to feed
//! a 1024-point transform from 64-frame callbacks is to slide, and say so.
//!
//! The window holds channel 0 only: the probe asks "is the chain producing the tone
//! I asked for", and one channel answers that. Feeding it the sum of channels would
//! be a different measurement and would be labelled differently.

/// Contiguous last-`N`-frames window.
#[derive(Debug, Clone)]
pub struct FftWindow {
    samples: Vec<f32>,
    size: usize,
    filled: usize,
}

impl FftWindow {
    /// A zeroed window of `size` frames.
    ///
    /// LEGAL ALLOCATION: setup only. This is the window the transform reads for the
    /// whole life of the stream.
    pub fn new(size: usize) -> Self {
        assert!(size >= 4, "FftWindow shorter than 4 frames is not a signal");
        Self {
            // LEGAL ALLOCATION: setup thread.
            samples: vec![0.0f32; size],
            size,
            filled: 0,
        }
    }

    /// Window length in frames.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Frames pushed so far, clamped at `size`.
    pub fn filled(&self) -> usize {
        self.filled
    }

    /// True once `size` frames have been pushed: the transform may run.
    pub fn ready(&self) -> bool {
        self.filled >= self.size
    }

    /// The `size` frames the transform will read. Zero-padded before the window is
    /// full — which is why the pump only calls it when [`FftWindow::ready`].
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Appends `new`, dropping the oldest frames to keep the length at `size`.
    /// Zero allocations, zero locks.
    pub fn push(&mut self, new: &[f32]) {
        let n = new.len();
        if n == 0 {
            return;
        }
        if n >= self.size {
            // Keep the newest `size` frames of what we were handed.
            let start = n - self.size;
            self.samples.copy_from_slice(&new[start..]);
            self.filled = self.size;
            return;
        }
        // Shift left by n, then append: [n..size] → [0..size-n], tail = new.
        self.samples.copy_within(n..self.size, 0);
        self.samples[self.size - n..].copy_from_slice(new);
        self.filled = (self.filled + n).min(self.size);
    }

    /// Rewinds to silence without reallocating (the pump uses it between runs).
    pub fn reset(&mut self) {
        self.samples.iter_mut().for_each(|s| *s = 0.0);
        self.filled = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_ready_exactly_after_size_frames() {
        let mut w = FftWindow::new(4);
        assert!(!w.ready(), "a window of nothing is not a signal");
        w.push(&[1.0, 2.0]);
        assert_eq!(w.filled(), 2);
        assert!(!w.ready());
        w.push(&[3.0]);
        assert!(!w.ready());
        w.push(&[4.0]);
        assert!(w.ready());
        assert_eq!(w.samples(), &[1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn sliding_keeps_the_newest_frames_in_order() {
        let mut w = FftWindow::new(4);
        for f in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0] {
            w.push(&[f]);
        }
        assert_eq!(w.samples(), &[3.0, 4.0, 5.0, 6.0], "the last 4 of 6");
        assert_eq!(
            w.filled(),
            4,
            "filled clamps at size, it does not count forever"
        );
    }

    #[test]
    fn pushes_larger_than_the_window_keep_their_own_tail() {
        let mut w = FftWindow::new(4);
        w.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(w.samples(), &[4.0, 5.0, 6.0, 7.0]);
        assert!(w.ready());
    }

    #[test]
    fn an_exact_size_push_is_a_plain_replace() {
        let mut w = FftWindow::new(4);
        w.push(&[0.5, 0.5, 0.5, 0.5]);
        w.push(&[9.0, 8.0, 7.0, 6.0]);
        assert_eq!(w.samples(), &[9.0, 8.0, 7.0, 6.0]);
    }

    #[test]
    fn empty_and_reset_paths_do_not_shift_the_world() {
        let mut w = FftWindow::new(8);
        w.push(&[]);
        assert_eq!(w.filled(), 0, "an empty push is not a frame");
        w.push(&[1.0; 8]);
        w.reset();
        assert_eq!(w.filled(), 0);
        assert!(!w.ready());
        assert!(w.samples().iter().all(|s| *s == 0.0), "reset is silence");
    }

    #[test]
    fn the_64_frame_chunk_case_shifts_exactly_960_frames_per_push() {
        // The real shape: N = 1024, chunk = 64. The window must be full after 16
        // chunks and must contain the newest 16 of them.
        let mut w = FftWindow::new(1_024);
        for chunk in 0..20u32 {
            let start = (chunk * 64) as f32;
            let block: Vec<f32> = (0..64).map(|i| start + i as f32).collect();
            w.push(&block);
        }
        assert!(w.ready());
        assert_eq!(
            w.samples().len(),
            1_024,
            "samples() is exactly the transform's input length"
        );
        assert_eq!(
            w.samples()[0],
            4.0 * 64.0,
            "chunks 0..=3 have been slid out: the newest 16 remain"
        );
        assert_eq!(
            *w.samples().last().unwrap_or(&0.0),
            19.0 * 64.0 + 63.0,
            "the newest frame of the newest chunk"
        );
    }

    #[test]
    #[should_panic(expected = "shorter than 4 frames")]
    fn a_window_below_four_frames_is_refused() {
        FftWindow::new(2);
    }
}
