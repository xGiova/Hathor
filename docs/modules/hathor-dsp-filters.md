# Module: `hathor-dsp-filters`

> Status: **written and tested** (M0, 27/27 green debug+release). Dependencies: `hathor-dsp-core`.

## Purpose

The biquad — the atom of EQ, crossover, key filter, phase rotator — done properly: correct
RBJ coefficients, a DF2T recursion stable in f32, coefficient ramping without zipper, and an
AVX2 kernel that exploits the only vectorization that exists (across channels, not in time).

## How it works

### `coeffs.rs` — filter design (setup time)

`design(kind, f0, sr) -> Result<Coeffs, FilterError>` implements the RBJ cookbook for
8 topologies. Choices that are worth more than the code:

1. **Design in f64, narrow only at `Raw::normalize()`.** At 10 Hz / 44.1 kHz `1 − cos(w0)`
   is worth ~1e-6 while the ulp of f32 near 1.0 is ~1.2e-7: an f32 design discards a
   significant digit *before* normalizing. The cost is setup-time, zero on the audio path;
   the frequency grid at 10 Hz is what proved the old f32 design wrong.
2. **`Raw{b0,b1,b2,a0,a1,a2}` → normalization in a single place.** The RBJ formulas produce
   `a0 ≠ 1` (for LP/HP/BP/notch/allpass `a0 = 1+α`; peaking `1+α/A`; shelf
   `(A+1) ± (A−1)·cos(w0) + 2·√A·α`). Forgetting the division by `a0` yields a filter that
   *works* but has the wrong out-of-band gain — an EQ that adds gain at every band. The
   first draft had exactly that bug (normalization forgotten in the peaking). Now there is
   `Raw::normalize()`, the single place where `a0` disappears, and the test
   `lp_normalization_gives_unity_dc_gain` nails it.
3. **`force_dc_unity()` — share the cancellation, don't fight it.** The only post-normalize
   step, private, applied only to the kinds designed for unity at DC (LP/Notch/AllPass/Peak;
   never shelves, whose low-end gain *is* the design, never HP/BP, which are 0 there).
   `response(0)` computes `1 + a1 + a2` with catastrophic cancellation (terms ~2 cancelling
   to ~1e-6 below 50 Hz), leaving ~0.1 dB of ghost DC gain in the f32 representation. The
   error cannot be corrected, only *shared*: rescale the `b` by `den/num` so their sum rides
   the same dirty `den` and the ratio is exactly 1.0 — the unity-DC trick of commercial EQs.
4. **Refusal instead of repair.** `f0 ≥ Nyquist`, `q ≤ 0`, `NaN` → `Err`; poles outside the
   unit circle after normalization → `Err(Unstable)`; a shelf gain/slope combination with no
   real solution → `Err(InvalidSlope)` (the story below). Delivering such a filter is a
   crackle that arrives *minutes after* the move: better to fail at setup, where the caller
   can decide.
5. **`response(w) -> (re, im)` is public API**, not just `magnitude_db`. Phase is the
   kick↔bass plugin's currency ([docs/it/dsp.md](../it/dsp.md) §5): every consumer (the behavioural
   test, the phase-rotator of M2, an allpass cascade) needs the complex number, not the dB.

> **The story of the shelf `2·√A·α` bug** (same lesson as `−a2·out`, one class later): the
> denominator terms used `2·α` instead of RBJ's `2·√A·α`, so the poles stopped mirroring the
> zeros — a subtly wrong out-of-band curve at moderate gain, and "Unstable" at large gain.
> Caught by the regression grid `large_gain_shelves_stay_stable_and_finite`. The adjacent
> edge is a refusal, not a fix: at `S = 2` beyond ~±24 dB the argument of α is negative,
> **no real filter exists** for that combination, and clamping α→0 would park a pole exactly
> on the unit circle — hence `Err(InvalidSlope)`
> (`shelf_slope_too_steep_for_gain_is_rejected`).

### `biquad.rs` — the recursion (processing time)

**DF2T (Direct Form II Transposed)**:

```
out = b0·x + z1
z1  = b1·x − a1·out + z2
z2  = b2·x − a2·out      ← the term forgotten in the first draft
```

> **The story of the `−a2·out` bug** (read it, it is worth more than ten tutorials): the
> first implementation had `z2 = b2·x`. The resulting filter: stable, magnitude response
> *almost* right, **phase wrong**. The test against the f64 reference did not catch it
> because I wrote the reference, with the same error: two copies of the same lie confirm each
> other. Only the **behavioural test** catches it
> (`measured_response_matches_predicted_response`): sine in → amplitude and phase measured by
> correlation → compared against `Coeffs::response` (an algebraic path independent of the
> recursion). Project rule: *every DSP module has at least one test comparing the external
> behaviour with a prediction independent of the implementation.*

**Coefficient ramping** (`ramp_to(target, frames)`): a linear per-frame delta over the 5
coefficients, closing **exactly** on the stored `target` (not on the sum of the deltas: the
f32 accumulation would leave a permanent residue = an EQ out of tune forever). Recomputing
coefficients per frame would drag `sin/cos/sqrt` into the callback: ~50 ns/frame (expected)
→ ~2.5–4 µs per filter per 64-frame block ≈ 0.2–0.3% of the 1333 µs block budget — small for
one filter, but it multiplies across bands × channels; ramping costs 5 FMA per frame. A ramp
interrupted halfway restarts from the current value (test
`interrupted_ramp_restarts_from_current_coefficients`).

**`BiquadNode`**: the `Node` wrapper. It reads the parameters at block start (`dirty` flag),
redesigns once and ramps across the block. `set_param` with the same value does not mark
dirty: the UI can pump identical events at no cost.

### `lanes8` (x86-64) — AVX2, 8 channels in the 8 lanes

The recursion is sequential *per channel*: sample `n+1` depends on `out(n)`. Therefore:

* ❌ vectorizing 8 consecutive samples of the same channel — impossible (dependency).
* ✅ vectorizing **8 channels at the same sample** — 8 independent recursions, one `__m256`
  per state (`z1`, `z2`), 4 FMA + 1 MUL per frame.

The price: the layout must be **interleaved-by-channel** (the 8 values of the same frame
contiguous), while the engine is planar. `planar_to_interleaved`/`interleaved_to_planar`
exist for that, and `hathor-audio-probe` measures the transpose cost *separately* from the kernel:
the honest number is `kernel + transpose`, not `kernel`.

`per_lane(&[Coeffs; 8])`: **different coefficients per lane** — 8 different sections of the
same signal. It is the shape the multiband (M4) and the 8-band phase rotator (M2) will need:
8 allpass in "parallel" cascade at a cost close to 1.

**Why `#[target_feature]` + `is_x86_feature_detected!` + explicit `unsafe` on
load/store**: see [../it/rust.md](../it/rust.md) §8. The `unsafe_op_in_unsafe_fn` lint is not
pedantry: forcing every unsafe to be encapsulated with a SAFETY comment means forcing a
line-by-line reread — and it was in that reread that the `−a2·out` bug surfaced.

## Invariants

1. `design` never returns unstable or `NaN` coefficients (test over a freq×Q×samplerate
   grid). The grid skips `f < sr·1e-4` with a documented carve-out: at those subsonic ratios
   the stability margin is O(w0²) — smaller than the coefficient ulp — so `is_stable()`
   would judge the rounding, not the physics (TD-18).
2. One `Biquad` processes **one channel**. State per channel, always (`process_channels` with
   one filter per lane). "A stereo biquad" with shared state is a mono filter with two
   inputs: in the kick↔bass plugin's correlation tests it would be a false negative.
3. After a completed ramp, `coeffs() == target` **exactly** (bit-exact), not "roughly".
4. The `lanes8` kernel and the scalar version converge within 2e-6 but are not bit-exact: FMA
   rounds once, mul+add twice. The SIMD is *more precise*; a test demanding bit-exact
   identity is testing the wrong thing.
5. `latency_samples() == 0` for every minimum-phase IIR. The group delay (fractional,
   f-dependent) is another quantity and lives in `hathor-dsp-measure`.

## Rust concepts embodied

`unsafe` confined and annotated (§8 of [docs/it/rust.md](../it/rust.md)) · `#[cfg(target_arch)]` for the SIMD
code · runtime feature detection · const-fn and `Copy` for `Coeffs` · matching on structural
constants (`ParamId`) · `#[deny]` lints as an automatic-review tool.

## DSP concepts embodied

All of [docs/it/dsp.md](../it/dsp.md) §3-5: RBJ and a0 normalization · pole/zero stability · DF1 vs DF2T ·
zipper noise and coefficient ramping · "recursive filters do not vectorize in time" ·
min-phase vs group delay · phase as a first-class quantity (`response`).

## Deferred (and why)

* **Linkwitz-Riley crossover / 4-pole Butterworth** → M2: the multiband and the phase rotator
  need them; they are biquad cascades, so this module is the prerequisite, not the place.
* **SVF (state-variable) and ladder** → M2/M8: the SVF is the right form for cutoff modulated
  at sample rate (synth); here the cutoff changes at block rate, and DF2T is enough.
* **Linear-phase FIR** → M2 (EQ with a linear-phase option): it is a different module
  (`hathor-dsp-time`, it needs the delay line), not an extension of this one.
* **Per-sample coefficient ramping** (audio-rate cutoff modulation) → M8 with the synth: it
  costs 5 FMA/sample instead of 5/block; it will be enabled only where needed.

## Exercises

1. Print the phase response (`phase_rad`) of an LP Butterworth at 1 kHz between 10 Hz and
   10 kHz. Where is it −90°? And −180°? (By hand first, then verify: it is
   [docs/it/dsp.md](../it/dsp.md) §5 with your own hands.)
2. Implement `cascade(a: &Coeffs, b: &Coeffs) -> Coeffs` (polynomial convolution, order 4) and
   verify that the cascade's `magnitude_db` = the sum of the `magnitude_db`. It is the exercise
   that makes you understand why an 8-band EQ is 8 biquads *in series* and not a single
   polynomial (hint: stability and independent parametrization).
3. Extend `per_lane` to 4 lanes (SSE, `__m128`) and measure: on this CPU (native AVX2) does it
   make sense? When does it? (hint: SSE2-only machines, and nodes with 2-4 bands.)
