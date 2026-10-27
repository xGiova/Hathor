# Design mocks (OpenDesign → this repo)

Canonical home of the M1+ interface mocks drawn in **OpenDesign** under the
design-before-code rule ([../open-design-brief-m1.md](../open-design-brief-m1.md)).

## What lands here

- `m1-analyzer/` — the analyzer face (live/frozen/diff layers, dB scale, masking position)
- `m1-calibration-strip/` — the channel-strip calibration view (G_cal, sweet-spot band, override)
- `m1-kick-bass/` — the kick↔bass tool (nudge-in-samples, per-band rotator, null, sidechain)
- `tokens.css` — the extracted token set (colors, spacing, type scale). **This file is the
  contract**: the `egui`+`wgpu` library implements these values, and M4's 12 plugin faces
  derive from them — not vice versa.

## Rules

1. **Commit mock files as OpenDesign produced them.** No reformatting, no hand edits in git:
   iterate in OpenDesign, re-export, replace the file — one decision per diff.
2. Every mock subfolder carries a one-paragraph `decision.md` *in-repo* (what state/answers
   were chosen there — e.g. how `−∞` silence and `NaN` read visually). The paragraph cites
   the brief's checklist item it closes.
3. `needs:` labels found during review are transcribed into the milestone task list (M1/M2/M3),
   never resolved silently inside the mock.
4. Files here are documentation-grade: CI skips `docs/**` by design (`paths-ignore`), and
   nothing in `apps/` or `crates/` may import from this folder at build time. The token file
   is *copied* into UI code at implementation time (measured against it), not linked.

## Naming

`<face>__v<N>__<state>.html` for iterations kept for the record (e.g.
`analyzer__v3__frozen.html`); the review only ever reads the highest `<N>`. Superseded
iterations stay (cheap history, and the redraw-during-spike case wants them).
