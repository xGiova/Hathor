# Module: `hathor-eq`

> Status: **written, tested 19** — 15 unit + 4 mock-host lifecycle (debug+release, clippy
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
* **f64 hosts are supported (task-11, 2026-11-07):** the pass-through is bit-exact at double
  width; the neutral/copy path never touches the scratch; the active-band path converts in,
  filters through the activate-sized f32 lanes, converts out — the same 24-bit-floor doctrine
  as the analyzer. Mixed or broken buffer widths are **refused, never guessed**
  (`mixed_or_broken_buffer_widths_are_refused`). Loading with a real 64-bit host is owner
  territory: **NOT VERIFIED** offline.
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
* **`neutral_fast` — the ratified invariant branch (task-11, 2026-11-07).** When all 16
  lanes sit at `IDENTITY`, no ramp is in flight, and the master trim is exactly 1.0, the
  process flow **copies the rows directly at the host's width** (f32 or f64, bit-exact both)
  and the lanes stay **frozen** — the bypass-freeze doctrine, extended: "neutral" and
  "bypassed" are now the same physical path, and unfreezing re-derives from the frozen
  coefficients exactly as after any bypassed stretch. The flag is a **derived per-block
  predicate**: `is_neutral_fast()` is the documented path-proof (armed on a fresh neutral
  instance, disarmed by one live band — the guard test shows both edges) and it is **never
  stored in the state blob** — 272 bytes are the params, period. The copy path carries an
  adversarial proof: it moves values an f32 scratch could not represent — 1/3, √2, the
  smallest subnormal, −1e-300 — and it **opens by asserting the fixture is not
  f32-round-trippable**, because "a test that cannot fail proves nothing".
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

* **15 unit** (`eq.rs`): `param_table_is_consistent_and_default_neutral`,
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
  `ramp_lands_exactly_on_the_designed_target`, `band_state_view_tracks_the_params`, and the
  task-11 three: `f64_rows_flow_through_the_scratch` (the hostile copy proof — 1/3, √2,
  subnormal, −1e-300, with the non-round-trippability pre-assert),
  `neutral_and_bypass_at_64_bit_never_touch_the_scratch` (the frozen lanes stay frozen at
  double width), `mixed_or_broken_buffer_widths_are_refused`.
* **4 lifecycle** (`tests/lifecycle.rs`) — the official **proxy**, same shape as the
  analyzer's: `eq_lifecycle_through_the_c_abi` (full call order through the real vtables —
  init/factory/descriptor/extensions/ports/params/flush/save→perturb→load/pass-through
  warm-up and post-gate/in-place untouched/refusal after `deactivate`/entry flag clear),
  `descriptor_ships_the_pinned_identity_byte_for_byte` (identity table, features list, NULL
  terminator), `create_paths_stay_clean` (null id refused; null host creates but `init`
  refuses). **`get_extension("clap.gui")` asserted NULL** — the GUI absence is a verified
  choice here too. The fourth, task-11: `hot_bands_pass_the_strict_guard_at_both_widths`
   — 32 hot param-events through the **real flush path**, ramps settled **outside** the gate
   (the house lesson in practice: the `allocs_here() == 0` asserts come after `drop(sec)`),
   then 8 f32 blocks **and** 8 f64 blocks inside `AudioSection` STRICT, zero allocations
   each half; plus `is_neutral_fast()` as the documented **path-proof** on the typed object
   (armed on fresh-neutral, disarmed by one live band — invisible through the vtable, same
   `process`, one extra window into its state).
* **1 cost bench**, `#[ignore]`d (`tests/cost_bench.rs`): 64-frame blocks at **both** 48 kHz
  (budget 1333.33 µs) and 96 kHz (666.67 µs), two shapes — shipping default (all bands off)
  and all 8 bands active — **at both widths** (f32 rows and fabricated f64 rows), driven
   through param-value events like a host would. The MEASURED
  lines are evidence, never promises made in this file.

## Measured (dev machine, release, 2026-11-07 — lead's gate, quiet machine)

**8 bands ACTIVE @ 96 kHz: f32 2.93–2.97 µs/block (0.44–0.45% of one core)** and **f64
2.91–3.14 µs (0.44–0.47%)** — the M2 acceptance line "<1% of a core at 96 kHz" is **MET AT
BOTH WIDTHS** (measured — still *not* a home anchor: those are named deliberately at
milestone close, these carry the "measured" label until then). Same path @ 48 kHz: **0.23%**
(f32, the task-10 run; active bands never took the copy path, so this number stands).
Neutral now costs **0.01–0.03 µs — the wrapper's own floor**: the `neutral_fast` copy skips
the recursion entirely. Wrapper+view zero-frame control **0.03–0.04 µs**. The bench prints
under `--ignored --nocapture` in both widths; the numbers here are the quiet-machine runs,
dated with their method.

> **A dated correction — kept, never erased.** This doc's first version claimed
> "all-bands-off reads the **same as active** — honesty by construction: the identity
> biquads walk the same recursion per sample" (measured 2026-11-05). That is **overturned by
> task-11's copy path (2026-11-07)**: neutral is now *strictly faster*, because a state that
> provably cannot change the audio is not walked through a filter recursion at all. The
> corrected claim is the honest one: off costs the wrapper floor, active costs 0.44–0.47%,
> and the old sentence survives only here, with its date and its verdict.

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
6. **64-bit host:** load with a host that hands `data64` rows — the f64 path is measured and
   mocked, but no real sequencer has seen it yet.

## Honesty ledger

**HOST LOAD: NOT VERIFIED** (the mock is the declared proxy; `dlsym` cannot be exercised
offline — **f64 hosts included**: "f64 path shipped and measured" says nothing about a real
64-bit DAW loading the binary). The cost figures are labeled *measured* with machine, date,
rate, width and shape — and the 0.44–0.47% @96k is the **M2 gate evidence, not a home
anchor** until named at milestone close. The old "off == active cost" claim was **overturned
with a date attached, not deleted**: the copy path earns its speed by not filtering, which
is exactly what "neutral" *means* — and the hostile-numerics proof (1/3, √2, subnormal,
−1e-300, non-round-trippability pre-asserted) keeps that speed honest at full width. The
`Q`-inert-on-shelves and serial-loop choices are **decisions, labeled**, not gaps. Host
behaviors (aliases, lifetimes, call order) inherit the wrapper's expected-per-spec labels.
The listening question is *human* and stays out of the green-count.
