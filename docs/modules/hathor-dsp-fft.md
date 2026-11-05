# Module: `hathor-dsp-fft`

> Status: **written, tested 43** — 36 lib + 7 allocation-contract; kernels **and** the M1
> multi-rate bank landed (the 2 cost benches are `#[ignore]`d, run only in the release gate).
> Dependencies:
> **none in the product** — `hathor-dsp-core` is a **dev-dependency only** (the guard tests in
> `tests/alloc_contract.rs` need `AudioSection`; nothing in the shipped kernels imports it), and
> `rustfft` likewise (in tests only, never in the product).

## Purpose

The fast Fourier transform, **written by us**, for two non-negotiable reasons:

1. The project is "rewrite the DSP from 0": the FFT is the heart of the analyzer (M1), the
   masking meter and null test (M3), polyphase oversampling (M4), convolution reverb (M4).
   Using a library for *all* of that means never having written it.
2. The actual usage (multi-rate bank, windows, hop, magnitude in dB) requires control over
   allocation and layout that general-purpose libraries do not give.

`rustfft` stays as a **dev-dependency**: every one of our kernels is compared against it in
the numerical tests (tolerance 1e-4). It is the independent reference — the lesson of the
`−a2·out` bug in `hathor-dsp-filters` applied by construction.

## How it works

### Phase M0 — the three kernels

```rust
pub struct Complex { re: f32, im: f32 }   // the decision recorded in "Rust concepts" below, shipped:
                                          // Copy, clearer than (f32, f32), same layout as [f32; 2]

pub struct FftPlan {                      // NO scratch field: the complex kernel is in-place
    n: usize,
    twiddles: Vec<Complex>,               // e^{-2πik/n}, k < n/2 (half the table, half the cache lines)
    bitrev: Vec<u32>,                     // the permutation that makes it in-place
}
impl FftPlan {
    pub fn new(n: usize) -> Self;                    // power of two ≥ 2; ALLOCATES HERE, nowhere else
    pub fn n(&self) -> usize;                        // documented panic on a bad n: programmer bug, setup
    pub fn process(&mut self, data: &mut [Complex]); // in-place, e^{-2πikj/n} convention
    pub fn inverse(&mut self, data: &mut [Complex]); // conj → process → conj → /n, same kernel
}

pub struct RealFftPlan {                             // the ONE plan that carries scratch: packing an
    plan: FftPlan,                                   // &[f32] input needs a destination that is not
    unpack_twiddles: Vec<Complex>,                   // the caller's memory
    scratch: Vec<Complex>,
}
impl RealFftPlan {
    pub fn new(n_samples: usize) -> Self;            // power of two ≥ 4
    pub fn n_samples(&self) -> usize;                // N
    pub fn bin_count(&self) -> usize;                // N/2 + 1 (DC..=Nyquist)
    pub fn process(&mut self, samples: &[f32], bins: &mut [Complex]); // reads N, writes N/2+1
}

pub fn bin_freq(k: usize, n: usize, sample_rate: f32) -> f32;   // k·fs/n — the only home (invariant 3)
pub fn freq_bin(freq: f32, n: usize, sample_rate: f32) -> f32;  // its inverse: fractional bin coordinate
```

* **Radix-2 DIT**: bit-reversal permutation + `log2(n)` stages of butterflies. The twiddles
  `e^{-2πik/n}` are precomputed in the `FftPlan` (setup), never computed in process.
* **Inverse** via the conjugation trick: `IFFT(X) = conj(FFT(conj(X)))/n`. One kernel, two
  directions, and the inverse is as exact as the forward.
* **Real-FFT via packing**: a real signal of N samples → N/2 complex (even samples in the
  real parts, odd in the imaginary) → an FFT of N/2 → *unpack* with hermitian symmetry:

  ```
  X[k] = ½(Y[k] + conj(Y[N/2−k])) − ½·i·e^{−2πik/N}·(Y[k] − conj(Y[N/2−k]))
  ```

  Output: N/2+1 bins. Cost: half the complex FFT + O(N) of unpack. The k=0 and k=N/2 cases
  (purely real bins) have to be handled by hand — the classic place where "almost right"
  unpacking goes wrong.

### Phase M1 — the multi-rate bank (the SPAN-style analyzer)

The real product is not "an FFT", it is a **bank**:

| Size | Update rate | Band served |
|---|---|---|
| 256 | every 64 frames | 4-24 kHz (reactivity) |
| 1024 | every 256 frames | 500 Hz-4 kHz |
| 4096 | every 1024 frames | 100-500 Hz |
| 16384 | every 4096 frames | 20-100 Hz (low-end resolution) |

Each size: an accumulation ring buffer (hop = N/4; the ring is stored twice over, so a frame
read is one linear slice, no modulo), a precomputed Hann window, the FFT, squared magnitude
spread with the normalization law below onto ~1024 shared log-spaced bins (20 Hz–24 kHz;
two-point weights `w0 + w1 = 1`, fixed at setup), smoothing per bin (moving average + peak-hold
with decay). It is the architecture that gives SPAN its look: reactive at the top, detailed at
the bottom. The table is pinned by `the_tier_table_is_the_doc_table` — code must equal the doc,
or the doc drifts. The shipped surface:

```rust
pub struct BankParams { /* sample_rate, bins = 1024, floor_db = −120, per-tier TierTuning */ }
impl BankParams { pub fn new(sample_rate: f32) -> Self; pub fn bypass(sample_rate: f32) -> Self; }

pub struct FftBank;                              // FftBank::new: the type's ONLY allocation site
impl FftBank {
    pub fn new(params: &BankParams) -> Self;     // every exp()/ln() the bank will ever need lives here
    pub fn process(&mut self, frames: &[f32]);   // audio rate: ONE channel; no alloc, no lock, no transcendental
    pub fn levels(&self) -> &[f32];              // the audio-thread product: linear power, full-scale sine = 1.0
    pub fn render_db(&mut self);                 // display-thread work: the log10 + floor clamp
    pub fn db(&self) -> &[f32];                  // the dB view as of the last render_db
    pub fn reset(&mut self);                     // Node::reset contract: clear without realloc
    pub fn log_bin_freq(&self, i: usize) -> f32; // invariant-3 helper for the log axis (a powf: display-side)
}
```

* **The dB materialization law.** `process` never writes dB. The audio-thread output is linear
  `levels()`; the `log10` belongs to the display thread, which calls `render_db()` at frame rate
  and reads `db()` (silence clamps to `floor_db`, default −120). A transcendental inside the
  deadline is an unbounded deadline — the same split SPAN itself must do, and what keeps
  `process` transcendental-free.
* **The normalization law: `(4/N)²`.** A full-scale sine reads power **1.0 (0 dBFS) at every
  tier**: Hann halves the on-bin peak and the cosine halves it again — the bin holds N/4.
  Without one law across tiers the cross-tier max would silently pick the largest N.
* **DC is excluded from the mapping** (`k = 1..=nc`: Nyquist in, k=0 out) — four tiers clamping
  DC onto bin 0 would **quadruple-count** it. A pure-DC stream still leaks through the three
  exact Hann deltas (0.5N at DC, −0.25N at k = ±1): `a_pure_dc_stream_only_touches_the_k1_skirt`
  pins skirt bins to ≤ 1.0·(their weight) and every other bin to < 1e-3 — and the floor bin is
  never lit, which is the skipped k=0 proving itself.
* **Trigger semantics.** A block covering several hops refreshes a due tier **once, against the
  newest full frame**: the analyzer shows the latest spectrum and per-call cost has a hard
  ceiling. Skipped hops are skipped *arithmetically* (`next_due += (overdue/hop + 1)·hop`) —
  no catch-up, because nobody watches the intermediate spectra. A block longer than the biggest
  ring (16384) is legal; the rings simply keep their tail.
* **Junction continuity is the cross-tier max per bin, not band-gated writes.** Every tier
  reads bin-centers within the same half-bin scalloping bound, so the max is never worse than
  either tier alone; `TIER_BANDS_HZ` declares *primary* ownership only — neighbouring tiers
  contribute everywhere. Measured against an independent prediction (f64 direct-DFT powers,
  never our kernel): `every_junction_is_continuous_on_a_known_sine` at 100/500/4k/23,990 Hz.
* **The tuning is expected, not measured.** Defaults τ = 10/30/100/250 ms, decay = 45/30/20/20
  dB/s (fast→slow, steep→gentle) are *starting points for the M2 listening pass against
  reference displays*, labelled so on purpose. `BankParams::bypass` (instant average,
  never-decaying hold) is the mode every analytic comparison in the tests runs in.
* **Edge physics, discovered and pinned.** Bins touched by a tier's *edge* FFT bins
  (`k ≤ 2` or `k ≥ nc−2`) fold the real signal's negative-frequency mirror onto the axis; the
  fold is phase-dependent — it rotates with every hop — so the max-of-history hold legitimately
  sits above any single frame (up to **1.13×** measured during development). The tests bracket
  those bins at **[0.99, 1.20]** of the frame prediction, guard named for the reason
  (`edge_touched`); equality everywhere else is **≤ 2%** relative, interior measured under
  **0.1%**. Naïve physics died here too: after one hop of silence the 16384-sample frame still
  contains 75% of the tone and *re-raises* the hold — the naïve one-hop drop (1.707 dB at
  20 dB/s) is wrong physics; measured **0.79 dB**, honest bracket [0.3, 2.2]
  (`hold_decays_after_the_signal_stops`). The derived constants themselves —
  `α = 1 − e^{−T/τ}`, `decay = 10^{−(dB/s)·T/10}`, `T = hop/fs` per tier — are re-evaluated in
  f64 against the bank's f32 fields at ≤ 1e-6
  (`smoothing_constants_are_derived_from_the_formulas`): "measured, not opined" as arithmetic.

### Verification (tests, not opinions)

1. Unit impulse → flat spectrum at 0 dB (±1e-4).
2. A sinusoid exactly on bin k → all the energy in k (zero leakage without a window); with
   Hann → the known window shape, compared against the analytic formula.
3. **Parseval**: Σ|x|² = Σ|X|²/N (±1e-3): catches scaling errors in both directions, the most
   common bug in home-made FFTs.
4. Point-by-point comparison with `rustfft` on a pseudo-random signal: 256/1024/4096/16384,
   max err < 1e-4.
5. Round-trip `process` → `inverse` = identity (±1e-4).
6. Real-FFT: the unpack against a full-size complex FFT on the same signal: identical (±1e-4).
   This test *is* the specification of the unpack — if you get the hermitian formula wrong, it
   dies here.
7. **Bank** (12 tests, bypass mode, expectations computed *independently* — f64 direct-DFT
   powers through re-derived axis arithmetic, never our kernel): the tier table equals the
   doc's; the log axis is monotone and exact at the ends (20 / 24,000 Hz); DC only in the k=1
   skirt; every junction continuous on a known sine (interior ≤ 2%, edge-fold bracketed
   [0.99, 1.20]); each tier lights its own band (±12-bin value window must match the
   prediction, +40 bins must be > 100× down); mapping weights conserve power entry-wise — the
   mapping's own Parseval, entry-wise because the honest full-spectrum anchor is a direct
   16384-point DFT (≈134M transcendentals, seconds in debug: structure tested entry-wise,
   placement tested externally); hold decay with the honest bracket; smoothing constants
   re-derived ≤ 1e-6; `render_db` maps levels and floor; constructor panic guards (rate outside
   44.1k–192k, bins outside 16..=65535 — the map stores u16); `reset` → the second cycle is
   identical.
8. **Allocation contract** (7 tests, `tests/alloc_contract.rs`): `CheckedAllocator` installed,
   each run-phase test enters an `AudioSection` and asserts the thread-local counter stays zero
   — for all three `process` paths *and* `inverse`; two *measure*-mode control tests prove the
   harness itself sees allocations (a guard that counts nothing passes a zero assertion
   forever) and constructor allocations are visible. Release-trap discipline: `black_box` at
   the call plus a real assertion afterwards — the optimizer may not elide the guarded work.
9. **Cost gate** (2 `#[ignore]` benches, `tests/bank_bench.rs` — a debug timing is a fiction,
   `require_release` panics first): whole bank ≤ 8% of the block budget *and* ≤ 8% of a core
   per second; worst case (a 4096-frame block triggering all four tiers at once) ≤ 8% of the
   scaled budget. Method copies `hathor-audio-probe`'s bench: discarded warm-up, batched reps,
   the `Instant` pair overhead *re-measured and subtracted* per batch rather than trusting the
   probe's quoted 42.7 ns, deterministic 440 Hz sine (CONVENTION §5: no RNG in numeric work).

## Invariants

1. The constructors — `FftPlan::new`, `RealFftPlan::new`, `FftBank::new` — are the only places
   that allocate. Nothing in any `process`/`inverse`/`levels`/`db`/`reset` does (the
   `hathor-dsp-core` rule applies: the 7 `AudioSection` tests in `tests/alloc_contract.rs`
   verify it, with two measure-mode controls proving the guard is not decoration).
2. `n` is a power of 2, guaranteed by the constructor (`Result` or a documented panic — a
   panic in setup is acceptable: it is a programmer bug, not a user bug).
3. Bins have index k = 0..=n/2 and frequency k·fs/n; no consumer recomputes this mapping on its
   own (helper `bin_freq(k, n, fs)` in the module).
4. The M1 bank is monotone: log-spaced bins fixed at compile time or setup, never reallocated
   per frame.
5. `process` never touches the dB buffer: dB materializes only in `render_db`, display-thread
   work — no `log10` inside a deadline (the law SPAN keeps too). `levels()` is the audio thread's
   product.
6. One normalization law across all tiers: `(4/N)²` — a full-scale sine reads 1.0 (0 dBFS) at
   every tier; and the axis mapping starts at k = 1 — DC is not audio, and four tiers would
   quadruple-count it.

## Rust concepts to put into practice

Pre-allocation as a way of life (`Vec` in `new`, slices in `process`) · the tuple `(f32, f32)`
vs a `Complex` struct (decision: a `Copy` struct `Complex{re,im}`, clearer; the layout is
identical to `[f32;2]` with `#[repr(C)]` if SIMD ever needs it) · dev-dependencies (`rustfft`
only under `[dev-dependencies]`: it does not enter the release binary) · feature-gating of the
M1 SIMD (`#[cfg(target_arch)]` as in `lanes8`).

## DSP concepts it embodies

[docs/it/dsp.md](../it/dsp.md) §6 in full: the fs/N resolution, leakage and windows, STFT and hop, dB per
bin. Plus: hermitian symmetry (why a real signal has half a spectrum), the packing trick (the
symmetry *used* to halve the work), and the reason the "right" analyzer is multi-rate: the
time-frequency constraint is not cheated, it is *architected*.

## Deferred (and why)

* **Inverse real-FFT** (hermitian unpacking in reverse): only needed by partitioned convolution
  (M4) and the phase vocoder (never, in this project). The forward covers the analyzer and
  correlation; writing the inverse now is unconsumed code = untested code = bug. It is written
  in M4 with its first consumer.
* **Radix-4 / split-radix**: ~15% fewer multiplications than radix-2 (expected). This was
  deferred *awaiting* measurement ("if radix-2 is enough for the bank (likely: the large FFTs
  run 12 times a second)") — the measurement has arrived: the whole bank costs **0.94% of one
  core** (measured, dev machine, release: `tests/bank_bench.rs`, the ≤8% gate asserted in both
  units). Saving 15% of 0.94% is noise, and radix-2's transparency is what makes the rustfft
  pin readable. Deferred **by measurement, not by waiting** — re-open when a profile, not a
  feeling, asks.
* **SIMD on the FFT**: butterflies vectorize well (4 AVX lanes). The number this item waited on
  exists now (bank: 0.94% of a core, measured — above) and it does not ask for vectorized
  butterflies. If the M2 analyzer UI moves the hot path, the number gets retaken. Optimizing
  before measuring remains the project's anti-thesis; measuring has now happened.

## Exercises (once the kernel exists)

1. Measure the time of `process` for n=4096 and compare it with the expected `n·log2(n)`: the
   implicit constant tells you how many ns per butterfly your machine does.
2. Print the spectrum (in dB) of a real windowed signal with and without Hann: count how many
   dB the first sidelobe drops. It should be ~−31 vs ~−13: that is the window buying truth.
3. Implement `bin_freq` and its inverse `freq_bin`, and use them to find at which bin 60 Hz
   lands with n=16384 at 48 kHz (exact: 60·16384/48000 = **20.48** — not an integer: this is
   why leakage exists. The earlier "20.5" in this doc was a rounding the test
   `bin_and_freq_agree_with_the_doc_number` now pins: `freq_bin(60, 16384, 48k) = 20.48 ±1e-4`.)
