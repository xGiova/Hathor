# TECH_DEBT.md — the honest register

Rule (CONVENTION.md §8): technical debt is not shame, it is **inventory**. Everything we
know and have not done lives here, with ID, severity and plan. Debt written down badly is
a future bug written down well.

## Open — blocks the M0 gate (G1)

| ID | Debt | Impact | Plan |
|---|---|---|---|
| TD-01 | `apps/hathor-audio-probe` does not exist | G1 cannot be closed: the numbers (0 allocs over 20k cycles, ns/frame biquad scalar vs AVX2 vs transpose, FFT) do not exist | Next step, offline-first (no cpal in the first build) |
| TD-02 | `crates/hathor-dsp-fft` does not exist | M0 acceptance "FFT vs reference" is not runnable; the M1 analyzer has no base | Specification written (docs/modules/hathor-dsp-fft.md); kernel after TD-01 |
| TD-03 | `crates/hathor-host-graph` `Chain` does not exist | hathor-audio-probe has no real multi-node consumer of the `Node` contract | Specification written (docs/modules/hathor-host-graph.md); small |
| TD-04 | `crates/hathor-dsp-measure` does not exist (renamed from `dsp-anal`: the abbreviation read as a pun in English) | `time_align_lag`/correlation: the M3 acceptance primitives | M0 wants it minimal; the FFT-based version is M3 |
| TD-05 | *(closed 2026-10-27 — see Closed table)* | | |
| TD-06 | *(closed 2026-10-27 — see Closed table)* | | |

## Open — does not block M0, but has an expiry date

| ID | Debt | Notes | Plan |
|---|---|---|---|
| TD-07 | Ecosystem checks: **residuals only** — ASIO EULA text inside the SDK zip, exact CLAP C-ABI "stable ABI" wording, rustfft `rfft` API shape | The other five landed in ROADMAP §1 (VST3 SDK MIT; nih-plug plugin-side ISC; hosting prior art `rust-vst3-host`/`sonix` — "Herrie" does not exist as noted; rtrb strictly SPSC; rustfft fine as dev-dep reference) | Before M1 (they determine the real dependencies) |
| TD-08 | `BiquadLanes8`: constrained to exactly 8 channels; >8 channels → scalar fallback; the cost of the planar↔lane transpose is **not measured** | The project's SIMD promise rests on this number | TD-01 measures it; if the transpose eats the gain, the layout is revisited (ADR) |
| TD-09 | `SmoothedParam::step` uses an anti-stall snap on a ULP-based threshold | Heuristically correct (the f32 stall bug is real and tested), but an automation that must land *exactly on a given frame* needs `LinearRamp` — decide the usage at the first real consumer (mixer M6) | Documented in the module; the usage pattern is written in docs/modules/hathor-dsp-core.md |
| TD-10 | Serialization of the global-state tests is **a convention, not a compiler-enforced constraint** | A new test that touches the allocator without the mutex = intermittent flaky. The pattern lives in `alloc_guard::tests::serial()` | Evaluate: test integration in a separate binary (implicit `--test-threads`) or a custom lint; not worth a dependency now |
| TD-11 | `Node` does not define the channel/sample-rate change policy once the session has started | `setup()` allocates (Vec lanes); what happens when the audio thread meets a block larger than `GraphHints`? Today: `BufferTooSmall` (correct), but *re-setup* does not exist | Before `--stream` in the probe: re-setup must be called by the control thread, never from the callback |
| TD-12 | MXCSR: `disable_denormals()` writes process/thread state; no test resets it, the effect is global | Accepted (FTZ+DAZ is exactly what we want everywhere on the audio thread); the current tests simply set it | If someday a numerical test *wants* denormals: use `count_denormals` on local slices, never remove FTZ globally |
| TD-13 | `panic = "abort"` rejected in the profiles | `catch_unwind` is needed for the guard's tests; it is a **decision**, not a gap (Cargo.toml comments it) | Nothing: renegotiate only with an ADR |
| TD-14 | No linear-phase/FIR: the `latency_samples() > 0` contract is not exercised by a real node | 0-latency IIRs cover it only halfway; the declared/compensated equivalence must be proven by a node that *has* real latency | M2 (linear-phase EQ) closes it; the chain-level sum test (`chain_reports_summed_latency`) is spec'd but lives with `hathor-host-graph` (TD-03) |
| TD-15 | Name: **Hathor** applied to README/ROADMAP/Cargo `repository`; still missing: per-source Apache-2.0 banner | A one-line header per file is only worth a dedicated mechanical commit | Next mechanical commit after the first push, together with CI |
| TD-17 | `#[non_exhaustive]` absent on public enums that can grow (`FilterKind`, `FilterError`, `NodeError`, `AudioSectionError`) | STYLE.md §C.8; consumers (our own plugins first) must compile-match future variants | Small commit before any crates.io publish |
| TD-18 | Subsonic designs (f/sr < ~1e-4): the stability margin is O(w0²) — **smaller than the coefficient ulp**, so f32 cannot represent the verdict honestly (the stability grid skips those ratios with a documented carve-out) | Real DAWs use f64 runtime states or cascaded 1st-order sections at those corners | Before any LP/HP below 20 Hz is used at 96/192 kHz by the M1 analyzer/EQ |

## Closed (history — useful for understanding why certain rules exist)

| Date | Debt | Closed by |
|---|---|---|
| 2026-10-26 | `coeffs.rs`: missing a0 normalization (LP/HP/BP/notch/allpass/peaking/shelf all with the wrong out-of-band gain) | module rewrite with a single `Raw::normalize()` + DC/Nyquist tests; **fault**: formulas copied without checking the scale factor |
| 2026-10-26 | `Biquad::ramp` did not store the target → the final snap was a no-op, EQ permanently out of tune | explicit `target: Coeffs`, closing with `self.c = self.target`; exact test at 1e-6 |
| 2026-10-26 | AVX2 intrinsics used in safe fns without `#[target_feature]` → `SIGILL` risk on non-avx2 CPUs | `reset()`/`state()` marked `#[target_feature] unsafe` + `process_detected()` |
| 2026-10-27 | re-export `AudioGuard` for a type renamed `AudioSection` | export fixed; the lesson (rename = global grep) in CONVENTION §3 |
| 2026-10-27 | `Self::report` did not resolve: methods in `impl CheckedAllocator` (= `<System>`) invisible to `impl<A>` | `impl<A> CheckedAllocator<A>`; Rust note in [docs/it/rust.md](docs/it/rust.md) §1 (nominal vs structural) |
| 2026-10-27 | `_mm_getcsr/_mm_setcsr` deprecated in std | `asm!` with m32 operands (`stmxcsr [{}]`/`ldmxcsr [{}]`) |
| 2026-10-27 | clippy `unwrap_used/expect_used` at `-D` in Cargo.toml made `clippy --all-targets` fail (tests *must* be allowed to `expect`) | moved to `#![cfg_attr(not(test), deny(...))]` in the crate root |
| 2026-10-27 | **DF2T: `z2 = b2·x` was missing `− a2·out`** → wrong phase in every biquad (magnitude almost right!), *invisible* to the f64 reference test because the reference had the same error | fixed in scalar/reference/SIMD (`na2`); the behavioural test `measured_response_matches_predicted_response` is born here (sine-in amplitude/phase vs `Coeffs::response`); CONVENTION §4.1 is born here |
| 2026-10-27 | `BiquadNode` dead fields (`filt`, `ramp_frames`) | removed; dead code = a false sense of API |
| 2026-10-27 | **`SmoothedParam` asymptotic stall in f32**: below ~ULP/coef the increment rounds away and the one-pole never reaches the target (`settled()` lying, DSP never "sleeping") | snap in `step()` when the increment is below resolution; the test `smoothed_param_converges_and_settles` documents the theory; lesson: "in floating point, an asymptote does not exist" ([docs/it/dsp.md](docs/it/dsp.md) §10) |
| 2026-10-27 | Guard test **flaky**: process-global state (`PANIC_MODE`, `LAST_SECTION_ALLOCS`) read/written by parallel tests | the serialization mutex `serial()` in `alloc_guard::tests`; rule in CONVENTION §4.5 |
| 2026-10-27 | (TD-05) clippy/fmt never run — root cause: the `clippy`/`rustfmt` **components were not even installed**; every sandboxed session failed to write `~/.rustup` | components installed at top level, `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check` now green on both crates; CI (G1) will make them permanent |
| 2026-10-27 | (TD-06) no git repo, no LICENSE, no README | `git init` + remote `xGiova/Hathor`; official Apache-2.0 text from apache.org; English README with a per-module Status section |
| 2026-10-27 | (TD-16) Italian comments, doc-comments, assert strings and test names across the codebase; bilingual-`.md` policy contradicting itself | full mechanical migration: English code (no-logic-change proved by token-level diff, test counts unchanged), English-canonical docs, Italian only in `docs/it/`; STYLE.md §A is the single normative source |
| 2026-10-27 | Shelf (RBJ): the denominator terms used `2·alpha` instead of `2·sqrt(A)·alpha` → poles not mirrored on the zeros: subtly wrong out-of-band curve, and "Unstable" at large gains (a failing test caught it) | fixed in `coeffs.rs`; plus: `S=2` with gain above ~±24 dB in absolute value makes the argument of `α` negative — **no real filter exists for that combination**, so `design` now refuses with `FilterError::InvalidSlope` instead of clamping `α→0` (which would put a pole exactly on the unit circle) |
| 2026-10-27 | `design()` computing in f32: at 10 Hz/44.1 kHz `1−cos(w0)` ≈ 1e-6 while ulp(1.0) ≈ 1.2e-7 → a significant digit discarded before normalizing | `Raw` fields and the whole `design` math in f64, narrowing only in `normalize()` — setup-time code, zero cost on the audio path |
| 2026-10-27 | f32 coefficients carry a ghost DC gain (~0.1 dB) below ~50 Hz: `response(0)` sums `1 + a1 + a2` with catastrophic cancellation | `Coeffs::force_dc_unity()`: rescale `b` so its sum **shares** the same noisy `den` → ratio exactly 1 (the commercial EQ unity-DC trick: the representation error cannot be corrected, only shared). Applied only to kinds designed for unity at DC (LP/Notch/AllPass/Peak — never shelves, never HP/BP) |
| 2026-10-27 | `BUDGET_64_48K_US` triple contradiction: named µs, valued ms (1.33), documented as "number of frames" — and the docs claimed a block is "0.7 ms" at 48k/64 | true value 1333.33 µs + accurate doc; cost prose recomputed (~50 ns/frame redesign ≈ 0.2–0.3% of the block budget **per filter**, not "18–28%": a number propagated across three files without ever being divided by the budget — a number without a formula is a rumor) |
| 2026-10-27 | The DF2T bug **reappeared in documentation**: the `hathor-dsp-filters/src/lib.rs` headline sketch said `z2 = b2·x` without `− a2·out` | fixed; lesson: code sketches inside docs drift exactly like code — they must be re-verified whenever the algorithm is touched |
| 2026-10-27 | `SmoothedParam::set_time` computed `τ = time_s/(2π)` while its own doc promised 63% rise in `time_s` (flagged by the i18n agent, not by a test) | `τ = time_s` (one-pole time constant); doc explicit: 63% at τ, 99% at ~6.9τ; tests re-verified green |
| 2026-10-27 | **clippy's `vec_init_then_push` "fix" broke the guard tests in release**: `vec![7]` allocates through `Box`/`exchange_malloc`, whose failure path is **nounwind** — the guard's panic inside it escapes `catch_unwind`, kills the worker thread holding `serial()`, and the rest of the suite hangs. Debug never sees it (no inlining). | push-form allocation (`Vec::new()+push` → plain `__rust_alloc`, catchable) + targeted `#[allow]` with the reason + `examples/guard_probe.rs` keeps the proof runnable (`push` catches, `vec![]` aborts the unwind). Lesson: a lint suggestion that changes the *allocation path* can change the *unwind path* — the release run is the gate, not the debug one. |

## How this file is managed

- New debt: a row with a progressive ID, severity (blocks-gate / does not block /
  accepted), and a plan with an *explicit trigger* ("when consumer X arrives").
- Closed debt: **never deleted** — it migrates to "Closed" with the lesson. The register's
  value is double: the current inventory + the memory of the causes.
- Each gate G1-G4 closes only with its own TD-0x zeroed or deliberately re-classified
  (a re-classification is a `docs:` commit, not a disappearance).
