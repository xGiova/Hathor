# OpenDesign brief — M1 surfaces (design-before-code)

> Companion to the owner rule in [../../ROADMAP.md](../../ROADMAP.md) (M1, line
> "Design-before-code rule"). This is the input for the **OpenDesign session** (launched by
> the owner); nothing `egui`/`wgpu` is written until these three faces are mocked and
> reviewed. The 12 plugin skins of M4 derive from the tokens set here, not vice versa.

## Ground rules for the mocks

- HTML mock, high-fidelity, interactive states — layout, states, interaction, in that order.
- Every visual must be drawable by a retained-mode-free immediate UI later: prefer flat
  shapes, strokes, text, and simple gradients; no blur/animation effect that cannot be
  re-implemented in `egui`+`wgpu` at 60 fps without a shader budget conversation.
- The mock does NOT retire the stack decision: the `egui`+`wgpu` touch-spike still runs on
  real hardware afterwards and its numbers (labeled *measured*) re-validate the design; if
  the spike fails, the mock is the cheap part to redraw.
- Data-density honesty: the analyzer is specified at **16,384 bins at 60 fps under 8% of one
  core** (M1 acceptance). Whatever the mock shows beyond that density (decimation, smoothing)
  is a *display* decision — mark each such element so the engine knows what it must produce.

## Shared states the mock must define (from shipped contracts, not taste)

These are real values the engine emits today; every face needs a visual answer for them:

| Value | Engine truth | Question for the mock |
|---|---|---|
| `f32::NEG_INFINITY` (dB) | silence is not a magic number (measure invariant 2) | blank meter? dimmed rail? "−∞" text? must not look like a quiet reading |
| `NaN` | corrupt signal must propagate (never hide it) | a distinct, ugly-on-purpose state: a bug must not read like a low level |
| above 0 dBFS | positive dB is legitimate (a digital signal CAN exceed full scale) | clip-adjacent scale must extend above 0, not cap at 0 |

## Face 1 — the analyzer (M1: SPAN-like plugin)

Purpose: real-time spectrum of what is actually playing, good enough to be the project's
measurement standard (A/B vs SPAN on kick, full-range bass, full mix).

Minimum surfaces and controls:
1. Response curve area: log-frequency × dB grid; the dB scale and floor are user-visible
   settings (SPAN convention: dB range, slope/tilt — mark as engine params).
2. Freeze/hold: frozen trace + live trace visible together (that is what "hold" means on
   screen); a clear visual identity for each, plus a state where frozen ≠ live.
3. Diff / sonar / curve-on-curve comparison: reference curve stored from a frozen trace,
   then differences shown *as a second curve* — the mock must show all three layers legible
   at once (live, reference, diff).
4. **Masking meter lives inside the same structure** (per plan): decide on-screen where its
   band-accumulation reads — this is the surface M2's A↔B masking inherits.
5. Multi-resolution artifact states: LF resolution comes from big FFTs on the full-rate
   signal, HF from small FFTs on decimated signal — the mock should make resolution
   *changes* visible (bin spacing, smoothing) rather than pretending one uniform comb.
6. Latency contract: display latency < 30 ms (M1 acceptance) — no UI element may imply
   averaging across many frames unless labeled as such (a slow meter is a lie about <30 ms).

## Face 2 — channel-strip calibration view (M6 skin, designed now; tokens reused)

Source: [../modules/hathor-calibration-variance.md](../modules/hathor-calibration-variance.md).

Minimum surfaces:
1. One `G_cal` multiply at strip input, applied ONCE — the view must make "where calibration
   happens" unmistakable, and every downstream insert (drive etc.) visually downstream of it.
2. Sweet-spot band meter: RMS over ~300 ms; the band **−20…−14 dBFS** marked on the rail,
   steady-state read against the project default **−18 dBFS ≡ 0 dBu** (per-strip override
   exists — show the override state visually distinct from default).
3. Hold/peak behavior on the band meter (the doc defers the skin to M6: the mock decides it
   now: band color, hold marker, and what "in the band" looks like at a glance).
4. Per-strip drift/trim readouts: `gain_bias_db` (user) vs `drift_db` (deterministic from
   (channel_id, console_seed)) — the mock must show the manual override *sticks* against
   reload (same channel, same drift) without lying that drift is user-settable.
5. Bypass must visually null to dry (acceptance 1: "strip bypass nulls dry") — design the
   bypass state so a silent second scaling factor would be visible.

## Face 3 — the kick↔bass tool (M3: "the thing that should make people say 'this exists?'")

Minimum surfaces (all features exist as engine decisions; this face makes them usable in
under 4 minutes — M3 acceptance):
1. **Nudge in samples** (never ms as the primary unit) + polarity flip. Show samples ↔
   degrees together for the reference case: the fixed arithmetic the UI states: **at 50 Hz,
   1 ms = 18°** (null residue of an uncorrected 1 ms offset reads −10.09 dBFS — that number
   is measured and the tool may reference it).
2. Overlapped fundamental display: the two pitch tracks over one frequency axis, with the
   overlap region visually distinct (the sweet spot of M2.5's pitch engine arrives later;
   mock it as a labeled region now).
3. Lag via cross-correlation: the engine's `time_align_lag` convention is **positive lag =
   `b` arrives late**; the readout must carry the direction (who is late), not just |lag|.
4. Per-band phase rotator: body (100–300 Hz) adjustable *independently* of sub — the mock
   must show two bands with two rotators, and what happens to the summed view when one moves.
5. Null test: inverted-sum level with **solo listening of what is not shared**; acceptance
   target residue < −30 dB in 40–120 Hz — design the "residue in band" element and its
   silence (NEG_INFINITY = perfect null, NOT "0 dB").
6. Sidechain transient-aware + key-filtered: a visible split between "body ducks" and
   "sub does not" (the defect of ordinary sidechains is eating the bottom; the UI must make
   that distinction legible or the feature reads as broken).
7. A↔B masking meter as the tool's ruler (shared element with Face 1's masking position —
   keep the token identical).

## Deliverable checklist (done = the mock answers, the code follows)

- [ ] Three faces drawn with the shared-state table resolved (−∞, NaN, >0 dBFS have visuals).
- [ ] Frequency axis conventions locked (log ticks, octave labels at 30/60/125/250/500/1k…,
      because the LF band 40–120 Hz and the sweet-spot 100–300 Hz must be readable without zoom).
- [ ] dB scale, floor, and slope controls placed on Face 1 and named for engine params.
- [ ] Every element that needs data the engine does NOT yet produce is labeled
      ("needs: X" — feeding the M1/M2/M3 task lists, never silently assumed).
- [ ] Token set (colors, spacing, type sizes) exported — M4's 12 plugin faces derive from it.
- [ ] Touch-spike pass/fail sheet drafted from the mock: the exact egui+wgpu elements that
      must be re-validated at 60 fps before anything native is committed.
