# Module draft: `hathor-nam` — Neural Amp Modeler hosting

> Status: **draft specification** — lead-accepted 2026-10-27; scope decision same day:
> **in-tree core crate only, no CLAP bundle at M6.5** (see §Open questions Q1). Placement
> **M6.5** (see [ROADMAP.md](../../ROADMAP.md)). Crate name `hathor-nam`, paths
> `hathor_nam` (the project's `hathor-` crate-name convention, ROADMAP §1 repo tree). Expected
> dependencies: `hathor-dsp-core` (buffers,
> `Node`, ramps), optionally `hathor-dsp-measure` (validation harness). **Feasibility
> verdict: feasible with constraints** — the constraints are budget evidence (gate G5) and
> honest model-size claims, not architecture.

## Purpose

Load `.nam` amp-capture files (Neural Amp Modeler, by Steven Atkinson — GitHub user
**sdatkinson**, not "steveatkinson" as an early note in this project's prompts had it; the
correction is part of the prior-art lesson: a citation is verified before it anchors a plan)
and run their inference as an ordinary L3 `Node` inside `hathor-host-graph`, so a guitar
track can play through community captures in Hathor.

Why this belongs to the thesis (ROADMAP §0): NN inference is *not* bolt-on DSP — its
amplitude sensitivity is exactly what makes the gain-staging contract
([hathor-calibration-variance.md](hathor-calibration-variance.md)) observable, and its CPU
shape (per-block matrix math with a hard 1333 µs/64-frame budget,
[architecture.md](../architecture.md) §2) demands the same measured-not-opined discipline.
The [pasted external spec](../archive/nam-gain-staging-spec-2026-10-27.md) (§6–7) motivates
the calibration contract; this doc scopes the model host itself.

## What a `.nam` file is (verified from source, not folklore)

A `.nam` file is **a JSON document**, emitted by the MIT-licensed trainer
([exportable.py](https://github.com/sdatkinson/neural-amp-modeler/blob/main/nam/models/exportable.py)),
whose top-level keys are exactly:

| Key | Content |
|---|---|
| `version` | model format version (str) |
| `metadata` | date + user metadata; gain/loudness-type fields exist — **semantics NOT VERIFIED** |
| `architecture` | class name: `Linear`, `ConvNet`, `Recurrent` (LSTM), `WaveNet` (dir listing of [`nam/models`](https://github.com/sdatkinson/neural-amp-modeler/tree/main/nam/models) + the `.namb` README) |
| `config` | hyperparameters (layer counts, channel counts, dilations, …) |
| `weights` | one flat array of JSON-encoded floats |

Also verified from the same source: the trainer optionally exports `test_inputs.npy` /
`test_outputs.npy` example pairs (and the loader ecosystem uses them for validation), and
`export_onnx` **raises `NotImplementedError`** in the base class — ONNX is *not* the
ecosystem's native distribution path. That single fact demotes "just use ONNX Runtime" from
plan to footnote.

A newer community **binary** container (`.namb`, magic `NAMB`, raw little-endian f32
weights, CRC32) is fully specified in [tone-3000/nam-binary-loader](https://github.com/tone-3000/nam-binary-loader)
(MIT). Per that README, text floats are ~10× the size of the binary and a typical WaveNet
shrinks ~80% when converted — third-party claims, **NOT VERIFIED** by us; the size class
("WaveNet weights fit in tens of MB of JSON on a 512 KB-RAM MCU once binary") is the
take-away, and it is enough to say the *compute*, not the bytes, is the realtime risk.

## How inference would work here

### The contract fit (why this is nearly architectural-free)

* **`Node`**: `setup()` parses JSON, allocates weight matrices and the history ring (the
  declared-allocation zone); `process()` is the inference itself: **zero allocation, zero
  lock** — every buffer sized at setup from `config`, every index static. The guard
  (`AudioSection`) enforces the promise exactly as it does for every other node; the loader
  never runs on the audio thread.
* **Latency**: the convnets (Linear/ConvNet/WaveNet) are **causal convolutions over a
  history ring** — the model needs the past, not the future, so `latency_samples() = 0` is
  the honest declaration, like every minimum-phase IIR (hathor-dsp-filters invariant 5).
  Whether the official NAM *plugin* reports additional delay: **NOT VERIFIED** (and it does
  not bind us — we declare what our kernel does). The receptive field is *state*, not latency.
* **Vectorization note (structural, not measured)**: the filters doc's law "recursion does
  not vectorize in time" is about IIR feedback. NN convolutions are **FIR over the history
  ring** — no state feedback per frame — so SIMD *along frames* (with tap reads from the
  ring) is legitimate here, unlike in a biquad. First version scalar and correct (the
  behavioral-test rule), `lanes`-style kernel later, cost measured in `bench/` before any
  claim.

### Validation methodology (the part that makes this Rust-project-grade)

Per CONVENTION §4 (external references, behavioral tests): the trainer's own exported
`test_inputs.npy`/`test_outputs.npy` pairs — and the official
[NeuralAmpModelerCore `example_models/`](https://github.com/sdatkinson/NeuralAmpModelerCore)
used by the community loaders — are the reference outputs our inference must reproduce
within a documented, justified tolerance (proposed: max abs error ≤ 1e-4 for f32 parity,
**expected** until measured; the exact number gets set against the first real diff, like
the filters crate's 2e-6 SIMD convergence bound was). Two independent paths again: the
C++ core (`benchmodel` tool, referenced by the core README) vs our Rust node.

### Realtime arithmetic (framework, not numbers)

Per output sample, one channel path costs ≈ `Σ_layers (2·k·c_in·c_out)` MACs (plus 1×1
mixers/gating as the architecture demands). Per 64-frame stereo block: `64 · 2 · MACs/sample`.
Whether that fits 1333 µs (budget per architecture §2) **depends entirely on the model** —
and typical per-model cost distribution is exactly the thing this project must **measure,
not opine** (ROADMAP principle 4). The spike (below) answers it on our machine with our
guard; anything printed before then is a rumor. What is known from prior art and can be
cited: the ecosystem runs these models "in real time" on desktop CPUs and on embedded
hardware (STM32H7-class per tone-3000's blog/README — their words, hardware and buffer
details **NOT VERIFIED** by us).

## Engine options — license & fit research table

All fetched 2026-10-27 via GitHub API / raw files (external, untrusted content; verified
where a primary source was readable):

| Item | What | License (source) | Fit for Hathor |
|---|---|---|---|
| [sdatkinson/NeuralAmpModelerCore](https://github.com/sdatkinson/NeuralAmpModelerCore) | reference C++ inference ("Core DSP library for NAM plugins"; includes `benchmodel` realtime bench tool) | **MIT** (GitHub API `license` field) | read-allowed prior art; C++ stays out of our tree (ROADMAP §4 risk table: C++ only in `hathor-vst3-host`) |
| [sdatkinson/NeuralAmpModelerPlugin](https://github.com/sdatkinson/NeuralAmpModelerPlugin) | official plugin app | **MIT** (GitHub API), built on [iPlug2](https://github.com/iPlug2/iPlug2) whose LICENSE.txt is a BSD-style permissive text (API: "Other/NOASSERTION"; full text not read — **NOT VERIFIED**) | the *app* is not a dependency; shows VST3/AU hosting of MIT core is accepted practice |
| [sdatkinson/neural-amp-modeler](https://github.com/sdatkinson/neural-amp-modeler) | Python trainer, emits `.nam` | **MIT** (GitHub API) | the format's *emitting code* is permissive; `export_onnx` = NotImplementedError (read) |
| **The `.nam` format itself** | JSON keys above; community docs & converters exist (`.namb` MIT spec; [neural-amp-modeler-wasm](https://github.com/tone-3000/neural-amp-modeler-wasm)) | format ≠ code: data layout; the *reference exporter* is MIT | **verdict below** |
| The `.nam` **weights** | user captures shared across sites (tone3000 etc.) | per-creator terms; some free, some paid/commercial | content licensing, not code licensing: we load what the user owns; no bundling of third-party captures in-repo |
| [mikeoliphant/neural-amp-modeler-lv2](https://github.com/mikeoliphant/neural-amp-modeler-lv2) | unofficial LV2 port | **GPL-3.0** (GitHub API) | read to understand, never copy (CONVENTION §7) — and we don't need to: three permissive reimplementations exist |
| [AidaDSP/AIDA-X](https://github.com/AidaDSP/AIDA-X) | prior art: NN amp hosting, DPF-based; inference via [RTNeural](https://github.com/jatinchowdhury18/RTNeural) | **GPL-3.0-or-later** (README, read) | learn-allowed. **Input-gain practice (README, read)**: a per-instance "Input Level" attenuator *before* the model plus an "Output Level" compensation *after*, peak meters clipping-warning at −3/0 dB — i.e. the ecosystem's real answer to §6 of the pasted spec is *per-model-slot trim, metered*, which corroborates un-fusing model trim from console `G_cal` |
| [jatinchowdhury18/RTNeural](https://github.com/jatinchowdhury18/RTNeural) | the realtime NN inference library AIDA-X actually uses | **BSD-3-Clause** (GitHub API) | the closest architecture to copy *concepts* from (permissive, header-only C++): per-layer ring state, block loop, no runtime allocation. Still C++ → study, don't vendor |
| [fabiohl/NeuralAmpModeler-rs](https://github.com/fabiohl/NeuralAmpModeler-rs) | "real-time neural inference DSP … in pure Rust" (own description) | **Apache-2.0** (API + LICENSE.txt read) | the only Rust full-stack NAM engine found; created 2026-07, ~7 stars: young. Legally compatible (vendor-OK); *risk* register: single maintainer, maturity unproven. Decision at spike time: vendor-as-dep vs study vs write-own. Default plan: write-own thin loader+WaveNet kernel (it is *our* thesis work), keep it as validation oracle |
| [sonos/tract](https://github.com/sonos/tract) | ONNX/NNEF inference in Rust ("used in production at Sonos for wake-word and streaming speech-recognition" — README, read: streaming/realtime **voice**, not audio-callback hard-RT) | **MIT OR Apache-2.0** dual (LICENSE read; note: vendored `tensorflow/protos` + `onnx/protos` excluded from that statement — upstreams are Apache-2.0 projects; audit before shipping) | **blocker found**: README badge says rustc ≥ 1.91.0 — our MSRV is 1.85 (README/CONVENTION §9). Also: `.nam` doesn't export ONNX natively (verified above), so tract would need trainer-side exports: extra moving part. Verdict: not for the audio callback; possible for offline model *conversion* experiments |
| [pykeio/ort](https://github.com/pykeio/ort) + [microsoft/onnxruntime](https://github.com/microsoft/onnxruntime) | ONNX Runtime bindings / runtime | **Apache-2.0** (API) / **MIT** (API) | ORT `Run()` brings thread pools and its own allocation strategy — claims of zero-allocation callback use are **NOT VERIFIED** and contrary to its design center; heavyweight (lib + DLL) for what is, for NAM, a handful of conv layers. Verdict: no |
| [huggingface/candle](https://github.com/huggingface/candle) | minimalist ML framework | **Apache-2.0** (API) | allocates per forward by design (ergonomics-first); useful *offline* (training-side experiments, format checking), not in `process()`. Verdict: no for inference; maybe dev-dependency |

### License verdict (the question the pasted research task asked)

**An Apache-2.0 DAW can implement a `.nam` loader and inference engine without copyleft
contamination, provided:**

1. the implementation is derived from the **MIT** trainer's emitted structure, the **MIT**
   `.namb` binary spec, and/or clean reading of permissive sources — not from the **GPLv3**
   LV2 port or AIDA-X internals (CONVENTION §7: GPL read-to-understand only, never copy);
2. "is the spec itself encumbered?" — the format has no separate legal spec document with
   its own license; it is embodied in MIT code and MIT-licensed community descriptions, and
   *at least three independent permissive reimplementations already exist*
   (`fabiohl/NeuralAmpModeler-rs` Apache-2.0, `djshaji/nam-loader` Apache-2.0 for Android,
   `tone-3000/nam-binary-loader` MIT + the tone-3000 wasm work). Reimplementing a public
   data format from its permissive-emitting code is the same posture this repo already took
   toward the VST3 SDK when it was still restricted, then MIT (ROADMAP §1). **This is an
   engineering judgment with citations, not legal advice — the lead signs the final call.**
3. third-party *captures* (.nam weights files) are user content: per-creator terms apply;
   Hathor bundles none.

## Where it hooks into Hathor (integration map)

* **L3 slot**: `hathor-nam` exposes `NamNode: Node` — one instance per loaded capture per
  strip insert slot, stereo = two independent mono paths (hathor-dsp-filters invariant 2
  logic: shared state across L/R would be a mono filter with two inputs).
* **Mixer (M6)**: the NAM insert sits *downstream* of the strip's `G_cal`
  ([hathor-calibration-variance.md](hathor-calibration-variance.md)); its own "input trim"
  parameter can be seeded from `.nam` metadata (semantics to be verified at spike time).
* **Post-NAM auto-makeup**: not this crate's job — M4 dynamics code + the strip utility
  defined in the calibration doc §4; look-ahead variant gated on M5 PDC.
* **State**: weights are immutable after `setup`; the project stores (file ref or hash,
  per-model slot params, bypass) — never re-serializes the weights (ROADMAP M5 opaque-state
  philosophy).
* **Failure model**: a corrupt `.nam` fails at *load* (control thread, `Result` with
  message), never mid-block; a NaN from a pathological model is caught by the M5 NaN
  watchdog policy (architecture §8), and FTZ/DAZ is already contract (hathor-dsp-core).

## Deferred / refused (and why)

* **LSTM (`Recurrent`) architecture** → after WaveNet+ConvNet+Linear are shipping: the
  per-sample recursion is real-time legal but the least understood by us; order by what
  captures guitarists actually use (spike should count architectures in a public capture
  library — measurement).
* **AIDA-X `.aidax` model loading** → refused for now: GPL family (engine concepts already
  absorbed via RTNeural/BSD).
* **GPU inference** → non-goal for the callback (driver round-trips are not realtime on
  this OS story); possible for offline retraining tools, out of roadmap scope.
* **Streaming/pulsified generic NN hosting (tract-class)** → out: we host *NAM models*, not
  arbitrary ONNX graphs; the thesis is a mixing engine, and this is where "if the 6th thing
  isn't thin, refactor" (ROADMAP §0) applies in reverse — a general NN host is a different product.

## Open questions (for the user, after the lead review)

1. ~~Core vs bundled plugin~~ — **DECIDED 2026-10-27 (user): in-tree core crate only.**
   The plugin-first recommendation was declined: NAM hosting is a feature of the DAW, not
   a distributable plugin. The CLAP wrapper is out of M6.5 scope; the license verdict holds
   either way if that decision is revisited.
2. Which architectures gate the M6.5 milestone as "minimum useful set" — proposal: Linear
   (cab/IR-style), ConvNet, WaveNet; LSTM post-milestone.
3. Model provenance UX: does Hathor ship a tiny permissive test model (a capture the author
   grants to the repo) or only load what the user supplies? (Trademarks note: "Neural Amp
   Modeler"/NAM are the project's names; we load a format, we are not the NAM plugin — the
   README's third-party list gets a line, per CONVENTION §7.)
4. ~~G5 budget posture~~ — **DECIDED 2026-10-27 (user): the documented posture is
   acceptable.** If heavier captures do not fit the 64/48k budget, "NN inserts: 128-frame
   blocks recommended" closes the milestone; the measured spread at 64 is still committed.
   64 is not a hard gate on the capture library — but it remains the engine's default block,
   and the gap is stated as a number, not smoothed over.

## Exercises (once it exists)

1. Load an official example model, render its `test_inputs.npy` through `NamNode` inside a
   `Chain`, correlate against `test_outputs.npy`: find the first divergence (it will be at
   the history-ring head or a channel-mix index — the two places a from-scratch reimplementation
   lies).
2. Drive the node with a sine at three calibrated levels (−20, −18, −14 dBFS reference window
   from the calibration doc) and plot output harmonics: this *measures* the amplitude
   sensitivity the whole feature exists for, and is the honest demo for the sweet-spot meter.
