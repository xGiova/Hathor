//! Planar buffers: one contiguous block per channel.
//!
//! `ChannelBuffer` allocates **exactly once**, at setup. `Block` is a zero-cost view
//! that neither allocates nor locks, so it is the only thing that crosses the
//! callback.

/// Shape (channels × frames) of a buffer. Identical for the whole life of the
/// buffer: changing shape at runtime means reallocating, so it is a *setup* event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// Number of channels.
    pub channels: usize,
    /// Number of frames (samples per channel) in the block.
    pub frames: usize,
}

impl Shape {
    /// Creates a shape.
    pub const fn new(channels: usize, frames: usize) -> Self {
        Self { channels, frames }
    }
    /// Total number of samples.
    pub const fn len(&self) -> usize {
        self.channels * self.frames
    }
    /// True if it holds no samples at all (required by `clippy::len_without_is_empty`).
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Owned audio buffer, allocated at setup.
#[derive(Debug, Clone)]
pub struct ChannelBuffer {
    data: Vec<f32>,
    shape: Shape,
}

impl ChannelBuffer {
    /// Allocates `channels × frames` zeroed samples. **Call it in setup only.**
    pub fn new(channels: usize, frames: usize) -> Self {
        let shape = Shape::new(channels, frames);
        Self {
            data: vec![0.0f32; shape.len()],
            shape,
        }
    }

    /// Shape of the buffer.
    pub fn shape(&self) -> Shape {
        self.shape
    }

    /// Zeroes every sample (no allocation cost).
    pub fn zero(&mut self) {
        self.data.iter_mut().for_each(|s| *s = 0.0);
    }

    /// Mutable view over the contents.
    pub fn block(&mut self) -> Block<'_> {
        Block {
            data: self.data.as_mut_slice(),
            shape: self.shape,
        }
    }

    /// Read-only view over the contents.
    pub fn view(&self) -> View<'_> {
        View {
            data: self.data.as_slice(),
            shape: self.shape,
        }
    }
}

/// Mutable planar-layout view. None of its methods allocate.
pub struct Block<'a> {
    data: &'a mut [f32],
    shape: Shape,
}

impl<'a> Block<'a> {
    /// Builds a view over a slice that is already `shape.len()` long.
    ///
    /// # Panics
    /// If `data.len() != shape.len()`.
    pub fn from_slice(data: &'a mut [f32], shape: Shape) -> Self {
        assert_eq!(
            data.len(),
            shape.len(),
            "slice of the wrong length for the requested shape"
        );
        Self { data, shape }
    }

    /// Shape.
    pub fn shape(&self) -> Shape {
        self.shape
    }
    /// Channels.
    pub fn channels(&self) -> usize {
        self.shape.channels
    }
    /// Frames.
    pub fn frames(&self) -> usize {
        self.shape.frames
    }

    /// Mutable channel `c`.
    pub fn channel(&mut self, c: usize) -> &mut [f32] {
        assert!(c < self.shape.channels, "channel index out of range");
        let f = self.shape.frames;
        &mut self.data[c * f..c * f + f]
    }

    /// Channel `c` for reading.
    pub fn channel_ref(&self, c: usize) -> &[f32] {
        assert!(c < self.shape.channels, "channel index out of range");
        let f = self.shape.frames;
        &self.data[c * f..c * f + f]
    }

    /// Iterator over the channels (planar layout = `chunks_exact_mut`, zero cost).
    pub fn channels_mut(&mut self) -> impl Iterator<Item = &mut [f32]> {
        self.data.chunks_exact_mut(self.shape.frames)
    }

    /// All the samples, flat and mutable.
    pub fn flat_mut(&mut self) -> &mut [f32] {
        self.data
    }

    /// Silence.
    pub fn zero(&mut self) {
        self.data.iter_mut().for_each(|s| *s = 0.0);
    }

    /// Copies `src` into `self` (channels = the min of the two, frames = the min of
    /// the two). Needed because a mono node feeding into a stereo chain must be able
    /// to fill channel 0 only, without touching 1.
    pub fn copy_from(&mut self, src: &View<'_>) {
        let ch = self.shape.channels.min(src.shape.channels);
        let fr = self.shape.frames.min(src.shape.frames);
        for c in 0..ch {
            let (d, s) = (self.channel(c), src.channel(c));
            d[..fr].copy_from_slice(&s[..fr]);
        }
    }

    /// `self += src * gain`, without allocations.
    pub fn mix_add(&mut self, src: &View<'_>, gain: f32) {
        let ch = self.shape.channels.min(src.shape.channels);
        let fr = self.shape.frames.min(src.shape.frames);
        for c in 0..ch {
            let (d, s) = (self.channel(c), src.channel(c));
            for (out, inp) in d[..fr].iter_mut().zip(&s[..fr]) {
                *out += *inp * gain;
            }
        }
    }
}

/// Read-only view, mirror image of [`Block`].
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    data: &'a [f32],
    shape: Shape,
}

impl<'a> View<'a> {
    /// Builds a read-only view.
    /// # Panics
    /// If the length does not match the shape.
    pub fn from_slice(data: &'a [f32], shape: Shape) -> Self {
        assert_eq!(data.len(), shape.len(), "wrong slice length");
        Self { data, shape }
    }
    /// Shape.
    pub fn shape(&self) -> Shape {
        self.shape
    }
    /// Channel `c` for reading.
    pub fn channel(&self, c: usize) -> &[f32] {
        assert!(c < self.shape.channels, "channel index out of range");
        let f = self.shape.frames;
        &self.data[c * f..c * f + f]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planar_layout_is_contiguous_per_channel() {
        let mut buf = ChannelBuffer::new(2, 4);
        {
            let mut b = buf.block();
            for (c, ch) in b.channels_mut().enumerate() {
                for (i, s) in ch.iter_mut().enumerate() {
                    *s = c as f32 * 10.0 + i as f32;
                }
            }
        }
        let v = buf.view();
        assert_eq!(v.channel(0), &[0.0, 1.0, 2.0, 3.0]);
        assert_eq!(v.channel(1), &[10.0, 11.0, 12.0, 13.0]);
    }

    #[test]
    fn mix_add_leaves_extra_channels_untouched() {
        let mut dst = ChannelBuffer::new(2, 2);
        let mut src = ChannelBuffer::new(1, 2);
        src.block().channel(0).copy_from_slice(&[1.0, 2.0]);
        dst.block().mix_add(&src.view(), 0.5);
        assert_eq!(dst.view().channel(0), &[0.5, 1.0]);
        assert_eq!(dst.view().channel(1), &[0.0, 0.0]);
    }

    #[test]
    fn shape_len() {
        assert_eq!(Shape::new(8, 64).len(), 512);
        assert!(Shape::new(0, 64).is_empty());
    }
}
