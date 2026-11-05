# Module: `hathor-analyzer`

> Status: **written, tested 15** — 7 unit + 2 mock-host lifecycle + 6 spectrum-proof
> (debug+release, clippy `-D warnings`, fmt; the cost bench is `#[ignore]`d and runs in the
> gate). Dependencies: `hathor-clap` (path) and `hathor-dsp-fft`; `hathor-dsp-core` is a
> **dev-dependency only** — the allocation guard for the tests; the plugin path itself never
> references it. The first plugin of the project: a `cdylib` exporting the CLAP entry
> `clap_entry` **by value** — the artifact a host loads.

## What it is

A SPAN-like multiresolution spectrum analyzer as a CLAP plugin — **headless v0, no GUI**.
It is the first consumer of `hathor-clap`: one safe `Plugin` implementation over
`FftBank`, handing the host the entry table. The point of its existence is the ROADMAP's CLAP
rationale: our DSP leaves the DAW and runs in real hosts to validate the engine *before* the
host exists. The v0 behavior is deliberately small so that **every claim is testable**.

## Contract (behavior v0)

* **The analyzer observes; it never alters.** Main stereo in → `0.5·(L+R)` mono → bank;
  audio out is a **bit-exact pass-through** (byte-identity over raw bits — an identity, not a
  comparison — gated over 40 random blocks per channel). Zero sound alteration is an
  *invariant*, not a missing feature: a spectrum analyzer that touches the signal would
  contradict its own definition.
* **Ports**: one main stereo port each way (ids 10 in / 20 out), declared **in-place pairs**.
  When the host hands the same rows, the pass-through becomes the no-op the host already
  performed — touching aliased rows would alter the sound we promise to leave alone (and the
  lifecycle test asserts the in-place buffers come out untouched).
* **Nine params** (the table below), all automatable. Hostile values are clamped into the
  spec range; a **non-finite** value resets that param to its default — never a NaN walking
  into the bank's setup asserts. An unknown id is ignored per CLAP, never guessed. Every
  saved-state-visible change calls `host.state_mark_dirty()` (counted by the lifecycle).
* **Mono in / stereo out**: right goes silent — a *declared v0 rule*, tested
  (`mono_in_copies_to_left_zeroes_right_and_still_drives_the_bank`), not a surprise to
  discover later.
* **Honest refusals, no panics inside host callbacks**: `activate` refuses rates outside
  44.1k–192k (the bank's own range) and a `max_frames` of 0 or > 1<<20; `process` outside
  the active window returns `Error`; more frames than the scratch promised at `activate`
  (a spec violation by a host — *expected, NOT VERIFIED*) returns `Error` rather than write
  past the buffer.
* **Latency 0, declared** — the STFT display's inherent block of delay belongs to seeing,
  not to the audio path: the pass-through is same-block. No hard realtime requirement.
* `init` logs one line through `host.log`, and a host **without** `clap.log` is still a
  valid host: log failure is not an init failure.

## The nine params (normative v0 table)

| id | name | module | range | default |
|---|---|---|---|---|
| 0 | Floor | Display | −144…−20 dB | −120 |
| 1–4 | Avg 256/1024/4096/16384 | Smoothing | 0…2000 ms | 10 / 30 / 100 / 250 |
| 5–8 | Decay 256/1024/4096/16384 | Smoothing | 0…96 dB/s | 45 / 30 / 20 / 20 |

`Floor` affects only `render_db`, never `process`. The defaults **mirror
`BankParams::new(48_000)` exactly** — and the mirror is pinned twice:
`defaults_mirror_the_bank_profile_exactly` (values) and
`default_params_rebuild_the_default_bank_bit_exact` (the round trip back into a
`BankParams` is lossless, or the plugin's default face would silently diverge from the
bank's). `value_to_text` writes plain decimal; `param_from_text` is its honest inverse —
anything else is refused. Param order in `param_info` is the host's display list, verbatim.

## How it works

* **One safe seam for all the math.** `process_audio(l, r, out0, out1)` holds the entire
  audio behavior in slice terms; the `Plugin::process` adapter differs only in borrow
  choreography (the view hands out one mutable row at a time) and runs the same
  `downmix_and_analyze` core. Why the seam exists: the wrapper's `ProcessView::from_sys` is
  `pub(crate)` — a consumer crate cannot fabricate a view for a direct DSP test. The
  spectrum proofs exercise *this* arithmetic with no raw pointers anywhere; the C-ABI path
  is the lifecycle's job. (Gap reported to the lead.)
* **The rebuild decision — labeled.** `FftBank::new` is the bank's only allocation site, so
  the plugin builds it **exactly once, in `activate`** (main thread, outside any deadline),
  and the mono scratch is sized there to the host's advertised max block. A param changed
  while active is stored, remembered, and applied at the **next** activate-cycle. A live
  swap would need double-buffering and buys nothing in v0 — a host automating smoothing
  constants mid-take is not a scenario the M1 roadmap needs. Decision reported;
  **change it only with measurement.**
* **State doctrine: 72 bytes, nothing else.** The 9 params, little-endian f64. Smoothing,
  hold and ring contents are **transient by CONVENTION §8.2** — a reloaded project rebuilds
  them by listening, exactly like SPAN does after a reload. `load_state` clamps on the way
  in: 72 bytes of garbage cannot produce an out-of-range param.
* **The entry is hand-wired, and that is a recorded wrapper gap.** A host `dlsym`s the
  symbol `clap_entry` and treats its address as a `const clap_plugin_entry` — the struct
  must live **by value at that symbol**, and Rust static initializers cannot copy another
  static's value, so `clap_entry!`'s generated `pub static CLAP_ENTRY` cannot be
  re-exported under the exported name by value. `clapabi` therefore re-derives the same
  tiny table from the wrapper's *audited public pieces* (`entry::init_impl`,
  `entry::deinit_impl`, `entry::factory_for`, `plugin::create_instance`) and exports it by
  value. **No wrapper logic is bypassed or duplicated**: init refusal, double-init,
  factory-id matching, instance creation — every policy delegates. (A macro shape that
  emits the static by value was reported to the lead; until it lands, this is the truth.)
* **Display views exist, unused.** `levels()` / `render_db()` / `db()` / `log_bin_freq()`
  are exposed for the future display layer so it can read the **same bank** the audio
  thread feeds — not a second one; nothing consumes them in v0 (the tests do, as oracles).

## Invariants

1. **Zero sound alteration** — bit-exact pass-through, byte-identity, including through the
   AudioSection blocks and the in-place mode. If this ever stops holding, the plugin is
  wrong, not the audio.
2. **`process` allocates nothing** — by construction (scratch sized at `activate`, bank
   alive only between activate and deactivate) *and* asserted: 8 blocks inside
   `AudioSection::enter()` with `allocs_here() == 0`, the warm-up run kept outside the gate
   so lazy init cannot hide inside it (the wrapper's recipe, inherited).
3. **No raw pointer in `analyzer.rs`** — pure safe Rust; the only `unsafe` in the crate is
   the `clapabi` forwarding, each raw operation in an explicit block with its SAFETY
   comment (`unsafe_op_in_unsafe_fn = "deny"`; `unwrap_used`/`expect_used`/`todo`/`exit`
   denied in non-test; `missing_docs` warned).
4. **The param table is the clamp** — hostile input reaches `FftBank::new` only through
   `sanitize`, so the bank's setup asserts are unreachable from outside; and the plugin's
   default face equals the bank's default profile (pinned, see the table above).

## Verification (tests, not opinions)

* **7 unit** (`analyzer.rs`): `defaults_mirror_the_bank_profile_exactly`,
  `default_params_rebuild_the_default_bank_bit_exact`,
  `hostile_values_are_clamped_or_reset_never_nan`, `state_codec_round_trips_and_clamps`,
  `activate_refuses_unsupported_rates_and_frame_sizes`,
  `param_specs_and_ids_are_consistent` (defaults inside ranges, ids stable, empty views
  before any bank), `mono_in_copies_to_left_zeroes_right_and_still_drives_the_bank`.
* **6 spectrum proofs** (`tests/spectrum_proof.rs`) against an **independent oracle** — a
  direct Hann DFT summed in f64 over the exact last frame, plus re-derived axis arithmetic:
  never our FFT, never our mapping tables ("two copies of a wrong constant share bugs; an
  independent sum does not"). The sine at **984.375 Hz** (bin-exact for the 1024 tier at
  48k) must light the **dominant bin the oracle predicts**, within **0.3 dB**, with the
  full-scale convention sane (0.5-amplitude sine reads ≈ −6 dBFS) —
  `a_known_sine_lights_the_bin_the_oracle_predicts`; **silence sits at the Floor exactly**
  (Floor −40 ⇒ every bin == −40, proving the param reached the bank) —
  `silence_sits_exactly_at_the_floor_param`; **L = −R cancels to zero everywhere** (a
  channel-pick instead of `0.5·(L+R)` would light the tone) —
  `downmix_is_the_average_not_a_channel_pick`; pass-through over **40 random blocks** by
  raw-byte identity, with the non-vacuity guard ("the bank heard something, else the above
  is vacuous") — `pass_through_keeps_distinct_channels_bit_exact`; in-place mode skips the
  copy but still analyzes — `in_place_mode_skips_the_copy_but_still_analyzes`; refusal
  paths (not-active, oversized, mismatched R) — `refuses_when_not_active_or_oversized`.
  Noise is the house deterministic Lcg — CONVENTION §5, no RNG in numeric work.
* **2 lifecycle** (`tests/lifecycle.rs`) — the official **proxy**,
  `analyzer_lifecycle_through_the_c_abi`, driving the very static
  exported at `clap_entry`: version handshake, double-init refused (wrapper policy),
  foreign factory id → null, descriptor fields through raw C strings, foreign plugin id
  refused at create, all **four wired extensions** answer and
  **`get_extension("clap.gui")` is asserted NULL — the absence of GUI is a verified
  choice, not laziness**; ports 10↔20 with `CLAP_PORT_STEREO`, MAIN flags and the in-place
  pair both ways; the 9-param table visible through the vtable (`"Floor"/"Display"`,
  `(−144, −20, default −120)`, `"Decay 256"/"Smoothing"`, `get_info(9)` false);
  `value_to_text(−96) = "-96.00"` and `text_to_value("2.5") = 2.5`; flush applies events
  in order (two params moved, two dirty-marks); `save` → perturb → `load` over the host
  streams restores exactly (72 bytes round-trip); pass-through bit-exact warm-up and
  post-gate; the AudioSection block above; in-place rows untouched; `process` after
  `deactivate` = `CLAP_PROCESS_ERROR`; entry flag clear after `deinit`. Plus
  `create_paths_stay_clean`: null id refused; a null host still *creates* but `init`
  refuses it instead of misbehaving — and it touches no global, safe in parallel.
* **1 cost bench**, `#[ignore]`d (`tests/cost_bench.rs`, `whole_plugin_path_cost_per_block`) —
  a debug timing is a fiction;
  the whole plugin path through the same vtable a host calls, with a **0-frame control**
  measuring the wrapper+view floor to subtract.

## Measured (dev machine, release, 2026-11-05) — the anchor

**12.34 µs per 64-frame block, whole plugin path** (vtable + view handling + bit-exact
pass-through + downmix + bank) = **0.93% of one core** against the 1333.33 µs budget —
*measured*, `cargo test -p hathor-analyzer --release --offline -- --ignored`. The
wrapper+view floor by the zero-frame control is **0.01 µs** — plumbing is noise, the DSP is
the cost. Reference: the bank alone measured **0.94% of a core** (`hathor-dsp-fft`'s own
bench, same machine, same day). The two agree within the resolution of their different
methods; the honest reading is that adding pass-through, downmix and the whole FFI path on
top of the bank changed nothing measurable.

## Deferred (and why)

* **GUI: forbidden until the mock OpenDesign passes** (design-before-code), not "pending
  polish". A CLAP plugin without GUI is legitimate and loadable — and the lifecycle test
  asserts `clap.gui` answers null, so the absence is verified.
* **Per-channel analysis, extra ports, multi-plugin factory**: v0 is one stereo pair and
  "this factory carries exactly one plugin" (crate decision); each waits for its scenario.
* **Live param application** (double-buffered bank swap): the labeled decision above;
  reopening requires a measurement and a real need.
* **Transport-driven display** (tempo grid, beat clock): the wrapper already maps the
  transport; the analyzer does not ask for it while headless.

## Honesty ledger

**HOST LOAD: NOT VERIFIED.** The lifecycle proof is the mock; loading the `.clap` in a real
Reaper is an **owner step** — the artifact recipe is in the commit report; the exported
table is driven exactly as a host would drive it, but `dlsym` itself cannot be exercised
offline, and the test file says so. Mono-in-right-silent is a *declared v0 rule*.
`features=null`, double-init refusal, in-place-pair promises and every host-behavior
assumption are **inherited from the wrapper with their labels** (expected, NOT VERIFIED).
The cost anchor above is the only performance claim this doc makes, it is labeled measured
with its method, and the zero-frame control prevents the pass-through copy from hiding in
timer noise. The hand-wired entry is documented as the wrapper gap it is — not as style.
