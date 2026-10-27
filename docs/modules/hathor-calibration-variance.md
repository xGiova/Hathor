# Feature: channel-strip gain staging (G_cal) & analog channel variance

> Status: **specification** (contract lands with the M6 mixer; metering primitives can ride
> `hathor-dsp-measure` earlier). This is not a crate of its own: the policy lives in the
> `hathor-host-graph` channel strip, the measurement in `hathor-dsp-measure`, the
> level-sensitive consumers (saturator M3/M4, NAM M6.5) downstream. Expected dependencies:
> `hathor-dsp-core` (ramps), `hathor-host-graph` (ownership), `hathor-dsp-measure` (metering).
> NAM licensing/inference research: [hathor-nam.md](hathor-nam.md).

## Purpose

Two separable promises, one document because both live in the same place (the channel strip):

1. **Gain-staging contract (`G_cal`).** Every strip applies one deterministic gain multiply
   between the incoming buffer and the insert chain, so that *level-sensitive* processors —
   saturators, compressors with drive, and amplitude-sensitive NN models (the NAM case that
   motivated this, [hathor-nam.md](hathor-nam.md)) — see a stream whose absolute amplitude has
   a **defined meaning**: the emulated hardware's nominal 0 dBu sits at a configurable
   `reference_dbfs` (default −18 dBFS, window −20…−14 dBFS) inside the floating-point
   pipeline. Linear processors are indifferent to it; that indifference is exactly what makes
   the contract free to hold.
2. **Analog channel variance.** Identical settings on two strips must not produce a
   bit-identical signal path: a per-channel, *seeded-deterministic* drift vector perturbs
   filter cutoffs by a fraction of a percent and the input bias by well under 1 dB, to break
   the "digital phase alignment" of multi-tracked parts. Determinism comes from a hash of
   `(channel_id, console_seed)` — not from RNG state — so a channel always sounds like itself
   across project reloads, like fixed component values.

Neither promise is NAM-specific. The NAM node is simply the first consumer whose behavior
*requires* the contract; the mixer-wide contract exists even if no neural model is ever
loaded.

## How it works

### 1. `G_cal` — one multiply at strip input, owned by the channel strip

```rust
/// Project-level calibration reference (serialized with the project, UI-rate).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalibrationRef {
    /// dBFS where the emulated hardware's nominal 0 dBu lives inside the float
    /// pipeline. Valid window −20.0..=−14.0; default −18.0. Refused at the door
    /// otherwise, same philosophy as `hathor_dsp_filters::design` refusing unstable
    /// designs.
    pub reference_dbfs: f32,
}

/// The strip's input stage: coefficient computed at UI/setup rate, ramped at frame rate.
pub struct GainStagingNode {
    g: hathor_dsp_core::param::SmoothedParam, // one-pole, τ = time_s semantics per its doc in hathor-dsp-core
}

impl GainStagingNode {
    /// UI/setup-rate math — `powf` here is fine: this is *not* the audio path.
    pub fn set_target(&mut self, cal: CalibrationRef, trim_db: f32, drift_db: f32) {
        // G = 10^((reference_dbfs + trim_db + drift_db) / 20)
        self.g.set_target(10.0f32.powf((cal.reference_dbfs + trim_db + drift_db) / 20.0));
    }
}
```

The transcendentals live at UI/setup rate — the same rule
[hathor-dsp-filters.md](hathor-dsp-filters.md) encodes for biquad designs: `G` is computed
when the user touches a knob or a project loads, handed to a `SmoothedParam`, and ramped per
frame inside `process()`, where the only remaining cost is the multiply.

* **Why it is nearly free.** Coefficients/gains change at setup/param rate, not at frame
  rate — the same rule [hathor-dsp-filters.md](hathor-dsp-filters.md) encodes for
  biquad designs (`design()` at setup, DF2T + coefficient ramps at runtime). The audio path
  is one multiply per frame per channel with a ramped coefficient. No percentage of the
  1333 µs/64-frame budget ([architecture.md](../architecture.md) §2) is claimed before the
  M0 bench exists; it is the cheapest operation the engine performs — this is a structural
  statement, not a measured one.
* **Zero latency by construction**: a memoryless multiply declares `latency_samples() = 0`
  and tells no lies (architecture §4: declared latency ≠ group delay — there is neither here).
* **One home for the scale factor.** The strip applies `G_cal` *once*, at chain input. No
  insert — native node, CLAP, or VST3 through the bridge — may apply a second
  console-scale factor; a processor that needs its own input trim (NAM's "level match",
  saturator drive) has its own parameter, *downstream* of `G_cal`. The [pasted external
   spec](../archive/nam-gain-staging-spec-2026-10-27.md) (§6–7, archived verbatim in-repo)
  fused model-matching gain into the console gain; the contract deliberately un-fuses them,
  because a per-model correction must travel with the model slot, not with the strip.
* **NAM metadata seeding** (planned, consumer = the NAM node, not the strip): `.nam` files
  carry gain/loudness metadata (keys verified in
  [exportable.py](https://github.com/sdatkinson/neural-amp-modeler/blob/main/nam/models/exportable.py);
  exact semantics NOT VERIFIED) — the NAM node can seed its *own* input trim from it. Never
  folded into `G_cal`.

### 2. Channel variance — a drift vector from a hash, applied to PARAMETERS

```rust
/// Deterministic per-channel drift. `Copy`, serialized into the project file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelDrift {
    /// Relative cutoff perturbation for the strip's filters: −0.02..=+0.02 (i.e. ±2 %).
    pub eq_freq_drift: f32,
    /// Bias perturbation of the calibrated input, in dB. The source spec is inconsistent
    /// (struct: ±0.005 linear = ±0.043 dB; prose: "±0.3 dB") — DECIDED 2026-10-27: the
    /// shipped envelope is the conservative ±0.005 linear (≈±0.043 dB); ±0.3 dB remains
    /// the maximum project-configurable ceiling. The discrepancy is documented, not erased.
    pub gain_bias_db: f32,
}

pub struct AnalogChannelVariance {
    channel_id: u32,     // stable handle, NEVER an index (see Invariants)
    console_seed: u64,   // project-level seed, serialized with the project
    drift: ChannelDrift, // computed at track creation AND stored (so manual override sticks)
}

impl AnalogChannelVariance {
    /// Track-creation time (control thread). Not the audio path: hashing there is trivial.
    pub fn new(channel_id: u32, console_seed: u64) -> Self {
        // splitmix64 finalizer: no dependency, no RNG state, reproducible forever.
        let h0 = mix64(console_seed ^ (u64::from(channel_id).wrapping_mul(0x9E37_79B9_7F4A_7C15)));
        let h1 = mix64(h0 ^ 0x5555_5555_5555_5555);
        Self {
            channel_id,
            console_seed,
            drift: ChannelDrift {
                eq_freq_drift: map_unit(h0, -0.02, 0.02),
                gain_bias_db: map_unit(h1, -0.043, 0.043), // ≈±0.005 linear, the shipped
                                                            // envelope; ±0.3 dB is the
                                                            // project-configurable ceiling
            },
        }
    }
    /// The drifted cutoff the strip hands to `hathor_dsp_filters::design`:
    pub fn drift_freq(&self, f0_hz: f32) -> f32 {
        f0_hz * (1.0 + self.drift.eq_freq_drift)
    }
}
```

* **Where it hooks in, and where it must never**: the drift is applied to *parameters*
  upstream of `design()` (the strip drifts the f0/Q it passes to
  [hathor-dsp-filters](hathor-dsp-filters.md)) and to the strip's `G_cal` (`gain_bias_db`).
  The filter crate stays exact — its stability/normalization test grid assumes exact designs
  (invariant 1 of that doc), and L2 must not know what a channel is
  ([architecture.md](../architecture.md) §1: "If you find yourself passing a `TrackId` into
  `hathor-dsp-filters`, you have broken a layer"). Variance is an L4/host policy.
* **Determinism across reloads comes from hashing, not RNG state** — the external blueprint's
  `low_overhead_hash(channel_id)` with the project seed replacing `0x55555555` as the second
  mixing constant. CONVENTION §4.4's deterministic-RNG rule (`Lcg`) is for *tests*; the drift
  is data, not randomness: it is computed once, persisted, and never re-drawn.
* **Honest note on the source numbers**: the [archived
  spec](../archive/nam-gain-staging-spec-2026-10-27.md) struct says `nam_bias_offset ±0.005`
  (linear ⇒ ±0.043 dB) while its prose says "a variance of just ±0.3 dB". The two disagree by
  ~7×. Decided 2026-10-27 (user): the **shipped** envelope is the conservative ±0.005 linear
  (≈±0.043 dB); the prose ±0.3 dB stays as the maximum project-configurable ceiling. The
  discrepancy is recorded here instead of being silently resolved.
* **Persistence model**: both the seed and the computed values are serialized. Reload
  regenerates from `(channel_id, console_seed)`; stored values win when the user overrode a
  strip's drift by hand. Undo (M5) treats "re-roll console seed" as one command.

### 3. Metering — the 300 ms sweet-spot guide (belongs to `hathor-dsp-measure`)

The running RMS over ~300 ms that "guides the user visually toward the analog sweet spot" is
a `hathor-dsp-measure` planned API (already folded into
[hathor-dsp-measure.md](hathor-dsp-measure.md)), consumed by mixer strips at UI rate:

* steady-state read against `reference_dbfs`: the strip meter marks the −20…−14 dBFS band;
* the tracker is one-pole-on-mean-square (same smoothing family as `SmoothedParam`,
  τ = 300 ms semantics per its doc), ~2 FMA per frame per channel at block rate
  (**expected** — structural count, not a measurement), zero allocation, publishes to the UI
  through the usual atomics (architecture §2), never a second buffer.

### 4. Post-NAM / post-drive auto-makeup gain

"Because driving a model harder compresses perceived dynamics, stabilize output level." Two
modes, deliberately separated by their latency consequences:

| Mode | Mechanism | Latency | Lands |
|---|---|---|---|
| **Instantaneous** (default) | peak-tracking makeup: gain = target_peak / running_peak, clamped, released slowly | 0 samples, declared | M6 (strip utility, on top of M4 envelope code) |
| **Look-ahead** ("decoupled": detector reads the *unprocessed* path, main path is delayed) | the M4 compressor/limiter envelope machinery reused | `max_latency_samples` = look-ahead length, **declared** | gated on M5 PDC being real (architecture §4): the **host engine owns compensation**, the strip only declares; exposing it in the mixer is M6 work |

Who owns latency compensation: `hathor-host-graph` / the M5 engine (path sums,
architecture §4) — the strip does not "fix" timing itself. A look-ahead node whose latency the
engine failed to compensate shifts the track by the look-ahead length; that is exactly the
class of lie the declared-latency rule exists to prevent, so enabling look-ahead makeup is a
**gate item**: accepted only with the 40-track PDC test of M5 passing with it on one strip.

### 5. The honest trade-off: variance breaks inter-channel phase coherence

Two tracks recorded through identical settings *no longer null perfectly*: their HPF/LPF
cutoffs differ by ±2 %, so their phase curves differ (small, frequency-dependent, real). This
is the point for multi-tracked parts — and it is a **responsibility**:

* the kick↔bass phase-alignment tool (M3) and the null test in
  [hathor-dsp-measure.md](hathor-dsp-measure.md) **must read the drift vector of every
  involved strip and display it** ("drift on: L ±x.x % / −y.y dB"), so a −26 dB null with
  drift on is never mistaken for a −∞ dB null with drift off;
* the null-residue acceptance numbers (M3's "residue below −30 dB") are defined **with drift
  off** unless a test explicitly opts in;
* a master switch (project-level, **default OFF, opt-in with a console seed** — decided
  2026-10-27) exists precisely so the metering claim can always be made truthful.

## Invariants

1. Exactly one `G_cal` multiply per strip per block; no insert applies a second console-scale
   factor. Test: strip bypass nulls dry.
2. `GainStagingNode::process` allocates nothing, locks nothing — the guard (`AudioSection`)
   proves it, like everything else here.
3. The drift vector is **data**: computed at track creation, persisted, reloaded bit-exactly.
   `design()` never sees an un-persisted drift; reloading a project twice yields
   bit-identical filter coefficients.
4. `hathor-dsp-filters` and `hathor-dsp-core` contain no variance, no `channel_id`, no
   calibration semantics (L2 purity, architecture §1).
5. `channel_id` is a stable `u32` handle for the life of the track; inserts/removes never
   renumber. A drift recomputed because an unrelated track was deleted is a bug report,
   not a behavior.
6. Metering reads the *calibrated* stream (after `G_cal`): the sweet-spot band means the same
   thing on every strip.
7. Any node introducing look-ahead latency declares it (`latency_samples`,
   `max_latency_samples`); the strip exposes, the engine compensates.

## Acceptance criteria (M6 gate lines for this feature)

1. **Calibration**: a 1 kHz sine at the console's electrical zero reads `reference_dbfs ±0.1
   dB` on the strip's sweet-spot meter for every reference in the −20…−14 window (the ±0.1
   criterion is a design target until the first real meter exists, marked **expected**).
2. **Determinism**: serialize → reload × 100 → `ChannelDrift` bit-identical; coefficients fed
   to `design()` identical (test exists at unit level once the strip does).
3. **Phase honesty**: with drift ON, two identical tracks through identical strips null to a
   *finite* residue, and the phase tool displays each strip's drift; with drift OFF, the same
   session nulls to ≤ −60 dB (residual: state the measured number at close, never the target).
4. **Cost**: `hathor-audio-probe` bench prints strip-in/out ns/frame with `G_cal` on and off;
   the delta is committed to `bench/`. No percentage claim before that number exists.
5. Guard-clean: automation of trim ±24 dB across 20,000 cycles, 0 allocations (M0-style test).

## Rust concepts it embodies

`SmoothedParam` as the only UI→audio gain path ([docs/it/rust.md](../it/rust.md) §4) ·
`Copy` config structs crossing threads by value · integer mixing (`wrapping_mul`, xorshift)
without `std::rand` · serde-versioned project fields (architecture §5).

## DSP concepts it embodies

dBu vs dBFS references and why a float engine still needs one [docs/it/dsp.md](../it/dsp.md)
(§1–2 level arithmetic) · minimum-phase drift: a ±2 % cutoff shift is a *phase* difference you
can measure with `Coeffs::response` · why component drift is a parameter-domain effect, not a
signal-domain one · declared vs uncompensated latency (architecture §4).

## Deferred (and why)

* **UI skin** of the sweet-spot meter (band colors, hold/peak) → M6: it is the shared
  `egui`+`wgpu` meter widget (M1 library), with a calibration-band parameter — writing it
  before the mixer would guess the visuals.
* **Auto-gain that listens** (perceived-loudness makeup, LUFS-based) → M4 loudness metering
  first; the instantaneous peak mode is honest without BS.1770, the loudness-based variant
  is not.
* **Per-band drift** (independent drift per EQ band, matching "each capacitor is its own
  component") → post-M6 maybe: the current per-strip scalar drift already breaks sterile
  coherence; per-band vectors need a project-format decision (list vs scalar) that M5
  serialization should make with its eyes open.

## Decisions and research findings

Decided by the user 2026-10-27 (recorded, not erased — the reasoning lived in this doc's
first draft):

1. **Variance default posture**: OFF, opt-in with a project console seed. A solo-authored
   DAW must not silently change null tests.
2. **Calibration reference**: −18 dBFS ≡ 0 dBu is the project default; per-strip override
   allowed.
3. **Drift bias range**: shipped envelope ±0.005 linear (≈±0.043 dB); ±0.3 dB remains the
   project-configurable ceiling.
4. **NAM hosting placement**: in-tree core crate only, no CLAP bundle at M6.5 — decided in
   [hathor-nam.md](hathor-nam.md) §Open questions (Q2 minimum-architecture set and Q3 test
   model provenance remain genuinely open there).

Findings and citations (license table, engine comparison): [hathor-nam.md](hathor-nam.md).
The one cross-cutting finding worth restating here: NAM-style models ship *per-model level
metadata*; the console contract must not pretend to know a model's expectation — hence the
un-fusing in §1.

## Exercises (once it exists)

1. Load a project, screenshot the null residue between two identical DI tracks; bump the
   console seed, re-roll, and re-measure 5 times. Plot the spread against the drift bounds:
   the ±2 % cutoff claim must predict the null's floor (this is the feature's null test).
2. Feed a sine at exactly `reference_dbfs` through the strip with trim = 0, drift = 0: the
   NAM-side (or a saturator-side) consumer's own input meter should read 0 dBu. If it reads
   −18, somebody applied the scale twice — find the layer that broke invariant 1.
