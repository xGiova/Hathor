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
| `crates/hathor-dsp-filters` | RBJ biquads: f64 design, DF2T runtime, AVX2 kernel | M0–M1 | **implemented** | 27/27 (debug+release) |
| `crates/hathor-dsp-fft` | complex/real FFT, STFT front-end | M0–M1 | **implemented** (kernels; STFT front-end → M1) | 29/29 (debug+release) |
| `crates/hathor-dsp-measure` | level/correlation/time-alignment, masking meter | M0–M1 | **implemented** (M0 API; masking → M1) | 54/54 (debug+release) |
| `crates/hathor-host-graph` | node chain, scratch buffers, latency sum | M0–M5 | **implemented** (M0 Chain) | 12+1/12+1 (debug+release) |
| `apps/hathor-audio-probe` | cpal/WASAPI harness, xrun & cycle benchmark | M0 | skeleton (build session next) | — |
| `plugins/*` | EQ, Filter, SPAN-like, Compressor, Reverb, Delay, Saturator, kick↔bass phase-alignment plugin | M1–M4 | planned | — |
| `crates/hathor-vst3-host` | C++ VST3 bridge (frozen ABI) | M5 | planned | — |
| Channel-strip calibration & variance | `G_cal` gain-staging contract, seeded per-channel drift, sweet-spot meter | M6 | spec ([docs/modules/hathor-calibration-variance.md](docs/modules/hathor-calibration-variance.md)) | — |
| `crates/hathor-nam` (accepted draft, not yet created) | `.nam` loader + causal NN inference node (NAM captures) | M6.5 (gate G5) | draft spec ([docs/modules/hathor-nam.md](docs/modules/hathor-nam.md)) | — |

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
rustup toolchain install stable   # MSRV 1.85, tested on 1.99
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
