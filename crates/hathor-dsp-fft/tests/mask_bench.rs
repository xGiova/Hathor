//! The M2 cost row: whole-meter `update` cost per recompute, **measured** and
//! *declared under contention*. The lead's 2026-11-07 coordination note says
//! three parallel builders are compiling right now, so every number here is
//! printed with that caveat and the assertion is a disaster bound, not a gate
//! percentage — the masking meter is a display-rate element (it reads bank
//! levels, it is not in the per-block `process` path).
//!
//! Ignored by default because timing is meaningless in a debug build — run as
//! part of the gate with:
//!
//! ```text
//! cargo test -p hathor-dsp-fft --release --offline -- --ignored
//! ```
//!
//! Method copies the probe/bank idiom: fixed warm-up, batched reps with the one
//! `Instant` pair subtracted per batch, median with min…max. Input is a
//! deterministic dense spectrum (no zeros: every skirt tap fires — the worst
//! case for `update`'s zero-skip).

use hathor_dsp_fft::{MaskingMeter, MaskingParams};
use std::hint::black_box;
use std::time::Instant;

/// Reps inside one timed batch.
const REPS: usize = 32;
/// Warm-up reps, discarded.
const WARMUP: usize = 8;
/// Timed samples per row; the median is the quoted number.
const SAMPLES: usize = 32;

fn require_release() {
    if cfg!(debug_assertions) {
        panic!("this bench measures; a debug timing is a fiction — run with --release");
    }
}

/// Deterministic dense spectrum: every bin carries power (worst case for the
/// update loop — the zero-skip never fires).
fn dense_spectrum(n: usize) -> Vec<f32> {
    (0..n).map(|k| ((k % 97) as f32 + 1.0) * 1e-3).collect()
}

fn run_batched(mut f: impl FnMut()) -> (u64, u64, u64) {
    for _ in 0..WARMUP {
        f();
    }
    let mut v: Vec<u64> = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        for _ in 0..REPS {
            f();
        }
        v.push(t.elapsed().as_nanos() as u64 / REPS as u64);
    }
    v.sort_unstable();
    (v[SAMPLES / 2], v[0], v[SAMPLES - 1])
}

/// One meter update over the full default axis: 2 × accumulate (skirt tables of
/// radius ~173 each side) + the buried-min pass, then the dB render. The bound
/// is deliberately a disaster line (2 ms): what this row exists to *print* is
/// the honest median, contended, declared.
#[test]
#[ignore = "timing is measured in release only: cargo test --release -- --ignored"]
fn meter_update_and_render_cost_is_measured_and_declared() {
    require_release();
    let mut meter = MaskingMeter::new(&MaskingParams::new());
    let a = dense_spectrum(meter.bins());
    let b = dense_spectrum(meter.bins());
    let (med, lo, hi) = run_batched(|| {
        meter.update(black_box(&a), black_box(&b));
        meter.render_db();
        black_box(meter.mask_a()[meter.bins() / 2]);
    });
    println!(
        "measured (UNDER PARALLEL-BUILD CONTENTION, declared): meter update+render \
         median {med} ns, range {lo}…{hi} ns at {} bins × skirt radius {}",
        meter.bins(),
        meter.skirt_radius()
    );
    assert!(
        med <= 2_000_000,
        "a 2 ms recompute is a disaster line, measured {med} ns"
    );
}
