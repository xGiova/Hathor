# Hathor — project documentation

Three audiences, three doors. Pick the right one: reading everything in order does not
work.

**Language policy** (normative: [../STYLE.md](../STYLE.md) §A): everything here is
**English canonical**. The only Italian content in the project is the teaching track under
[it/](it/), which is *not* translated and *not* duplicated. `docs/` = architecture and
module reference; `docs/it/` = the learning track.

## If you want to understand **how the software is built** → [architecture.md](architecture.md)

The layers, the threading model, who may call whom, where the truth of time lives, how
latency compensation works, how a corrupted project dies. It is the document that saves
you when, eight months from now, you no longer remember why a thing is the way it is.

## If you want to **learn** → [it/](it/) — Italian only

| Document | What it teaches | When to read it |
|---|---|---|
| [it/dsp.md](it/dsp.md) | The DSP of this project from zero: sample rate, dB, filters, **phase**, FFT, dynamics, loudness | Before touching a DSP module |
| [it/rust.md](it/rust.md) | Rust for people coming from Java/JS/Python, with examples taken **from our code**, not from generic tutorials | When the borrow checker says no |
| [it/prior-art.md](it/prior-art.md) | Reverse engineering yes/no, what to measure, what to read, the 40 things that teach you more than closed sources | Now |

## If you need **one specific module** → [modules/](modules/)

Every module has its own MD with the same structure:
`Purpose · How it works · Invariants (the rules that bite when violated) · Acceptance
criteria · Rust concepts it uses · DSP concepts it embodies · What is deferred and why ·
Exercises.`

| Module | Status | Document |
|---|---|---|
| `hathor-dsp-core` | written, tested | [modules/hathor-dsp-core.md](modules/hathor-dsp-core.md) |
| `hathor-dsp-filters` | written, tested | [modules/hathor-dsp-filters.md](modules/hathor-dsp-filters.md) |
| `hathor-dsp-fft` | written, tested (kernels) | [modules/hathor-dsp-fft.md](modules/hathor-dsp-fft.md) |
| `hathor-dsp-measure` | written, tested | [modules/hathor-dsp-measure.md](modules/hathor-dsp-measure.md) |
| `hathor-host-graph` | written, tested (M0 Chain) | [modules/hathor-host-graph.md](modules/hathor-host-graph.md) |
| gain staging & channel variance (M6 contract, policy not crate) | spec | [modules/hathor-calibration-variance.md](modules/hathor-calibration-variance.md) |
| `hathor-nam` (accepted draft, crate not yet created; gated G5) | draft spec (M6.5) | [modules/hathor-nam.md](modules/hathor-nam.md) |
| `hathor-audio-probe` | written, tested | [modules/hathor-audio-probe.md](modules/hathor-audio-probe.md) |
| `hathor-vst3-host` | spec (M5) | `modules/hathor-vst3-host.md` (spec due M5) |

## How to read the code of this project

1. Read the module's `.md`.
2. Read the **doc-comment at the top of the `.rs` file** (it is written to be read, not to
   decorate: it contains the *why* a formula is that one and not another).
3. Read the **tests**. In this project tests are the executed specification: the test
   `butterworth_is_minus_3db_exactly_at_f0` says more than the RBJ documentation.
4. Only then read the implementation.

Style rule: **a comment explaining *what* the code does is a wasted comment** (the code
already says it). A comment explaining *why* that formula, *which* design error it
prevents, or *what* would have happened otherwise — that one gets written.

## Document conventions

- `→ M2` marks something deliberately deferred, with a phase reference in
  [../ROADMAP.md](../ROADMAP.md). "Not done *yet*" is a decision; "not done, period" is
  debt: they must be written differently.
- Every performance number quoted is labelled **measured** or **expected**. If there is no
  label, it is a lie: flag it and fix it in the file.
- When an acceptance figure is changed, the old value is written down, not deleted
  (see ROADMAP §9).
- Links to the teaching track are relative from the file that holds them: from `docs/` it
  is `it/dsp.md`, from `docs/modules/` it is `../it/dsp.md`.
