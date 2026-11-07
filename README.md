# Hathor

An open-source digital audio workstation for Windows, written from scratch in
Rust — DSP engine included. Named after the Egyptian goddess of music, rhythm
and joy.

**Status: pre-alpha, milestone M0.** This repository is currently the DSP
foundation plus the architecture documentation. No UI, no audio recording yet —
see [ROADMAP.md](ROADMAP.md) for the full plan and gates.

## What this is

Hathor is a solo-authored but openly licensed DAW. The bet: build the audio
stack bottom-up (own allocator contract, own biquad/FFT/analyzer libraries, own
plugin system with CLAP as the native ABI and VST3 hosting via a frozen C++
bridge), stealing *workflow ideas* — not code — from the best:

- **Ableton Live** → session/workflow ergonomics
- **Studio One** → mixer layout and inspection
- **Pro Tools** → audio editing precision

Among the built-in plugins is a **kick ↔ bass phase-alignment tool** (M3): sample-accurate,
per-band phase metering and correction. Accurate phase math is a requirement many features
share — which is why the lower layers expose it honestly.

## Status

| Module | Scope | Milestone | State | Tests |
|---|---|---|---|---|
| `crates/hathor-dsp-core` | realtime contract, buffers, node ABI, params, allocation guard | M0 | **implemented** | 20/20 (debug+release) |
| `crates/hathor-dsp-filters` | RBJ biquads: f64 design, DF2T runtime, AVX2 kernel; M2: BW/LR crossovers (measured parity law) & allpass phase rotator | M0–M2 | **implemented** | 44/44 (debug+release) |
| `crates/hathor-dsp-fft` | complex/real FFT, multi-rate bank, masking meter (M2, `mask.rs`) | M0–M2 | **implemented** (bank = 0.94% of one core, measured release; meter 200.2–204.0 µs per update+render at display rate, measured quiet machine 2026-11-07) | 59/59 +3 bench (debug+release) |
| `crates/hathor-dsp-measure` | level/correlation/time-alignment, per-bin phase & group delay (M2), RMS tracker (M6 primitive) | M0–M6 | **implemented** (masking meter ships in `hathor-dsp-fft`) | 75/75 (debug+release) |
| `crates/hathor-host-graph` | node chain, scratch buffers, latency sum | M0–M5 | **implemented** (M0 Chain) | 12+1/12+1 (debug+release) |
| `crates/hathor-clap` | safe plugin-side CLAP wrapper over clap-sys 0.5.0 (f64 wire formats: the `data32` XOR `data64` law; mock-host proxy green; real DAW load = owner step) | M1 | **implemented** | 31/31 +2 doctest-sketch ignored (debug+release) |
| `apps/hathor-audio-probe` | cpal/WASAPI harness, xrun proxy & cost bench | M0 | **implemented** (G1 closed: live 60 s WASAPI run, 0 allocs, worst chunk 0.042× budget) | 169/169 (debug+release) |
| `plugins/hathor-analyzer` | SPAN-like analyzer v0, headless CLAP (GUI pending design; f64 hosts supported — proof 7 measured at double width; real-DAW load = owner step) | M1 | **built** (anchor: whole path 0.93% of one core, measured) | 18/18 (debug+release) |
| `plugins/hathor-eq` | 8-band parametric EQ v0, headless CLAP (f64 hosts supported (measured, host load = owner step); GUI pending design; real-DAW load = owner step, **NOT VERIFIED**) | M2 | **built** (M2 cost gate **MET at both widths**: 8 bands active 0.44–0.45% f32 / 0.44–0.47% f64 @96 kHz, neutral at the wrapper floor 0.01–0.03 µs — measured, lead's gate, quiet machine 2026-11-07; not yet a home anchor) | 19/19 (debug+release) |
| `plugins/*` (rest) | Filter, Compressor, Reverb, Delay, Saturator, kick↔bass phase-alignment | M1–M4 | planned | — |
| `crates/hathor-vst3-host` | C++ VST3 bridge (frozen ABI) | M5 | planned | — |
| Channel-strip calibration & variance | `G_cal` gain-staging contract, seeded per-channel drift, sweet-spot meter | M6 | spec ([docs/modules/hathor-calibration-variance.md](docs/modules/hathor-calibration-variance.md)) | — |
| `crates/hathor-nam` (accepted draft, not yet created) | `.nam` loader + causal NN inference node (NAM captures) | M6.5 (gate G5) | draft spec ([docs/modules/hathor-nam.md](docs/modules/hathor-nam.md)) | — |

**Workspace total (2026-11-07, task-11 fold): 448/448 green, debug and release** — the lead's
gate on a quiet dev machine (440 from the M2-gate batch + 3 clap + 1 analyzer + 4 eq). The
5 cost benches (2 bank + 1 meter + 1 analyzer + 1 EQ) are
`#[ignore]`d and run separately in release, alongside the 2 non-executed `ignore`d doctest
sketches in hathor-clap; totals include the 2 executable doctests (hathor-host-graph,
hathor-clap).

<sub>* "Implemented" = green in debug **and** release, `clippy -D warnings` clean, realtime contract enforced (zero alloc/lock in `process`). Test counts are real `cargo test` results, refreshed at each milestone gate — not aspirations.</sub>

## Repository layout

| Path | What it is |
|---|---|
| `crates/hathor-dsp-core` | Realtime contracts: buffers, node ABI, params, allocation guard |
| `crates/hathor-dsp-filters` | Biquad section (RBJ design in f64, DF2T runtime, AVX2 kernel) |
| `docs/` | Architecture and module documentation (English canonical) |
| `docs/it/` | Learning track: DSP & Rust docs in Italian, project-specific |
| `STYLE.md` / `CONVENTION.md` | Normative style (Google Rust guide + deviations) |
| `ROADMAP.md` | Milestones M0–M8 with acceptance gates |
| `TECH_DEBT.md` | Honest ledger of what's weak and why |

## Getting started

```sh
rustup toolchain install stable   # MSRV 1.88, tested on 1.99
cargo test                        # unit + behavioral tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Windows-only by design (WASAPI shared/exclusive, ASIO opt-in later). x86-64 with
AVX2 is the SIMD baseline; scalar fallbacks are tested against the kernels.

## Documentation map

- [docs/architecture.md](docs/architecture.md) — how the system is layered and threaded
- [docs/modules/](docs/modules/) — one design doc per module
- [docs/it/dsp.md](docs/it/dsp.md) — the DSP course, taught on this project's bugs
- [docs/it/rust.md](docs/it/rust.md) — the Rust course, same treatment
- [CONVENTION.md](CONVENTION.md) — rules that make a solo project reviewable

## License

Apache-2.0. See [LICENSE](LICENSE). Third-party licensing notes and the
clean-room policy live in [docs/it/prior-art.md](docs/it/prior-art.md) and
`ROADMAP.md`.
