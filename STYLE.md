# STYLE.md — code style of this project

> Normative basis: **Google Rust Style Guide**
> ([google.github.io/styleguide/rustguide.html](https://google.github.io/styleguide/rustguide.html)),
> which itself follows the official Rust style guide + Rust API guidelines, with
> **rustfmt** as the formatting arbiter. Where this file records a *deviation*, the
> deviation is justified in §D. New rules require a documentation commit, not a silent
> convention change.

## A. Language policy (project decision, top priority)

| Artifact | Language | Notes |
|---|---|---|
| Identifiers (types, functions, **test names**) | English | Google guide spirit + open-source audience |
| Comments `//` and doc comments `///` `//!` | English | including `SAFETY:` and `TODO(#id):` prefixes |
| Assertion / panic / error *context strings* | English | the guide explicitly wants clarifying context strings |
| `docs/**/*.md` (except `docs/it/`), `ROADMAP.md`, `CONVENTION.md`, `TECH_DEBT.md`, `STYLE.md` | **English canonical** | one language for the engineering record; no bilingual mirroring |
| Teaching layer: `docs/it/*.md` (`dsp.md`, `rust.md`, `prior-art.md`) | **Italian only** | where we study; **not** translated and **not** duplicated — English docs link to it |
| Commit subject | English, Conventional Commits | `fix(hathor-dsp-filters): ...` |
| Commit body | English preferred; Italian allowed for reasoning | reviewable history |
| Bench reports (`bench/*.md`) | English tables and English prose | numbers are the language |

Migration of existing Italian comments/test names to this table was **TD-16: completed
2026-10-27** as a mechanical commit (`chore: i18n`) that did not mix with logic fixes.

The earlier scheme (every document written twice, English section first and Italian section
second) is retired: documentation is English canonical, and Italian lives only in the
teaching track under `docs/it/`.

## B. Formatting

- **rustfmt is the law** (`cargo fmt --check` in the M0 gate). No manual style
  debates. Stable rustfmt defaults already give Google's 100-column max width;
  nightly-only options (`imports_granularity = "Module"`) are adopted when feasible,
  otherwise applied by review.
- `#[rustfmt::skip]` only for **manual alignment** (coefficient tables, vector
  initializers) — the guide's only first-party exception — never for taste.
- Code blocks in doc comments must be marked `ignore` when they reference test-only
  or unsafe scaffolding that rustdoc would otherwise compile.

## C. The rules most binding on THIS codebase

Selected because they actually collide with how audio code gets written; the rest of
the Google guide applies as written (read it — this file is not a substitute).

1. **File organization**: leaf modules named after their primary type
   (`buffer.rs` → `Block`, `param.rs` → `AtomicParam/SmoothedParam/LinearRamp`),
   non-leaf modules (`lib.rs`) contain only `mod` + `pub use` re-exports.
   `use super::*` is allowed only inside `mod tests` (guide exception).
2. **API shapes**: `&[T]` not `&Vec<T>`, `&T` not `&Box<T>`; small `Copy` structs
   (`Coeffs`, `ProcessCtx`, `Shape`) cross APIs by value; `T: Trait` preferred over
   `dyn Trait` *except* the documented `dyn Node` case (§D.2).
3. **Getters/setters follow the standard names**: `foo()`, `foo_mut()`, `set_foo()`,
   `take_foo()`, `with_foo(closure)` — the arena `with(id, closure)` pattern of
   `hathor-host-graph` is *mandated* by the guide's "accessors that accept closures".
4. **Constructors**: `new()` returns `Self` or `Result<Self, E>` (`try_new()` only
   when both exist); conversion constructors named `from_*` paired with `From` impls.
5. **Error reporting**: `Result` for recoverable failures (bad parameter, buffer too
   small); `panic!` only for programmer-invariant violations — which in this project
   means the allocator guard (§D.1). Error types implement `Copy` where possible
   (`NodeError`, `FilterError` do: `&'static str` payloads, no heap).
6. **`unsafe`**: encapsulated in safe abstractions; every `unsafe` block carries a
   `// SAFETY:` line with the technical justification; `unsafe_op_in_unsafe_fn` stays
   `deny` at the workspace level. The guide requires benchmarks as part of the
   justification — `hathor-audio-probe` numbers are cited inside the unsafe modules.
7. **No unstable features**, stable toolchain pinned by `rust-version` (MSRV 1.85).
8. **Public enums/structs across crate boundaries**: `#[non_exhaustive]` where the
   API can grow (`FilterKind`, `NodeError`, `AudioSectionError` — tracked as TD-17).
9. **Documentation**: `missing_docs` warning stays on; every public item has `///`;
   modules have `//!`; doc comments must add what the code cannot say — the guide
   bans `/// Removes an element. / pub fn remove_element()`.
10. **Tests**: one framework per codebase = plain `#[test]` (no external harness until
    a real need exists); context strings on every assert (`assert!(cond, "why")`)
    — English since the TD-16 migration; deterministic inputs (custom `Lcg`, never
    `thread_rng`).
11. **Concurrency**: no `async`/`await` (Chrome rule adopted): the audio callback is
    a hard-realtime context and worker tasks run on dedicated threads with channels.
    "Handling poison": the guide forbids reaching inside a poisoned mutex — our test
    serialization mutex uses `into_inner` **only** on a `Mutex<()>` whose poison
    carries no data (documented deviation §D.4).

## D. Deliberate deviations (with justification)

| # | Deviation | Why, and where the guide itself has room |
|---|---|---|
| 1 | **`panic::catch_unwind` is called** (the guide says don't) | Only in tests of the allocation guard. The whole point of the guard is that a violation must *abort the block, not the process*, and a negative test must prove the panic exists. Guide exception exists for "codebases may choose to avoid panics entirely"; we document the choice. Never in library code outside `#[cfg(test)]`. |
| 2 | **`dyn Node` in the graph** (guide prefers generics) | Graph nodes are heterogeneous at runtime and chosen by the user; per-block vtable cost (~5 ns, **expected**) is amortized over 64–2048 frames. Monomorphizing an entire user-defined graph is impossible. The guide's own rationale concedes code-size and dispatch-when-necessary cases. |
| 3 | **`Rc`/`Arc` are used** (guide: avoid) | Exactly one pattern: double-buffered immutable state UI→audio (`Arc<[f32]>` swap). The alternative (locks) violates the realtime contract; the guide's exception "unreasonably complex to write/maintain otherwise" applies with `Cell`-free cross-thread constraints. |
| 4 | **Poisoned test mutex unwrapped via `into_inner`** | `Mutex<()>` holds no state; a panicking test cannot corrupt "nothing". §11's rule protects shared *data*; documented, scoped, test-only. |
| 5 | **Italian prose in the teaching docs (`docs/it/*.md`)** | The guide governs *code* language. Project docs are English canonical (§A); Italian survives only in the teaching track under `docs/it/`, which is deliberately neither translated nor duplicated. |

## E. What this changes right now

- New code follows A–D immediately.
- The filters-crate `fix` patches landed with **English** comments and English context
  strings; the former Italian files migrated in one mechanical `chore: i18n` commit
  (TD-16, completed 2026-10-27) reviewed as pure rename.
