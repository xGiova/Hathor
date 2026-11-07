# Module: `hathor-dsp-measure`

> Status: **written, tested 75** — M0 primitives, **M2 phase vector** (landed 2026-11-07)
> and M6 level tracking; the masking meter itself landed **in `hathor-dsp-fft`** (`mask.rs`,
> layered on the bank — §M3 below keeps the contract and points there). Dependencies:
> `hathor-dsp-core`, and **`hathor-dsp-fft` is now a product dependency**: the phase vector
> delegates to the frozen `RealFftPlan` kernel — it arrived with its first consumer, not
> sooner, as this header always promised. `hathor-dsp-filters` exports `coeffs::db_to_lin`
> with a **design-oriented clamp**; this crate deliberately carries its own **public meter
> pair** (`db::lin_to_db`/`db::db_to_lin`, silence → `f32::NEG_INFINITY`): TD-22, two
> contracts on purpose.

## Purpose

The **measurements**: everything that turns two audio signals into numbers you can decide on.
It is the module the kick↔bass plugin consults constantly ("are they aligned? by how much?
where do they mask each other?"), and it is also the acceptance bench for the plugins (M4):
the same functions that answer the user answer the tests.

Design principle: **pure functions, no allocation per call, results in small preallocated
structures**. It is not a "meter plugin": it is a library of measurements that the meters (M1)
and the kick↔bass plugin (M3) consume.

## How it works

### M0 — the primitives (few, useful immediately)

```rust
pub fn peak_db(block: &Block) -> f32;                    // max|x| → dBFS
pub fn rms_db(block: &Block) -> f32;                     // square root of the mean of squares → dBFS
pub fn correlation(a: &[f32], b: &[f32]) -> f32;         // normalized Pearson, −1..+1
pub fn time_align_lag(a: &[f32], b: &[f32], max_lag: i32) -> i32;  // optimal lag in samples
pub fn null_residue_db(a: &[f32], b: &[f32], gain_db: f32, invert: bool) -> f32;
```

* **`correlation`**: `Σ(a·b) / sqrt(Σa²·Σb²)`. It is the number behind the stereo meter ("how
  much in phase are L and R") and, between kick and bass, "how good the phase is wideband".
  Interpretation caveat: −1 is not "wrong", it is *perfect anti-phase* — for the null test it
  is the greatest success.
* **`time_align_lag`** (M0, naive version): cross-correlation for every candidate lag in
  `−max_lag..=max_lag`, the maximum wins. Cost O(n·2·max_lag): with n=4096 and max_lag=±256
  that is ~2M multiplications (expected) — acceptable **offline** or at UI rate (once per
  analysis, not per audio block). In M3 it moves to the FFT (`corr = IFFT(FFT(a)·conj(FFT(b)))`,
  O(n·log n)) once `hathor-dsp-fft` has the real inverse. The naive version stays as the reference in
  the fast version's tests — same lesson as the `−a2·out`: two independent paths.
* **`null_residue_db`**: sums `a` with `b` scaled/inverted and measures the residual peak in
  dBFS. It is the kick↔bass plugin's acceptance ("null residue < −30 dB in 40-120 Hz") and the
  transparency test of *any* processor ("did I touch nothing?"). Per-band variant: pre-filter
  with `hathor-dsp-filters` (band 40-120 Hz) before the null — a **consumer-side composition**
  (the kick↔bass plugin owns the filters dependency; this crate does not).

### M2 — the phase vector (live API, landed 2026-11-07, `phase_vector.rs`)

The kick↔bass tooling's currency — per-bin phase and **group delay in samples** — was a
sketch until today; the live shape, with its two-rate split:

```rust
let mut pv = PhaseVector::new(n);           // setup: buffers sized once, declared maximum
pv.analyze(&frame)?;                        // hot-kernel rate: delegates to the frozen
                                            // `RealFftPlan` — zero allocation, ZERO
                                            // transcendentals
pv.extract_phases();                        // UI-rate by contract: where atan2 lives honestly
// pv.phases()/unwrapped()/group_delay()   // group delay −dφ/dω in samples
```

* **The split is the doctrine, not an optimization detail**: `analyze` is the only
  callback-shaped part; `extract_phases` runs the `atan2`s at UI rate — the same
  `process`/`render_db` division the fft crate makes, restated here so a measurement never
  smuggles transcendentals into a deadline.
* **Correctness is pinned against closed form**: a pure delay `D` feeds the bins
  `φ_k = −2πkD/N`, and `group_delay()` measures **exactly D on every interior bin**
  (forward differences `(φ_{k+1}−φ_k)·(−N/2π)`, backward at the last interior). Edges are
  **documented exclusion, not silent garbage**: DC and Nyquist are purely real by the
  unpacking's construction (the fft's `real.rs` header makes that zero-imaginary crux the
  reason they carry **no phase to differentiate**) — `group_delay()` reports `NAN` at bins
  `0` and `N/2`, with the closed-form argument in the header.
* **The unwrap convention, in words before code**: `atan2` answers in [−π, +π]; unwrapping
  walks the **bins** (frequency axis — what `dφ/dω` needs) and adds the multiple of 2π that
  keeps each step in `(−π, π]`, the exact `+π` boundary counted as `+π` — a **total**
  convention, and the impulse-ramp test reproduces it bit for bit. This is *not* time-axis
  unwrapping of successive frames (a phase-vocoder concern with no consumer; not guessed
  here).
* **`phase::block_null_residue_db(a, b, gain_db, invert) -> f32`** — the M3 acceptance shape
  (kick↔bass "null residue < −30 dB") as a whole-block, multi-channel test: it returns the
  **WORST channel's** residue, never a mean. The reason is acceptance-honesty: a 1% leak
  hiding in R must not disappear behind L's perfect −∞ — the test
  `block_null_reports_the_worst_channel_not_an_average` pins exactly that (`−40.000 dB`,
  `20·log10 0.01`). No surviving channels / matched silence
  → `NEG_INFINITY` (honest silence, invariant 2); a `NaN` sample propagates — a broken
  signal never decays into a reassuring number.

### M3 — the masking meter

> **Landing note (2026-11-07):** the meter shipped as `mask.rs` in `hathor-dsp-fft` — layered
> on the bank's shared log axis, never owning a bank; its model (asymmetric exponential
> skirts, power-add, flat AT from the floor, five views) and its declared status — a
> first-class approximation, **not** a psychoacoustic standard, acceptance = agreement with
> listening in M3 — are documented in [hathor-dsp-fft.md](hathor-dsp-fft.md) §M2. The sketch
> below is the contract's first draft, preserved as history; what survives unchanged are the
> questions and the double-buffered UI shape.

The "where they step on each other" between kick and bass:

1. STFT of both signals (`RealFftPlan` of `hathor-dsp-fft`, size ~2048, Hann, hop 512) —
   **not** the multi-rate display bank: two different jobs. A measurement wants uniform
   resolution and a free hop; the bank is display architecture with normative tiers
   (256/1024/4096/16384, hop fixed at N/4) — there is no 2048 tier there to request, and
   asking for one would be the wrong instinct anyway.
2. Per bin: `masking(k) = min(L_kick(k), L_bass(k)) · spread(k)` — the energy of the weaker one
   *under* the louder one is wasted/covered energy; `spread` is the widening of the masking
   towards neighbouring bins (a known psychoacoustic curve: ~25 dB/decade towards the highs,
   asymmetric).
3. Integration into third-octave bands → a 2D heatmap (time × frequency) for the UI.
4. Summed with the number that counts: **recoverable headroom in dB** = how much you gain by
   carving the bass EQ where the kick masks (and vice versa). It is the "≥1.5 dB measured" of
   the M3 acceptance.

The returned structure is preallocated (`MaskingFrame { bands: [f32; N_BANDS], ... }`): the UI
copies it off the audio thread with a double buffer (`Arc` swap, architecture §2), never a
`Vec` allocated per analysis frame.

### M6 — level tracking for the calibration contract (landed, tested — the primitive came early)

The mixer's "analog sweet-spot" guide is measurement, so it lives here, not in a UI crate
(policy and ownership: [hathor-calibration-variance.md](hathor-calibration-variance.md) §1 and
§3 — the band and its `G_cal` are the strip's contract, the honest dBFS is this crate's):

```rust
pub struct RmsTracker { /* mean_square + coef + tau_s — Copy, zero heap */ }
impl RmsTracker {
    pub fn new(tau_s: f32) -> Self;                           // UI thread; conservative default coef
    pub fn set_time(&mut self, sample_rate: u32, tau_s: f32); // in setup(); NOT per block (it is an exp)
    pub fn process_block(&mut self, block: &Block<'_>);       // audio rate: zero alloc, ~2 FMA/frame (expected)
    pub fn rms_db(&self) -> f32;                              // read at UI rate (atomics/double buffer)
}
```

* **Why `new(tau_s)` alone is not enough** — the trap the original sketch hid: the coefficient
  is `1 − exp(−1/(τ·fs))` and cannot exist before the sample rate does. `new` starts at
  `SmoothedParam`'s conservative 1e-3 default — a meter silently running on a guessed time
  constant is the bug class, so `set_time` in `setup()` is the contract, and
  `default_coef_is_conservative_until_setup` pins the honest slow start (the first block of
  full scale reads −12 dB, not 0).
* **One-pole on mean-square, stepping once per frame on the channel-mean energy**
  (`Σ_c x_c²/channels`), τ semantics shared with `SmoothedParam` (`τ = time_s`, 63 % in one τ —
  both sides measured: rise to −1.99 dB, decay by −4.34 dB after exactly one τ): the
  block-rate `rms_db(block)` stays exactly what it is (per-block truth), the tracker adds the
  *running* window the human eye needs. Two numbers, two questions — same doctrine as
  correlation vs optimal lag below.
* **The regression test is this block's best documentation**: the first implementation stepped
  the state sequentially in *channel order*, so with a full-scale ch0 and a silent ch1 the
  silent last channel simply **won** — the meter read −57.9 dB where the truth was −3.01 dBFS
  (frame energy `(1+0)/2`). `stereo_energy_averages_channels_not_the_last_one_wins` nailed the
  channel-mean fix in place, and its long-run average *is* `rms_db`'s all-samples mean: one
  doctrine, two windows. A `NaN` sample poisons the meter permanently and stays visible —
  a broken signal must never decay into a reassuring silence.
* **Calibration metering** is a consumer-side classification, not new math: the UI maps
  `rms_db` onto the sweet-spot band `reference_dbfs ± window` (−20…−14 dBFS by default).
  The band lives in the channel-strip contract (that doc §3); this crate only supplies honest
  dBFS, invariant 2 keeps that reference ±1.0 and silence = `f32::NEG_INFINITY` —
  `silence_keeps_silence` guards the muted-track case against ever "warming up".
* **The sweet-spot metering primitive, pinned to its contract** (2026-11-07):
  `RmsTracker::sweet_spot()` is exactly `new(SWEET_SPOT_TAU_S)` with
  `pub const SWEET_SPOT_TAU_S: f32 = 0.300` — the **300 ms window lifted verbatim from
  [hathor-calibration-variance.md](hathor-calibration-variance.md) §3**, so the guide the eye
  reads and the contract the strip promises share one number by constant, not by comment
  (`sweet_spot_carries_the_300ms_window_contract` asserts `tau == 0.300`). Publishing is
  lock-free: `publish_to(&AtomicParam)` moves the value through atomics (architecture §2),
  never a mutex — fresh/poisoned/level states all pinned by tests.

> **House lesson, from this crate's gate tests:** an assertion that *fails inside*
> `AudioSection` is a **double panic** — the guard's own panic escapes `catch_unwind` on the
> release `nounwind` Box path and the abort **swallows the message**: the test dies with no
> verdict (proved in `crates/hathor-dsp-core/examples/guard_probe.rs`, re-proved in
> `measure/src/lib.rs`). The recipe is now law: **values measured inside the section are
> captured and asserted OUTSIDE it.** The gate is a scope; the verdict comes after.

## Invariants

1. No function allocates per call. Analysis buffers: either a slice owned by the caller, or
   structures created in `new(n)` with a declared maximum n.
2. All dB measurements are dBFS with reference ±1.0 = 0 dBFS, and `−inf` is silence (never −999
   or other magic constants: `f32::NEG_INFINITY` behaves correctly with `max`).
3. `correlation` on silent signals (Σa² = 0) returns 0.0, not NaN: the "no signal" case is
   legal and frequent (a muted track), not an error.
4. `time_align_lag` returns the lag in **samples**, positive = `b` delayed relative to `a`. The
   convention is documented here and is not renegotiated: a wrong sign in an alignment tool
   moves in the opposite direction, and the user "fixes" it while making things worse.
5. Every measurement has a test against a synthetic signal with known answer: a sine delayed by
   k samples → `time_align_lag` must return exactly k (not "approximately").

## Rust concepts to put into practice

Slices in, small `Copy` structs out · `Result` only where the input can be *illegal* (not where
it is merely empty: see invariant 3) · double buffer for the UI (M3) with an `Arc<[T]>` swap ·
dev-dependency `rustfft` for the fast-correlation tests.

## DSP concepts it embodies

[docs/it/dsp.md](../it/dsp.md) §5 (correlation = phase wideband; the null test = the most honest
measurement) and §6 (the STFT for masking). Plus: the difference between *similarity*
(correlation) and *alignment* (optimal lag) — two numbers answering two different questions, and
the kick↔bass plugin's UI shows both.

## Deferred (and why)

* **FFT cross-correlation** → M3: it needs the real inverse of `hathor-dsp-fft`, which arrives with
  the convolution. The naive one is enough for M0/M1 and stays as a reference forever.
* **Loudness BS.1770** → M4: that is mastering metering, consumer = `hathor-dsp-measure` but priority =
  the M4 suite. The structure (K-weighting filters in `hathor-dsp-filters` + gating + integration) is
  already mapped in the ROADMAP.
* **Transient detection** → M3, inside the kick↔bass plugin: the nudge "to the transient" is needed
  there; writing it earlier without its consumer (the nudge UI) means guessing the API.
* **Read-only `View` meter variants** → M1: the shipped meters borrow `&Block`, the writable
  view of the realtime contract; read-only consumers (a `ChannelBuffer::view()` on the UI side)
  may want `View` variants of the same functions. None invented today — no consumer: the API
  arrives with its first user, not with a guess.
* **Sweet-spot classification** (is this reading "in the analog window"?) → M6: the math is
  `RmsTracker` + a comparison with the strip's `reference_dbfs`; the band itself belongs to the
  channel-strip contract ([hathor-calibration-variance.md](hathor-calibration-variance.md)),
  not to a measurement library. Writing it into this API before its consumer exists = guessing.

## Exercises (once it exists)

1. Generate two copies of an impulse delayed by 7 samples: `time_align_lag` must return exactly
   7. Then add noise at −20 dB and repeat: at what SNR does the measurement go off by ±1? (It
   is the question that decides whether the kick↔bass plugin's automatic nudge can be trusted.)
2. Build two 50 Hz sinusoids in opposite phase: correlation = −1, null residue = −inf dB. Then
   delay one by 1 ms (18° at 50 Hz): the correlation rises to **−0.951** — cos 162°, still
   anti-phase, and the sign is the lesson (the doc once said "+0.95": reading the magnitude
   without the sign is exactly the UI mistake this module exists to prevent) — while the null
   residue rises to an exact `2·sin(9°)` ≈ **−10.09 dBFS** (the earlier "~−18 dB" was wrong
   arithmetic, not a rounding: the shipped `db::lin_to_db` states it honestly). These
   are the numbers the plugin's user must *see* — understanding them here means you can
   design the UI later.
3. Prove to yourself why `Σ(a·b)/sqrt(Σa²Σb²)` is gain-invariant: double `b` and recompute.
   Then explain why a correlation meter cannot distinguish "the same signal at a different
   volume" from "the same signal" — and why that is fine.
