# Module: `hathor-eq`

> Status: **written, tested 15** — 12 unit + 3 mock-host lifecycle (debug+release, clippy
> `-D warnings`, fmt; the cost bench is `#[ignore]`d and excluded from the gate — run it
> explicitly). Dependencies: `hathor-clap` (path) and `hathor-dsp-filters`; `hathor-dsp-core`
> is a **dev-dependency only** — the allocation guard for the tests; the plugin path never
> references it. The second plugin of the project: a `cdylib` exporting the CLAP entry
> `clap_entry` by value — and the first consumer of `hathor-dsp-filters` through a host face.

## What it is

An 8-band parametric EQ as a CLAP plugin — **headless v0, no GUI** (design-before-code, same
law as the analyzer). 34 params (bypass + master trim + 8 bands × {type, freq, gain, Q}) over
**true stereo**: one filter state per channel, never a fake-stereo filter — the filters crate
says why (invariant 2 there); this crate obeys. The v0 surface is deliberately small so every
claim is testable.

## Contract (behavior v0)

* **Neutral by default, and neutrality is bit-exact.** Every band starts `Off`, bypass off,
  trim 0 dB: the power-on graph is exact identity (biquads with `IDENTITY` coefficients), so
  "install and hear nothing" is a testable property, not a vibe. The lifecycle asserts the
  bit-exactness **also in the in-place mode** (identity biquads rewriting the same bits).
* **Band types are the RBJ menu**: `Off / LowPass / HighPass / Peak / Notch / LowShelf /
  HighShelf` (the numeric `Type` param selects through `TYPE_NAMES`). Shelves use slope
  `S = 1.0` — maximally flat — and the `Q` param is **documented-inert** for them: a v0
  decision, in the param table, not hidden in a comment nobody reads.
* **34 params** (the normative table below), all automatable, hostile values clamped,
  non-finite values reset to default — "nothing hostile ever reaches a `design()` call".
* **Mono in / stereo out**: right goes silent — the same *declared* rule as the analyzer
  (`mono_in_stereo_out_keeps_the_silence_doctrine`), tested, not a surprise.
* **Honest refusals, no panics inside host callbacks**: `activate` refuses sample rates
  outside **8 kHz–192 kHz** (the "fixed-topology biquad EQ" envelope — the analyzer's floor
  is 44.1k because the bank's tiers need it; an EQ needs nothing, so it honestly goes
  wider) and a `max_frames` of 0 or > 1<<20; `process` outside the active window returns
  `Error`; a too-large block is **refused before touching anything**
  (`oversized_block_refuses_before_touching_anything` — the task-5 law, inherited).
* **Latency 0 declared** — minimum-phase biquads have fractional *group delay*, an integer
  PDC figure would be a lie (the filters crate states the difference; the analyzer declares
  the same zero).
* `init` logs one line; a host without `clap.log` is still a valid host.

## The 34 params (normative v0 table)

| id | name | module | range | default |
|---|---|---|---|---|
| 0 | Bypass | Global | 0…1 | 0 (processing) |
| 1 | Gain | Global | −24…+24 dB | 0 |
| 2+4b | Type | Band b+1 | 0…6 | 0 = Off |
| 3+4b | Freq | Band b+1 | 10…20 000 Hz | 1 000 |
| 4+4b | Gain | Band b+1 | −24…+24 dB | 0 |
| 5+4b | Q | Band b+1 | 0.1…20 | 1.0 |

The table is built by a `const fn` from one template — 34 entries cannot drift apart by
hand. The param-table test (`param_table_is_consistent_and_default_neutral`) pins the shape:
`SPECS[0].max == 1.0`, `SPECS[2].name == "Type"`/`"Band 1"`, `SPECS[33].name == "Q"`/
`"Band 8"`, every default inside its range, band 1 starts `Off`.

## How it works

* **Coefficient doctrine — the filters crate law, enforced at the host seam.** A param edit
  runs the design (`sin/cos/sqrt`, f64) **in `set_param_value`**, never in `process`;
  `process` only walks the linear coefficient ramp (`Biquad::ramp_to`, `RAMP_FRAMES = 128`
  frames ≈ 2.7 ms at 48k: long enough to kill zippers on a resonant, short enough to feel
  instant — and the ramp is per-coefficient, never per-parameter value). Zero transcendentals
  per sample, zero allocations inside the audio section. The ramp test is exactness, not
  vibes: `ramp_lands_exactly_on_the_designed_target` (a live edit must ramp, never jump; 192
  frames > 128: finished, landed on target).
* **Bypass freezes, it does not poison.** Bypassed, the plugin still accepts param edits as
  pure field writes (ramp targets set), and un-bypassing resumes the ramps —
  `bypass_freezes_state_and_unbypass_resumes_the_ramp`.
* **Unknown type / hostile design falls back to `IDENTITY`, never to a poisoned filter**: a
  design failure routes to identity coefficients (`unwrap_or(Coeffs::IDENTITY)` at the one
  call site, which documents why).
* **Decision, labeled: a flat serial band loop over the 16 `Biquad` lanes — NOT a
  `hathor-host-graph` `Chain`.** The loop is the whole topology (fixed order, no side-chains)
  and `hathor-host-graph`'s own tests already prove the `Chain` machinery; wiring EQ
  through it now adds
  indirection, not proof. Revisit when a plugin needs reconfigurable routing.
* **State doctrine: 272 bytes, nothing else.** The 34 params, little-endian f64. Filter
  memory (z1/z2) and in-flight ramps are **transient** by the same CONVENTION §8.2 logic as
  the analyzer — a reloaded project settles by re-designing, not by a serialized state blob
  of internals. Hostile blobs come out legal (every field clamped through `sanitize` on load).

## Identity (what the host reads — pinned forever)

`dev.hathor.eq` | **Hathor EQ** | vendor **Hathor** | url, manual and support all
`https://github.com/xGiova/Hathor` | version **0.1.0** | "8-band parametric EQ (headless v0;
GUI pending design)." | features **`audio-effect` + `equalizer`**.

The same law as the analyzer, applied from day one: the id is persisted by hosts into saved
projects, so it is forever; `descriptor_ships_the_pinned_identity_byte_for_byte` walks
entry → factory → descriptor and pins every string byte-for-byte (the table *is* the shipped
artifact); `equalizer` is the CLAP `plugin_features` constant, compile-verified
NULL-terminated by `FeatureList` — the field is filled, the host's reaction is the open
checklist item below.

## Invariants

1. **Neutrality is bit-exact** — defaults mean identity lanes and untouched audio, asserted
   warm-up, in-place, and inside the allocation gate. An EQ that colors "nothing" is broken
   by definition.
2. **`process` allocates nothing and pays no transcendentals** — designs happen at the param
   edge; the lifecycle's AudioSection blocks with `allocs_here() == 0` guard the claim
   (warm-up outside the gate, the wrapper's recipe).
3. **Coefficient design is host-input-proof** — clamp/sanitize before every `design()`; a
   failure falls back to `IDENTITY`; hostile params "never poison the graph"
   (`hostile_params_are_clamped_and_never_poison_the_graph`).
4. **True stereo** — 16 lanes, one state per channel; the filters crate's invariant 2 is
   obeyed at the plugin seam, and the pass-through preserves channel distinctness.
5. **No raw pointer in `eq.rs`** — pure safe Rust; the only `unsafe` outside tests is the
   `clap_entry!` expansion, wrapper-generated and audited inside `hathor-clap`. Tests carry
   their own unsafe under SAFETY comments (fabricated views, mock vtables).

## Verification (tests, not opinions)

* **12 unit** (`eq.rs`): `param_table_is_consistent_and_default_neutral`,
  `defaults_are_bit_exact_identity_and_lanes_settle_as_identity`,
  `lp_band_attenuates_a_hot_tone_and_passes_a_low_one` (bins at multiples of 750 Hz =
  48k/64, whole cycles in the window: a "5 kHz" tone would measure the window, not the
  filter — the 3.5 dB lie its comment records; RMS vs prediction),
  `peak_at_zero_gain_is_identity_within_tolerance`,
  `bypass_freezes_state_and_unbypass_resumes_the_ramp`,
  `hostile_params_are_clamped_and_never_poison_the_graph`,
  `activate_refusals_and_process_outside_active`,
  `oversized_block_refuses_before_touching_anything`,
  `state_round_trips_and_hostile_blobs_come_out_legal`,
  `mono_in_stereo_out_keeps_the_silence_doctrine`,
  `ramp_lands_exactly_on_the_designed_target`, `band_state_view_tracks_the_params`.
* **3 lifecycle** (`tests/lifecycle.rs`) — the official **proxy**, same shape as the
  analyzer's: `eq_lifecycle_through_the_c_abi` (full call order through the real vtables —
  init/factory/descriptor/extensions/ports/params/flush/save→perturb→load/pass-through
  warm-up and post-gate/in-place untouched/refusal after `deactivate`/entry flag clear),
  `descriptor_ships_the_pinned_identity_byte_for_byte` (identity table, features list, NULL
  terminator), `create_paths_stay_clean` (null id refused; null host creates but `init`
  refuses). **`get_extension("clap.gui")` asserted NULL** — the GUI absence is a verified
  choice here too.
* **1 cost bench**, `#[ignore]`d (`tests/cost_bench.rs`): 64-frame blocks at **both** 48 kHz
  (budget 1333.33 µs) and 96 kHz (666.67 µs), two shapes — shipping default (all bands off)
  and all 8 bands active — driven through param-value events like a host would. The MEASURED
  lines are evidence, never promises made in this file.

## Measured (dev machine, release, 2026-11-07 — lead's gate, quiet machine)

**8 bands ACTIVE @ 96 kHz: 0.46–0.48% of one core** — the M2 acceptance line "<1% of a core
at 96 kHz" is **MET** (measured; *not yet* a home anchor — those are named deliberately at
milestone close, this number carries the "measured" label until then). Same path @ 48 kHz:
**0.23%**. All-bands-off reads the **same as active** — honesty by construction: the
identity biquads walk the same recursion per sample. Wrapper+view floor **0.03–0.04 µs**
(zero-frame control). The bench itself prints these under `--ignored --nocapture`; the
numbers quoted here are the quiet-machine runs, dated with their method.

## Deferred (and why)

* **GUI** — forbidden until the mock OpenDesign passes (design-before-code); the lifecycle
  asserts `clap.gui` NULL, so the absence is verified, not assumed.
* **Chain-based topology** — the labeled decision above (serial loop now; `Chain` when a
  plugin needs reconfigurable routing).
* **Per-band bypass, drive/saturation per band, asymmetric shelves (S param)** — v0 is the
  RBJ menu at `S = 1.0` with the documented-inert `Q` on shelves; each waits for its
  consumer and a listening pass.
* **Listening acceptance** ("does it sound like an EQ") — a human item by the crate's own
  header, not a CI one; it belongs to the M2 pass alongside the bank's tuning review.

## The owner's load step — checklist (what no CI can take)

1. Build the `cdylib` and install it where Reaper scans for CLAP plugins.
2. Load: first real `dlsym`/`init`/factory reads by a real host.
3. **Features:** verify `audio-effect` and `equalizer` show up sane in the plugin browser —
   host-side handling expected-per-spec, **NOT VERIFIED** today.
4. Null test in a real session: default state must leave the sound bit-identical (the
   analyzer's invariant, now an EQ's too).
5. Save → reload: id `dev.hathor.eq` resolves, 272 bytes round-trip through host streams.

## Honesty ledger

**HOST LOAD: NOT VERIFIED** (the mock is the declared proxy; `dlsym` cannot be exercised
offline). The cost figures are labeled *measured* with machine, date, rate and shape — and
the 0.46–0.48% @96k is the **M2 gate evidence, not a home anchor** until named at milestone
close. "Off == active cost" is stated as the honesty it is (identity lanes still walk the
recursion — the plugin buys neutrality at full price rather than hiding a fast path). The
`Q`-inert-on-shelves and serial-loop choices are **decisions, labeled**, not gaps. Host
behaviors (aliases, lifetimes, call order) inherit the wrapper's expected-per-spec labels.
The listening question is *human* and stays out of the green-count.
