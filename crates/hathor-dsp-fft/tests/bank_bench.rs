//! The M1 cost gate: whole-bank cost per 64-frame block and per full second at
//! 48 kHz, **measured** against the 1333.33 µs budget (8%-of-core acceptance).
//!
//! Ignored by default because timing is meaningless in a debug build — run it as
//! part of the gate with:
//!
//! ```text
//! cargo test -p hathor-dsp-fft --release --offline -- --ignored
//! ```
//!
//! Method copies `apps/hathor-audio-probe/src/bench.rs`: fixed warm-up, then
//! samples of batched reps (a batch is one timed region, so the `Instant` pair is
//! paid once per batch), median reported with min…max, and the measured pair
//! overhead **subtracted from every batch** — the probe cites 42.7 ns for the
//! `Instant::now() + elapsed()` pair on this machine; we re-measure it rather than
//! trusting the citation, and say so in the printed rows. Input is a deterministic
//! 440 Hz sine (CONVENTION §5: no RNG in numeric work).

use hathor_dsp_core::BUDGET_64_48K_US;
use hathor_dsp_fft::{BankParams, FftBank};
use std::hint::black_box;
use std::time::Instant;

/// Block size the budget is quoted for.
const BLOCK: usize = 64;
/// Reps inside one timed batch: 128 blocks ≈ 8.5 ms of work at the expected cost
/// (~5 µs/block), comfortably above the clock quantum, and the single pair inside
/// it shrinks to pair/128 per block.
const REPS: usize = 128;
/// Warm-up reps, discarded (probe's WARMUP_REPS).
const WARMUP: usize = 16;
/// Timed samples per row; the median is the quoted number (probe's method line).
const SAMPLES: usize = 64;
/// Acceptance: 8% of the block budget (the M1 gate).
const ACCEPTANCE: f64 = 0.08;

fn sine_block(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (std::f64::consts::TAU * 440.0 * i as f64 / 48_000.0).sin() as f32)
        .collect()
}

fn require_release() {
    if cfg!(debug_assertions) {
        panic!("this bench measures; a debug timing is a fiction — run with --release");
    }
}

/// `Instant::now() + elapsed()` pair, timed the same way the rows are timed: one
/// pair is inside every batch, so `batch_ns/reps` carries pair/reps of timer
/// noise — this row says how big that is, and `run_batched` subtracts it.
fn measure_pair_overhead() -> u64 {
    let mut v: Vec<u64> = Vec::with_capacity(16);
    for _ in 0..16 {
        let t = Instant::now();
        for _ in 0..10_000 {
            let t2 = Instant::now();
            black_box(t2.elapsed().as_nanos());
        }
        v.push(t.elapsed().as_nanos() as u64 / 10_000);
    }
    v.sort_unstable();
    v[8]
}

/// One row: run `f` for SAMPLES batches of REPS reps, return (median, min, max)
/// per-rep ns after subtracting `pair_ns` once per batch.
fn run_batched(pair_ns: u64, mut f: impl FnMut()) -> (u64, u64, u64) {
    for _ in 0..WARMUP {
        f();
    }
    let mut v: Vec<u64> = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        for _ in 0..REPS {
            f();
        }
        let batch = t.elapsed().as_nanos() as u64;
        let per_rep = (batch.saturating_sub(pair_ns)) / REPS as u64;
        v.push(per_rep);
    }
    v.sort_unstable();
    (v[SAMPLES / 2], v[0], v[SAMPLES - 1])
}

/// Steady state: 64-frame blocks. At 48 kHz there are 750 per second, so the
/// per-second cost is `median_ns × 750 / 1000` µs — the same number read in two
/// units (per-block against the budget, per-second against one core).
#[test]
#[ignore = "timing is measured in release only: cargo test --release -- --ignored"]
fn whole_bank_steady_block_cost_is_within_eight_percent() {
    require_release();
    let pair_ns = measure_pair_overhead();
    let mut bank = FftBank::new(&BankParams::new(48_000.0));
    let block = sine_block(BLOCK);
    let last_bin = bank.bins() - 1;
    let (med, lo, hi) = run_batched(pair_ns, || {
        bank.process(&block);
        // Materialize: the guard reads one float, not a fold, so the timing is
        // the bank's cost and not the observer's (the probe's black_box idiom).
        black_box(bank.levels()[last_bin]);
    });
    let budget_ns = (BUDGET_64_48K_US * 1_000.0) as u64; // 1_333_333 ns
    let pct = med as f64 * 100.0 / budget_ns as f64;
    let per_second_us = med as f64 * 750.0 / 1_000.0;
    println!(
        "measured: Instant pair {} ns (probe cites 42.7 ns; subtracted once per batch of {REPS})",
        pair_ns
    );
    println!(
        "measured: bank per 64-frame block: median {med} ns, range {lo}…{hi} ns = {:.2}% of the {:.2} µs budget",
        pct, BUDGET_64_48K_US
    );
    println!(
        "measured: bank per full second at 48 kHz: {per_second_us:.1} µs = {:.3}% of one core",
        per_second_us / 10_000.0
    );
    assert!(
        med as f64 <= ACCEPTANCE * budget_ns as f64,
        "measured median {med} ns exceeds 8% of the budget ({:.0} ns)",
        ACCEPTANCE * budget_ns as f64
    );
    assert!(
        per_second_us <= ACCEPTANCE * 1_000_000.0,
        "measured {per_second_us:.1} µs/s exceeds 8% of a core"
    );
}

/// Worst case: one call refreshes **all four tiers** (a 4096-frame block lands on
/// every schedule). The budget a call this long owns is 64 blocks' worth
/// (85,333 µs); the 8% acceptance is against that scaled budget — and the printed
/// row also gives the per-frame cost so a reader can re-quote it in any unit.
#[test]
#[ignore = "timing is measured in release only: cargo test --release -- --ignored"]
fn all_tier_trigger_block_cost_is_bounded() {
    require_release();
    let pair_ns = measure_pair_overhead();
    let mut bank = FftBank::new(&BankParams::new(48_000.0));
    let block = sine_block(4_096);
    // Advance to a schedule-aligned point before timing: 16384 frames prime every
    // tier, so from frame 16384 every timed call triggers all four tiers.
    let prime = sine_block(16_384);
    bank.process(&prime);
    let last_bin = bank.bins() - 1;
    let (med, lo, hi) = run_batched(pair_ns, || {
        bank.process(&block);
        black_box(bank.levels()[last_bin]);
    });
    let scaled_budget_ns = (BUDGET_64_48K_US * 1_000.0 * (4_096.0 / BLOCK as f64)) as u64;
    let per_frame = med as f64 / 4_096.0;
    println!(
        "measured: all-tiers trigger call: median {med} ns, range {lo}…{hi} ns = {:.3}% of the scaled budget, {:.1} ns/frame",
        med as f64 * 100.0 / scaled_budget_ns as f64,
        per_frame
    );
    assert!(
        med as f64 <= ACCEPTANCE * scaled_budget_ns as f64,
        "measured {med} ns exceeds 8% of the scaled budget ({scaled_budget_ns} ns)"
    );
}
