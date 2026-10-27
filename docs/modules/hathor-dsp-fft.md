# Module: `hathor-dsp-fft`

> Status: **written, tested 29** (kernels landed; the multi-rate bank is M1). Dependencies:
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

Each size: an accumulation ring buffer (hop = N/4), a precomputed Hann window, the FFT,
magnitude² → mapped onto ~1024 shared log-spaced bins → smoothing (peak-hold with decay + a
moving average per bin). It is the architecture that gives SPAN its look: reactive at the top,
detailed at the bottom. The junction between sizes must be continuous in dB: the boundary bins
are compared against a known signal in the tests.

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

## Invariants

1. `FftPlan::new` is the only place that allocates. `process` never does (the `hathor-dsp-core` rule
   applies: the test with `AudioSection` verifies it, as for every module).
2. `n` is a power of 2, guaranteed by the constructor (`Result` or a documented panic — a
   panic in setup is acceptable: it is a programmer bug, not a user bug).
3. Bins have index k = 0..=n/2 and frequency k·fs/n; no consumer recomputes this mapping on its
   own (helper `bin_freq(k, n, fs)` in the module).
4. The M1 bank is monotone: log-spaced bins fixed at compile time or setup, never reallocated
   per frame.

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
* **Radix-4 / split-radix**: ~15% fewer multiplications than radix-2 (expected). Deferred until after the
  M0 benchmark: if radix-2 is enough for the bank (likely: the large FFTs run 12 times a
  second), the complexity is not justified.
* **SIMD on the FFT**: butterflies vectorize well (4 AVX lanes), but first the real cost of the
  bank is measured in `hathor-audio-probe`. Optimizing before measuring is the project's anti-thesis.

## Exercises (once the kernel exists)

1. Measure the time of `process` for n=4096 and compare it with the expected `n·log2(n)`: the
   implicit constant tells you how many ns per butterfly your machine does.
2. Print the spectrum (in dB) of a real windowed signal with and without Hann: count how many
   dB the first sidelobe drops. It should be ~−31 vs ~−13: that is the window buying truth.
3. Implement `bin_freq` and its inverse `freq_bin`, and use them to find at which bin 60 Hz
   lands with n=16384 at 48 kHz (exact: 60·16384/48000 = **20.48** — not an integer: this is
   why leakage exists. The earlier "20.5" in this doc was a rounding the test
   `bin_and_freq_agree_with_the_doc_number` now pins: `freq_bin(60, 16384, 48k) = 20.48 ±1e-4`.)
