//! The layout edge: a driver hands us interleaved frames, our `Block`s are planar.
//!
//! [architecture.md](../../../docs/architecture.md) §5 says interleaved lives only at
//! the edges and that the transpose cost must be *measured*, not hidden. These two
//! functions are the whole edge of this binary, so they are (a) pure, (b) unit-tested
//! against a hand-computed 2×3 example, and (c) measured by `bench` as its own row —
//! including the version with the transpose folded into the AVX2 lane kernel.
//!
//! Both are in-place-safe (`dst` may alias nothing) and allocation-free; the stride of
//! the planar side is `frames`, which is what lets a short final chunk of a callback
//! use the same buffer as a full one.

/// Interleaved `src` (`frames × channels`, frame-major) → planar `dst`
/// (`channels` rows of `frames`, row-major).
///
/// # Panics
/// If either slice is not exactly `channels * frames` long. That is a caller bug
/// (the lengths come from the driver's own `data.len()`), and a panic is the only
/// honest answer to it — see STYLE §5.
pub fn deinterleave(src: &[f32], dst: &mut [f32], channels: usize, frames: usize) {
    let len = channels * frames;
    assert_eq!(src.len(), len, "interleaved side of the wrong length");
    assert_eq!(dst.len(), len, "planar side of the wrong length");
    if channels == 0 || frames == 0 {
        return;
    }
    for (f, frame) in src.chunks_exact(channels).enumerate() {
        for (c, s) in frame.iter().enumerate() {
            dst[c * frames + f] = *s;
        }
    }
}

/// Planar `src` (`channels` rows of `frames`) → interleaved `dst`.
///
/// # Panics
/// As [`deinterleave`].
pub fn interleave(src: &[f32], dst: &mut [f32], channels: usize, frames: usize) {
    let len = channels * frames;
    assert_eq!(src.len(), len, "planar side of the wrong length");
    assert_eq!(dst.len(), len, "interleaved side of the wrong length");
    if channels == 0 || frames == 0 {
        return;
    }
    for (c, row) in src.chunks_exact(frames).enumerate() {
        for (f, s) in row.iter().enumerate() {
            dst[f * channels + c] = *s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_two_frames_is_the_hand_computed_case() {
        // interleave: [L0, R0, L1, R1] ← planar [L0, L1, R0, R1] (stride 2)
        let planar = [1.0f32, 2.0, 3.0, 4.0];
        let mut out = [0.0f32; 4];
        interleave(&planar, &mut out, 2, 2);
        assert_eq!(out, [1.0, 3.0, 2.0, 4.0], "L0 R0 L1 R1");
    }

    #[test]
    fn deinterleave_is_the_exact_inverse() {
        let interleaved = [
            0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.2,
        ];
        let mut planar = [0.0f32; 12];
        // 3 channels × 4 frames.
        deinterleave(&interleaved, &mut planar, 3, 4);
        assert_eq!(
            planar,
            [0.1, 0.4, 0.7, 1.0, 0.2, 0.5, 0.8, 1.1, 0.3, 0.6, 0.9, 1.2],
            "row c = channel c, stride = frames"
        );
        let mut back = [0.0f32; 12];
        interleave(&planar, &mut back, 3, 4);
        assert_eq!(back, interleaved, "round trip is identity");
    }

    #[test]
    fn mono_is_the_identity_but_still_writes() {
        let src = [5.0f32, 6.0, 7.0];
        let mut planar = [0.0f32; 3];
        deinterleave(&src, &mut planar, 1, 3);
        assert_eq!(planar, src);
    }

    #[test]
    fn a_short_tail_chunk_uses_the_same_buffer_with_a_smaller_stride() {
        // The 96-frame callback case: a full 64 then a tail of 32, same allocation.
        let mut planar = [0.0f32; 128];
        let inter = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        deinterleave(&inter, &mut planar[..6], 2, 3);
        assert_eq!(&planar[..6], &[1.0, 3.0, 5.0, 2.0, 4.0, 6.0]);
        let mut out = [0.0f32; 6];
        interleave(&planar[..6], &mut out, 2, 3);
        assert_eq!(out, inter, "stride 3 planar round-trips");
    }

    #[test]
    fn zero_frames_writes_nothing_and_does_not_divide_by_zero() {
        let mut out = [9.0f32; 0];
        interleave(&[], &mut out, 2, 0);
        deinterleave(&[], &mut out, 0, 4);
        assert!(out.is_empty());
    }

    #[test]
    #[should_panic(expected = "interleaved side of the wrong length")]
    fn a_mismatched_length_is_a_panic_not_a_partial_copy() {
        let mut dst = [0.0f32; 4];
        deinterleave(&[1.0, 2.0], &mut dst, 2, 2);
    }

    #[test]
    #[should_panic(expected = "planar side of the wrong length")]
    fn a_short_planar_side_is_a_panic() {
        let mut dst = [0.0f32; 2];
        deinterleave(&[1.0, 2.0, 3.0, 4.0], &mut dst, 2, 2);
    }
}
