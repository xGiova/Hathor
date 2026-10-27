# ROADMAP — Hathor: native DAW + DSP suite (Windows-first)

> Living document. Every phase has **measurable acceptance criteria**: a phase is not
> "done" when it feels done, it is done when it passes its criteria.
> Last update: 2026-10-27 (M0 in progress).

---

## 0. Project thesis

**The product is not "one more DAW". It is a DAW that owns its DSP engine: the realtime
contract, and the honest phase/latency math shared by many features (filtering, PDC,
metering, alignment), are not things you can bolt onto someone else's engine.**

This thesis drives every scope decision. If a feature does not serve it, it goes in
`NON-GOALS`. The practical benefit: a project that is otherwise "impossible"
(DAW + 12 plugins + synth) becomes "orderable".

Three design sources, decomposed into cheap parts and expensive parts (§6).

### Guiding principles

1. **Plugin-first, host-second.** Every increment must be usable *immediately*, inside
   other hosts, before our host exists.
2. **One engine, many plugins.** EQ, multiband, saturator (oversampling), reverb
   (delay line), analyzer (FFT) and the kick/bass tool share ~5 primitives. If the 6th
   plugin is not "thin", the architecture is wrong: stop and refactor.
3. **Contract > memory.** The realtime rules are mechanical (allocation guard,
   tests, lints), not mnemonic.
4. **Measure, don't opine.** No "should be fast enough": every phase has a number and a
   budget.
5. **Reversibility of language choices.** The DSP is a pure crate with an exportable
   C-ABI; the host is the only layer that might change language.

### Decision gates (where course can be changed at low cost)

| Gate | After | Question | If the answer is no |
|---|---|---|---|
| G1 | M0 | The benchmark numbers + how the borrow checker feels | `hathor-dsp-core` stays Rust, but UI/host in C++/JUCE. Cost: ~0 (the DSP is already isolated). |
| G2 | M1 | Is the analyzer already useful compared to SPAN? | The suite stays the engine, the kick↔bass plugin (M3) becomes the first product. |
| G3 | M3 | Is the kick/bass tool something other people *use*? | The project becomes "my private DAW": host scope cut down to mixer+editing. |
| G4 | M5 | Does the host hold 40 tracks at 64 samples? | Cut advanced editing (M6) and stay "mixer + plugin host". |
| G5 | M6.5 spike | Do NAM-class neural captures fit the 64/48k budget on this machine, inside the zero-alloc contract, at a usable share of the model library? | NN hosting stays *outside* the DAW (the NAM plugin loads through the VST3 bridge); the calibration & variance contract ships unchanged (M6) — it was never NAM-only. |

---

## 1. State of the repo (today)

```
daw/
  Cargo.toml            workspace, MSRV 1.85, release profile with lto=fat
  .gitignore            excludes the ASIO SDK (not redistributable)
  crates/
    hathor-dsp-core/    ✓ written: planar buffers, Node (+latency), atomic params + ramps,
                        allocation guard, FTZ/DAZ. With tests.
    hathor-dsp-filters/ ✓ written: RBJ design in f64, DF2T + coefficient ramp, AVX2 lanes8,
                        BiquadNode. 27/27 green debug+release.
    hathor-dsp-fft/     → M0
    hathor-dsp-measure/ → M0
    hathor-host-graph/  → M0 (linear Chain)
    hathor-vst3-host/   → M5 (C++/unsafe bridge, now with no licensing obstacles)
  apps/
    hathor-audio-probe/ → M0 (real stream + offline mode + benchmark)
```

### Ecosystem and license facts — verified

| What | Outcome | Source |
|---|---|---|
| VST3 SDK license | **MIT** (© 2026 Steinberg). An open-source host is legally clean. The constraint is the trademark, not the code. | [LICENSE.txt](https://github.com/steinbergmedia/vst3sdk/blob/master/LICENSE.txt) via API |
| ASIO in cpal | the `asio` feature; requires **LLVM/Clang** (bindgen), MSVC, and an SDK downloaded at build time from steinberg.net (`CPAL_ASIO_DIR`). Not redistributable. | [cpal README](https://github.com/RustAudio/cpal) |
| cpal MSRV | **1.85**, Windows 10 minimum. Default backend **WASAPI**, `asio` optional, `realtime` raises the thread priority. | ibid. |
| cpal license | Apache-2.0 (compatible with our choice). | ibid. |
| `BufferSize::Fixed(n)` | exists; `device.default_output_config()?.buffer_size()` gives the valid ranges. | ibid. |
| NAM core / plugin / trainer licenses | `NeuralAmpModelerCore`, `NeuralAmpModelerPlugin`, `neural-amp-modeler` (trainer): all **MIT** (API `license` field). Official plugin shell = **iPlug2**, permissive custom text (full text not read: NOT VERIFIED). | [core](https://github.com/sdatkinson/NeuralAmpModelerCore) · [plugin](https://github.com/sdatkinson/NeuralAmpModelerPlugin) · [trainer](https://github.com/sdatkinson/neural-amp-modeler) |
| The `.nam` **format** | JSON with keys `version/metadata/architecture/config/weights`; architectures `Linear · ConvNet · Recurrent(LSTM) · WaveNet`; `export_onnx` raises `NotImplementedError` (ONNX is not the distribution path). Reading the MIT exporter is the spec. | [exportable.py](https://github.com/sdatkinson/neural-amp-modeler/blob/main/nam/models/exportable.py) |
| Permissive reimplementations already exist | `NeuralAmpModeler-rs` (**Apache-2.0**, pure-Rust realtime NAM engine, young: 2026-07), `nam-binary-loader`/.namb (**MIT**, binary format spec), `nam-loader` Android (**Apache-2.0**). GPL family (LV2 port, AIDA-X **GPL-3.0**) is read-only prior art. | [NeuralAmpModeler-rs](https://github.com/fabiohl/NeuralAmpModeler-rs) · [nam-binary-loader](https://github.com/tone-3000/nam-binary-loader) · [AIDA-X](https://github.com/AidaDSP/AIDA-X) |
| NN inference crates in Rust | `tract` (sonos): **MIT OR Apache-2.0**, production streaming ASR — but MSRV badge **1.91 > our 1.85**, and NAM has no ONNX export. `ort` **Apache-2.0** over ONNX Runtime **MIT**: thread pools + allocations by design, wrong shape for `process()`. `candle` **Apache-2.0**: allocates per forward; offline tools only. The relevant prior art for a **zero-alloc callback** engine is `RTNeural` (**BSD-3-Clause**, C++), which is what AIDA-X uses. | [tract](https://github.com/sonos/tract) · [ort](https://github.com/pykeio/ort) · [candle](https://github.com/huggingface/candle) · [RTNeural](https://github.com/jatinchowdhury18/RTNeural) |

**To verify** — research landed 2026-10-27 (license agent); resolved items keep their source:

- [x] `nih-plug`: **plugin-side only** (ISC) — it ships plugins, not a host; hosting does not go through it. <https://api.github.com/repos/robbert-vdh/nih-plug>
- [x] VST3 host-side prior art: **HelgeSverre/rust-vst3-host** (MIT) and **alexwest1981/sonix** (Apache-2.0) — with the SDK itself now MIT (§1 table), these are the two bridges to study. The `vst3-sys` coverage question is moot for the decision: the binding can follow the SDK directly.
- [x] ~~Herrie~~ — **does not exist as we had noted**; replaced by the two repos above (lesson: a prior-art entry is a citation and gets verified before it anchors a plan).
- [x] `rtrb`: fine for our use, but **strictly SPSC** (one producer, one consumer) — the design assumed exactly that. <https://crates.io/api/v1/crates/rtrb>
- [x] `rustfft`: fine as the dev-dependency reference (and our test bench). <https://crates.io/api/v1/crates/rustfft>
- [x] VST3 SDK license: **MIT** since 2026 — <https://raw.githubusercontent.com/steinbergmedia/vst3sdk/master/LICENSE.txt> (already in the §1 table); CLAP spec/headers: **MIT**, ASIO SDK: user-side download only, cpal: WASAPI default + optional `asio` feature — <https://raw.githubusercontent.com/RustAudio/cpal/master/README.md>.
- [x] NAM hosting research (calibration & variance feature, spec §6–7): core/plugin/trainer **MIT**, `.nam` = JSON emitted by MIT code, three independent permissive loader reimplementations, AIDA-X/LV2 = GPL (study-only), RTNeural = BSD-3 (concepts prior art), tract MSRV 1.91 **conflicts with our 1.85** — full table + license verdict in [docs/modules/hathor-nam.md](docs/modules/hathor-nam.md). Lesson repeated from the `Herrie` entry: the earlier internal note said `steveatkinson/…` — the real GitHub user is **`sdatkinson`**; the correction is recorded, not silently applied.

**Still to verify** (TD-07 shrunk to exactly this): the **ASIO EULA text inside the SDK zip** (we know it is not redistributable; the license terms themselves unread), the exact **CLAP C-ABI wording** (the "stable ABI" claim in the spec, not the license), the **rustfft `rfft` API** shape for the real-FFT wrapper. Plus, opened by the M6.5 spike (G5): the **`.nam` `metadata` gain/loudness semantics** (keys verified, meaning not), the **iPlug2 license full text**, tract's **vendored proto exclusion** (tensorflow/onnx protos) audit before shipping it, **model cost distribution** in a public capture library (nothing about per-block CPU is asserted before the spike measures it), and the **maturity** of `NeuralAmpModeler-rs` if it is ever considered as a dependency or validation oracle.

---

## 2. The phases

Hours = **real person-hours** (solo, including finishing work: tests + measurement, not "code for 3 hours and call it done"). The figures are professionally pessimistic, not enthusiastic.

### M0 — The contract (25–45 h) ← **WE ARE HERE**

**Objective:** prove the model holds, and close the Rust vs C++ discussion with our own numbers.

Deliverables:
- `hathor-dsp-core`: planar zero-alloc `Block`/`ChannelBuffer`; `Node` with **`latency_samples()` from day 1** (PDC is built here, retrofitting = rewrite); `AtomicParam` (UI→audio without locks) + `SmoothedParam`/`LinearRamp` (no clicks in automation); `AudioSection` + `CheckedAllocator`; FTZ/DAZ.
- `hathor-dsp-filters`: RBJ biquad (LP/HP/peak/notch/shelf) in **DF2T**; per-channel scalar version; **AVX2 "8 channels in lanes"** version over an interleaved-by-channel layout + transpose helpers.
- `hathor-dsp-fft`: `rustfft` wrapper + our own radix-2 with a twiddle table, real-FFT via packing.
- `hathor-dsp-measure`: L/R correlation, RMS, dB, **`time_align_lag()`** (cross-correlation: the numerical seed of the kick↔bass plugin).
- `hathor-host-graph`: linear `Chain`.
- `hathor-audio-probe`: real WASAPI stream + **`--offline` mode** (runs even without a sound card) + benchmark.

Acceptance criteria:
1. `cargo test --release` green on every crate.
2. `hathor-audio-probe --offline` completes **20,000 cycles** at 64/48 kHz with **0 allocations** on the guard.
3. The *negative* test passes: a `Vec::push` inside the section trips the guard (strict) or counts it (measure). If it does not blow up, the guard is decoration.
4. Real stream opened at buffer 64: **0 xruns in 60 s**, worst-case callback < **0.40×** budget.
5. Biquad numbers against the RBJ reference within 1e-6; own FFT vs `rustfft` within 1e-4; real-FFT round-trip < 1e-6.
6. Benchmark prints ns/frame for: scalar mono biquad, AVX2 8-lane biquad (with and without transpose), element-wise auto-vectorized, FFT 4096/16384. **With the honest reading: a recursive filter is not vectorized along frames** (state dependency), it is vectorized *across channels*.
7. `clippy -D warnings` clean, with the disallowed-methods configured.

Unblocks: the language choice stops being a matter of faith. And you have a test bench for every CPU figure to come.

---

### M1 — Multi-resolution engine + analyzer (120–180 h)

**Objective:** build the most reusable DSP asset of the project, in the form of a plugin you actually use.

- FFT **multi-rate/multi-size**: a decimated bank (HF: small FFTs on a decimated signal; LF: large FFTs), overlap, smoothing in the magnitude domain. SPAN's "trick" is not the fast FFT, it is the **multi-resolution architecture**; and the masking meter lives inside the same structure.
- **CLAP (+VST3)** plugin wrapper with freeze/hold, dB scale, diff/sonar, curve-on-curve comparison.
- First **shared UI library** (meters, curves, knobs, spectrum on `egui`+`wgpu`): writing it once here instead of 12 times in M4 is the single biggest source of saved hours in the plan.

Acceptance: 16384 bins at 60 fps under **8% of 1 core** on your CPU; display latency < 30 ms; **A/B against SPAN** on 3 sources (kick, full-range bass, full mix) judged "equivalent or better" in the LF reading; the plugin loads in Reaper/Bitwig and does not crash.

---

### M2 — Filters, EQ, and the masking meter (100–150 h)

- Parametric EQ (minimum-phase, IIR, with optional linear-phase) + filter/hook + **live A↔B masking meter**.
- Crossover (Butterworth/Linkwitz) and **per-band allpass/phase rotator**: without this piece the kick↔bass plugin does not exist.
- `hathor-dsp-measure`: per-band phase vector, group delay, null test.

Acceptance: 8-band EQ at 96 kHz under 1% of a core; a masking meter that **agrees** with the listening on a notoriously conflicting kick/bass pair; a phase rotator applied to the bass that makes **the hole at 60-90 Hz disappear**, verified with the null test.

---

### M3 — The kick↔bass instrument (150–220 h)

**Objective:** the thing that should make people say "this exists?".

Features, all built on top of M1+M2 (if one of them needs new DSP, M2 was done wrong):
1. **Time/phase alignment**: nudge in **samples** (not ms) + polarity flip; display of the overlapped fundamental; lag via cross-correlation; the fixed arithmetic written into the UI: *at 50 Hz, 1 ms = 18°*.
2. **Per-band phase rotator**: align the *body* (100-300 Hz) independently of the *sub*.
3. **Null test** (inverted sum) with solo listening of what is *not* shared.
4. **Sidechain transient-aware + key-filtered**: duck the body while leaving the sub (the defect of ordinary sidechains is that they eat the bottom end).
5. **Saturator for the bass** with polyphase oversampling (2×/4×) and anti-imaging filters.
6. **A↔B masking meter** in real time, as the tool's yardstick.

Acceptance (it must be a *listening test*, not a number):
- On a set of 5 "difficult" kick/bass pairs, the time to a clean relationship drops from ~15 min (current chain with external plugins) to **< 4 min**.
- The headroom gain after align+sidechain is **≥ 1.5 dB measured** without changing what is perceived.
- Null test: residue below **-30 dB** in the 40-120 Hz band where cancellation used to be.
- The tool is usable in Reaper as a plugin: that means it is *good*, not just *yours*.

---

### M4 — The classic suite (400–600 h)

All thin on top of `dsp-*`: compressor (FF/FB, key filtering, lookahead with latency reporting), **multiband**, delay (fractional, feedback, tempo-sync), **FDN reverb**, saturator (models), **true-peak / inter-sample** limiter for mastering, gate/expand, metering (LUFS BS.1770, crest, correlation), one utility (gain/flip/monitor).

Acceptance: every plugin passes "the blind listen" against a reference you already own; no plugin allocates in the callback (guard active in every plugin's tests); multiband + EQ + analyzer in chain over 20 tracks < 12% of a core; stereo FDN reverb at 48k < 2% of a core.

---

### M5 — The heart of the host + **the VST3 bridge** (500–700 h)

Here the DAW is born as a *container*, and here your "VST3 as soon as possible" lands — the **earliest physically** possible, because a hosting bridge with no host to validate it against is code you cannot test.

- **Clock and graph**: sample-accurate transport, automation, **real PDC** (group delay, recording/input latency compensation, delay compensation between paths), sidechain routing, freeze/offline render.
- **Project model + serialization** (serde): autosave, undo, recovery from a corrupted file. This is the part that sinks hobby DAWs: it is *state*, not audio.
- **VST3 bridge** (`hathor-vst3-host`): minimal C++ loader + vtable calls from confined Rust unsafe. `IComponentHandler(2/3)`, `IMidiMapping`, parameters, `process()`, latencies, opaque state, sample-rate change, side-chain.
- **Early spike M2.5**: validate loading 2-3 third-party VST3s **in offline rendering** (no UI) as soon as the graph holds. Cost ~40 h, and it removes the "the bridge cannot be done" risk 9 months early.

Acceptance: 40 tracks + 3 third-party plugins per track (one of them with a declared group delay) with the **phase null test intact** between "DAW bypassed" and "through the DAW"; 60 s at buffer 64 with 0 xruns; the project reopened after `taskkill` is not corrupt; undo/redo 100 steps consistent.

---

### M6 — Mixer (Studio One), editing (Pro Tools), workflow (Ableton) (600–900 h)

The visually biggest part and technically the least audio one. See §6 for the **cheap/expensive** decomposition of each wish: this is where the most months are saved.

- Mixer: dense strips, colors/labels, foldback, visible routing, crossfader.
- **Gain-staging & calibration engine + analog channel variance** (the
  [NAM spec §6–7](docs/archive/nam-gain-staging-spec-2026-10-27.md)
  contract — mixer-wide, *not* NAM-only): the strip's `G_cal` multiply at chain input, the
  configurable reference window −20…−14 dBFS (default −18), per-channel drift vectors hashed
  from `(channel_id, console_seed)` and persisted with the track, the 300 ms sweet-spot
  meter (`hathor-dsp-measure` API), post-drive auto-makeup (instantaneous mode; look-ahead
  mode gated on M5's real PDC). Spec:
  [docs/modules/hathor-calibration-variance.md](docs/modules/hathor-calibration-variance.md)
  — ≈15–25 h of this phase (estimate), absorbed inside the range; the primitives (RMS
  tracker) can ride M0/M1 because they are five lines of honest math.
- Audio editing: fade tool with curves, crossfade (equal-power), slip/modeless edit, **tab-to-transient**, snapping to grid/transients, non-destructive comping, **waveform rendering with LOD/tiles** (not a naive `Painter`: this is where timelines die).
- Workflow: global non-modal shortcut system, palette, keyboard-driven editing.

Acceptance: a 30-minute waveform over 40 tracks scrollable at **60 fps**; editing 20 clips with fade+crossfade without click errors; every editing operation reachable **without a mouse**; a 20-minute editing session without the project becoming inconsistent. Calibration & variance: drift vectors reload **bit-identical** (100× serialize/reload test); the sweet-spot meter criterion and the `bench/` cost delta per the acceptance list of the calibration doc (numbers measured at close, never asserted now); the kick↔bass phase tool displays per-strip drift with variance ON — variance deliberately breaks exact inter-channel phase coherence, and the meters are required to stay truthful about it.

---

### M6.5 — NAM model hosting (80–140 h) ← **gated by G5**

**Objective:** load `.nam` captures (Neural Amp Modeler format) and run them as ordinary
zero-alloc `Node`s — the first *consumer* that makes the M6 calibration contract observable.
**Scope decided (2026-10-27, user): in-tree core crate only — no CLAP bundle at M6.5.** NAM
hosting is a feature of the DAW, not a distributable plugin; principle 1 still governs the
native suite, not this host.

Deliverables (all research/licensing verified in
[docs/modules/hathor-nam.md](docs/modules/hathor-nam.md)):

- **Spike first (~20–30 h, the M2.5 pattern)**: loader (Linear/WaveNet) + scalar inference
  inside `Chain`, validated against the trainer's own exported input/output snapshot pairs
  (CONVENTION §4: external reference, not self-agreement), and **benchmarked at 64/48k on a
  spread of public captures** → gate **G5** answers with a committed `bench/` report, before
  the milestone is trusted.
- `hathor-nam` crate: `.nam` JSON loader at `setup()`, causal convnet kernels
  (Linear → WaveNet → ConvNet; LSTM deferred), history rings sized at setup,
  `latency_samples() = 0` declared honestly (causal FIR-over-history — no lookahead by
  construction), model metadata plumbed for per-slot input trim seeding.
- **Not in scope (decided 2026-10-27):** a CLAP wrapper around the same core. If that ever
  changes, the license verdict in the module doc already holds for it.

Acceptance: inference matches the reference snapshot pairs within a documented, justified
tolerance (set against the first real diff, not guessed now); 20,000 cycles at 64/48k inside
`AudioSection` with **0 allocations**; G5 measured share of a sample of public captures that
run at 64/48k on the dev machine, printed and committed; license posture re-checked per §1
rows (no GPL-derived code; the verdict text lives in the module doc). G5 budget posture
(decided 2026-10-27): if heavier captures do not fit the 64/48k budget, a documented
"NN inserts: 128-frame blocks recommended" closes the milestone — the measured distribution
is still committed; we document reality, we do not lower the target quietly.

---

### M7 — Phase-locked sub-synth (150–250 h)

Not Serum. A **sub/fundamental synth that locks onto the kick's phase**: it is small, it reuses everything (M1+M2+M3) and it stays faithful to the thesis. It is also the answer to "ROAR/Signal, but one that solves phase, not just level".

Acceptance: generate a sub that keeps the **null test > -30 dB** against the kick automatically; 4 voices < 2% of a core.

---

### M8 — Serum-2-style synth → **separate project** (800–1500 h)

Wavetable + warp + FM + multimode filter + unison/detune + modulation matrix + GPU display of the tables. It is the single largest project **and the least overlapping** with the mixing engine. Approach it as a second product, not as the last phase of the first.

---

## 3. Arithmos: what it totals

| Phase | Hours | Cumulative |
|---|---|---|
| M0 | 25–45 | 45 |
| M1 | 120–180 | 225 |
| M2 | 100–150 | 375 |
| M3 | 150–220 | 595 |
| M4 | 400–600 | 1,195 |
| M5 | 500–700 | 1,895 |
| M6 | 600–900 | 2,795 |
| M6.5 | 80–140 | 2,935 |
| M7 | 150–250 | 3,185 |

*(2026-10-27: M6.5 (NAM hosting) inserted; M7 cumulative recomputed from the upper bounds
(2,795 → 2,935 → 3,185). The calibration & variance contract was absorbed into M6's range —
it fits inside a ±300 h uncertainty band.)*

**~10 h/week**: M3 (the kick↔bass plugin, usable on its own) in **12-15 months**; M6 (a DAW you can work in) in **5-6 years**.
**~25 h/week**: the plugin in ~6 months; a usable host in ~2.5 years.

Practical and non-negotiable consequence: **the kick↔bass plugin (M3) must be publishable and usable outside your DAW** (VST3/CLAP, like every plugin here): the suite earns its keep while the host matures, and it is what makes M6 easier — by then you already have 5 plugins to test inside it.

---

## 4. Risk register

| Risk | Impact | Mitigation already decided |
|---|---|---|
| **Ending up with 2 languages to learn** (C++ JUCE + Rust) at the same time | lethal | C++ lives **only** in `hathor-vst3-host`, frozen. Everything else Rust. |
| VST3 hosting reveals incompatibilities between plugins | high (months) | M2.5 offline spike, 3 real plugins. Prior art: `rust-vst3-host` (MIT), `sonix` (Apache-2.0). |
| The timeline cannot hold the frame rate | high | LOD/tiles + texture cache; the M6 acceptance test is the first thing done in M6, not the last. |
| Burnout on a 3,000 h project | lethal | Every phase has a deliverable usable *outside*. If M3 delights you, you keep going. |
| Scope creep "let me also take from Reaper…" | high | If a feature is not in §0/§6, it goes to `NON-GOALS` and comes back only after G3. |
| ASIO: LLVM+SDK, non-reproducible build | medium | Default WASAPI (already decided); ASIO = documented opt-in feature, never required for CI. |
| NN inference (M6.5) may not fit the block budget — model sizes are not ours to choose | medium (a feature dies, not the structure) | G5: the spike **measures** a spread of public captures before any commitment; the calibration & variance contract (M6) was deliberately designed to stand alone, so "no" costs a milestone, not an architecture. |
| Denormals / CPU jitter | medium | FTZ/DAZ in the contract, allocation guard in every plugin's tests. |

---

## 5. Technical conventions (decided now, so we don't fight about them later)

- **License of our code: Apache-2.0** (patent grant; in audio there are patents on DRC/codecs, MIT does not cover them).
- **Internal ABI of our plugins: CLAP** (C-ABI, permissive). Our plugins *are* CLAP+VST3 via nih_plug: native inside your DAW and provable outside it. Third-party VST3 enters through `hathor-vst3-host`.
- **Planar** layout in the engine; interleaved **only** at the edges with the driver (`hathor-audio-probe` measures the cost of the transpose, so you know it).
- Every node declares its latency (`latency_samples`) **always**, even 0.
- No `String`, `Vec`, `Box`, `format!` in `process()`. The guard turns it into a failing test, not a review that slips through.
- Lints: `unwrap_used`/`expect_used`/`panic_in_result_fn` deny in the DSP crates (already in `hathor-dsp-core`'s `Cargo.toml`).
- Every phase closes with a committed **`bench/` report** (today's number, to compare with the next one: CPU regressions are real and silent).
- Numerical tests against an *external* reference (RBJ, rustfft, and later BS.1770 with its published values), not against ourselves.

---

## 6. Decomposing the three wishes: where the real cost is

This is the table that decides whether M6 is 600 hours or 2000. "DAW features" are always a package: the cheap part gives you 80% of the feeling, the expensive part the 20%.

### Ableton → workflow + shortcuts
| Cheap (do it now) | Expensive (defer) |
|---|---|
| A **non-modal** shortcut system (context = what has focus), which is the real reason Ableton flies | Session view / clip launcher with scene matrix |
| Creating/navigating tracks and clips from the keyboard, global keys, palette | Racking/warping with musical time-stretch quality (which is serious DSP: pitch/time) |
| Routing from the keyboard, mute/solo arm | Nested group tracks with Ableton behaviour |

### Studio One → mixer
| Cheap | Expensive |
|---|---|
| Dense strips, colors, labels, metering on the channel | VCA/fader groups with behave-to-link, complete "mixer snapshot" |
| Fold, scroll synchronized with the arrange | On-screen routing console with patchbay |
| Plugin-in-slot + drag reorder of the chain | "Scratch pad" / separate monitoring path |

### Pro Tools → audio editing
| Cheap | Expensive |
|---|---|
| Fade tool + curves + equal-power crossfade | Comping with full playlist/take lanes |
| Slip/drag, trim, split, grid+transient snap | Warping / destructive-safe bounce-on-edit |
| **Tab-to-transient**, transient detection (which you already have from the analyzer) | Time editor with multi-clip alignment and "elastique"-grade algorithms |
| Waveform with LOD | Mature MIDI editing (piano roll, Ableton-style MIDI clips) |

> Note: **mature MIDI editing is in no phase.** It does not serve the thesis, and it is
> worth hundreds of hours. If the project has to feel like a "finished" DAW, it will
> enter eventually: the honest place for it is M9, not "somewhere in the middle".

---

## 7. NON-GOALS (explicit list, so we don't rewrite it every time)

- ❌ macOS / Linux (until G3 exists and is passed).
- ❌ VST2 (non-redistributable license; the world has moved to VST3/CLAP).
- ❌ AU / AAX.
- ❌ Other people's proprietary file formats as input (`.als`, `.rpp`, `.ptx`). Import WAV/AIFF + MIDI.
- ❌ Advanced MIDI editing (§6).
- ❌ Video, movie player, film scoring.
- ❌ Cloud/collaboration, multi-user.
- ❌ Controller surfaces / MIDI-learn at the start (after G3).
- ❌ "Let me also take from Reaper" — see §4, scope creep risk.

---

## 8. Next 7 days (concrete)

Done today (2026-10-27): toolchain components + `cargo build --release`; `clippy -D warnings`
and `fmt --check` green on both crates (TD-05); the mechanical i18n migration to English code
(TD-16); `git init` + LICENSE + README (TD-06); `hathor-dsp-filters` closed — 27/27 green debug
**and** release.

1. Close the remaining M0 code: `hathor-dsp-fft`, `hathor-dsp-measure` (minimal), `hathor-host-graph` `Chain`.
2. `hathor-audio-probe`: `--list`, `--offline`, `--stream`, `--bench`.
3. **Benchmark number 1 on screen** → G1: Rust confirmed, or a C++ bridge for the host; then
   the M0 commit with the message "contract + numbers".
4. TD-07 research landed (§1 "To verify"): SDK MIT, nih-plug plugin-side, hosting prior art =
   `rust-vst3-host`/`sonix`, rtrb SPSC, rustfft OK. Remaining: ASIO EULA text, CLAP C-ABI wording,
   rustfft `rfft` API.
5. Decide: the shareable UI (M1) in `egui`+`wgpu` — it must be *touched* before M1, because it is the choice that is worst to revoke.

---

## 9. How to update this file

At every phase close: (a) criteria checked off **with the measured number next to them**, (b) real hours consumed against the estimated ones (the deviation of the estimates is the only tool you have for estimating M6 without fantasy), (c) any reminders moved from/into `NON-GOALS`. If an acceptance figure gets "adjusted to pass", write it *in the file*, do not delete it.
