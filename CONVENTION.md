# CONVENTION.md — the rules of this project

These are not rules for rules' sake: every rule here exists because it has already bitten
at least once (see [TECH_DEBT.md](TECH_DEBT.md)) or because without it the project *cannot*
reach its gates (see [ROADMAP.md](ROADMAP.md)).

## 1. Languages (normative source: [STYLE.md](STYLE.md) §A)

| What | Language | Why |
|---|---|---|
| Identifiers, APIs, types | English | market standard; `BiquadNode`, not `NodoBiquad` |
| Test names | English | a test names the **behaviour it defends**: `audio_section_resets_state_after_panic` is a readable title, unlike `test_guard_state_reset_on_panic_v2`, which names nothing |
| Comments `//` and doc comments `//!`/`///` | English | including `SAFETY:` and `TODO(#id):`; comments explain the *why*, never the *what* (the code already says the what) |
| Assertion / panic / error context strings | English | the context string is part of the failure's API |
| Documentation: `docs/**/*.md` (except `docs/it/`), `ROADMAP.md`, `CONVENTION.md`, `TECH_DEBT.md`, `STYLE.md` | English canonical | one language for the engineering record |
| Teaching docs: `docs/it/*.md` | Italian only | the teaching layer lives here and is **not** translated or duplicated; English docs link to it |
| Commit message | subject English (Conventional Commits); body English preferred, Italian allowed for reasoning | subject = standard; body = the reasoning for whoever rereads in six months |

Where this table and [STYLE.md](STYLE.md) §A disagree, **STYLE.md wins** — it is the
normative source for the language policy, including the Google-style basis and the
deviations (§D).

## 2. The realtime contract (the law, not a guideline)

1. `Node::process`: **zero allocations, zero locks, zero I/O, zero syscalls**.
2. Allocations only in `setup()` (and there the code *declares* them with a comment).
3. UI→audio parameters: `AtomicParam` (bit-cast, lock-free). Discrete events: SPSC ring
   (`rtrb`). Never a `Mutex` on the audio path.
4. Errors on the audio path: `NodeError` carrying a `&'static str` — allocating to report
   an allocation failure is the paradox this type forbids by construction.
5. The guard (`CheckedAllocator` + `AudioSection`) makes the contract **mechanical**: every
   binary that processes audio installs the guard; every processing test runs inside
   `AudioSection`. A verifiable contract is worth ten documented ones.

## 3. Lints and tooling

- `unsafe_op_in_unsafe_fn = "deny"` everywhere. **It already found a bug** (see the story
  in [docs/modules/hathor-dsp-filters.md](docs/modules/hathor-dsp-filters.md)): that is not pedantry, it
  is automatic review.
- `unwrap_used`/`expect_used`/`exit`/`todo`: `deny` **outside tests**
  (`#![cfg_attr(not(test), deny(...))]` — inside tests an `.expect()` with a message is
  documentation).
- `#![warn(missing_docs)]` on every public crate.
- M0 gate: `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean.
- `rustfmt`: never formatting whack-a-mole inside a diff — if the formatter touches a
  file, it touches the whole file in the same separate commit (`chore: fmt`).

## 4. Tests: how DSP is validated

1. **Every DSP module has a behavioural test independent of the implementation.**
   Comparing "code A vs code B written by the same head" is *not* a verification: the
   history of the `−a2·out` bug proves it (two identically wrong implementations agreeing
   with each other). Valid references: closed forms (RBJ, Parseval), external libraries
   (`rustfft` as a dev-dependency), measurements (sine in → amplitude/phase out against
   `Coeffs::response`).
2. **Numbers, not judgements**: "the filter must have −3.01 dB at f0" is a test; "it
   sounds good" is not. The roadmap gates are all measurable (null residue < −30 dB,
   headroom ≥ 1.5 dB, 0 xruns in 60 s).
3. **Justified tolerances**: FMA rounds once, mul+add twice → the SIMD kernel is *more*
   precise than the scalar reference, and the test says "they converge within X", not
   "bit-exact".
4. **Deterministic RNG in tests** (`Lcg`): a numerical test that is not reproducible is not
   a test.
5. **Process-global state** (allocator, TLS, MXCSR): every test that touches it takes the
   serialization mutex (the `alloc_guard` doc-test pattern). `cargo test` is parallel by
   default: global state is not.
6. 90% of DSP bugs live at the **edges**: DC, Nyquist, q→0, NaN, gain ±24 dB, an
   interrupted ramp. Edge-case test grids come before the central cases.

## 5. DSP: structural choices (not renegotiable without an ADR in docs/architecture.md)

- **Planar layout** (channel-contiguous) as the universal format; interleaved only at the
  edges (driver, lane-SIMD kernels). Conversions are paid for and **measured**, not hidden.
- **Latency always declared** (`Node::latency_samples`, from day 1). Declared ≠ group
  delay: the two quantities are not to be confused ([docs/it/dsp.md](docs/it/dsp.md) §5).
- **Coefficients are ramped, not jumped**: zipper noise is a perceptible bug.
- **Recursion = per-channel, not per-frame**: IIR filters do not vectorize in time. Any
  SIMD optimization that ignores this fact is fake.
- Parameters travel **at block rate**; audio-rate arrives only when a real consumer demands
  it (M8/synth), never "but one day".
- Every performance number in every document is labelled **measured** or **expected**.
  Benchmark reports are committed (bench/), not narrated.

## 6. Commits and branching

- **Conventional Commits**: `feat:`, `fix:`, `docs:`, `test:`, `perf:`, `chore:`.
  Scope in the subject when useful: `fix(hathor-dsp-filters): DF2T missing the −a2·out term`.
- One commit = one area/decision. `fix` commits cite the test that now nails them.
- Single developer: work on `main` for small things, branch for anything touching
  `unsafe`, threading or the ABI (`rt/`, `ffi/`). Merge: fast-forward or squash, linear
  readable history.
- Tags: `v0.x.y` at every gate (G1…G5) reached, with the gate's numbers in the message.
- CI (`.github/workflows/ci.yml`) runs the same four gates as the local ones — fmt, clippy
  `-D warnings`, tests debug **and release** — on every push/PR to `main`, plus an MSRV job
  at the pinned 1.85. A red CI blocks the gate: "green on my machine" is not evidence. The
  G1 hardware run is intentionally NOT in CI (runners have no audio device): it is a
  measured human-present session whose numbers get committed with the `measured` label.

## 7. Licenses and prior art (see [docs/it/prior-art.md](docs/it/prior-art.md) §4)

- Our code: **Apache-2.0** (patent grant included, relevant for audio DSP).
- SDKs/dependencies in `external/` with *their* license and one line in the README listing
  them. **The ASIO SDK is never committed** (not redistributable; gitignored, opt-in
  `--features asio`).
- **Never** decompile/derive from binaries whose EULA forbids it. Black-box measurements
  stay private (calibration), not advertised against a vendor.
- GPL code is read *to understand*, never copied; reuse only from permissive (MIT/Apache).

## 8. Documentation

- Every crate that lands as code **brings its own `docs/modules/<name>.md` in the same
  commit** (template in [docs/README.md](docs/README.md)). A module without a document is
  debt (TD).
- Architecture decisions → the table in [docs/architecture.md](docs/architecture.md); the
  *non-decisions* (what we defer and why) are written down too — they are the hardest to
  remember.
- Fixed bugs leave a **trace in the test** (a name that tells the story) and in the module
  doc (short history): the value of a bug is the lesson you extract from it.
- The teaching track (`docs/it/`) stays Italian: it is the layer where we study, not a
  translation target. English documents link to it instead of duplicating it.

## 9. Compatibility

- MSRV = 1.85 (bound by `cpal`). Target: `x86_64-pc-windows-msvc` today; the code *must
  not* make portability impossible (no `std`-only Windows APIs in the core), but nothing is
  optimized for undeclared platforms.
- `ABI_CONTRACT_VERSION` (hathor-dsp-core): bumped **only** for a real break of the
  `Node`/parameter contract; each break = one ADR.
