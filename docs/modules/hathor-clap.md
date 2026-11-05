# Module: `hathor-clap`

> Status: **written, tested 28** — 25 unit + 2 integration + 1 doctest (debug+release,
> clippy `-D warnings`, fmt; the integration pair is the mock-host lifecycle proxy).
> Dependencies: `clap-sys` 0.5.0 (the ABI truth, re-exported as `hathor_clap::sys`) and
> `hathor-dsp-core` as a **normal dependency — approved 2026-11-05**: translating
> `clap_audio_buffer` into the core `Block`/`ProcessCtx` ABI *is* this wrapper's job, so
> its public surface legitimately exposes those types (the declared exception to the
> "core is dev-dependency only" pattern of the other crates).

## Problem

The plugins must leave the DAW to prove themselves in Reaper/Bitwig before any host of ours
exists. That needs CLAP — but a general CLAP library is not the need (and not what this is):
the scope, stated honestly from `lib.rs`, is **the wrapper for *our* plugins** — solo author,
Windows-only. It owns the raw ABI (entry/factory/instance vtables, extension structs,
audio-buffer mapping) and nothing else: **no DSP, no GUI (there is no GUI anywhere in this
project), no plugin implementation**. Consumers implement one safe trait and a `static`
descriptor; `clap_entry!` wires them to a host. The thinness is the point: every line of
`unsafe` this crate writes is a line the plugin authors will never have to read.

## Contract

The host's call order, as the wrapper understands CLAP 1.2 (from `entry.rs`, and what
`tests/mock_host.rs` drives end to end):

`entry.init(plugin_path)` → `entry.get_factory("clap.plugin-factory")` →
`factory.get_plugin_count()` / `get_plugin_descriptor(i)` / `create_plugin(host, id)` →
per instance: `plugin.init()` → add extensions → `activate(rate, min, max)` →
`start_processing()` → `process(&block)*` → `stop_processing()` → `deactivate()` →
`destroy()` → `entry.deinit()`.

* **The mock-host lifecycle test is the PROXY of host loading, not the proof.** The
  integration pair drives the whole C-ABI against a minimal mock host — asserting zero
  allocations on the process path under the workspace `AudioSection` in STRICT mode — and it
  is the official proxy for "the plugin loads in a host". **The real load in Reaper/Bitwig is
  an OWNER step after the plugin task (task-4); no CI can replace it, and nothing in this
  crate claims it happened.** Host-behavior assumptions (pointer stability, call ordering)
  are labeled *expected, NOT VERIFIED* at each site in the sources; the mock asserts them
  only for ours.
* **Init policy, declared** (`entry.rs`): a second `init` without a `deinit` is host misuse —
  the wrapper answers `false` (refuse, do not pretend success; the interpretation of `false`
  by real hosts is expected, NOT VERIFIED). `deinit` → `init` again is **allowed**; the state
  is one `AtomicBool`, and `plugin_path` is never dereferenced (it is required to be
  well-formed if non-null, and nothing else). `is_initialized()` is exported for the plugin
  task's sanity checks and is *never load-bearing* inside the wrapper.
* **Foreign ids answer null, not a panic**: `get_factory` compares the id against the one
  `CLAP_PLUGIN_FACTORY_ID` and never dereferences blindly — a null `factory_id` is a legal
  "who are you?" and is refused cleanly; `create_plugin` refuses ids that are not the
  descriptor's (`create_refuses_foreign_ids_without_entry_state`).
* **Audio arrives planar.** CLAP hands one `clap_audio_buffer` per port: `data32` is an array
  of `channel_count` pointers, each channel's frames **contiguous f32** — channels need not
  be contiguous with each other. That is exactly the guarantee `hathor_dsp_core::buffer::Block`
  needs of a single-channel view, so the bridge is **zero-copy**: each channel maps as
  `Block::from_slice(channel, Shape::new(1, frames))`. Inputs are exposed as *read-only*
  slices on purpose: a mutable `Block` over input memory would let two unique views of the
  same frames exist. The one unavoidable copy is *into* the host's output arrays — and hosts
  can make even that a no-op via in-place pairs (`ProcessView::out_aliases_in`).
* **MIDI never enters through params.** The CLAP 1.2 `clap.params` vtable is **six** functions
  (count, info, get, set, value→text, text→value — `abi.rs` pins its size at 48 bytes =
  exactly six slots); `flush` is a `clap_plugin` method, not a params slot. **There is no
  `get_midi` in any released CLAP** — it is a pre-1.2 draft API, and the wrapper corrects the
  myth at the source (the `Plugin::flush` doc in `plugin.rs` says so verbatim) so it cannot
  reappear in M4/M5 planning: MIDI arrives as **events**, through `flush` like everything
  else. Today the wrapper reads the one core event type it has a consumer for
  (`clap_event_param_value`) and transport; wider event plumbing arrives with its first real
  consumer, not before.

## API

Module map (visibility is deliberate: the raw-ABI modules are public only where a plugin
binary must name them):

| Module | Public? | Owns |
|---|---|---|
| `abi` | pub | const size/alignment gates for every `#[repr(C)]` struct touched (see Invariants) |
| `cstr` | private (items re-exported) | `clap_cstr!`, padded-name read/write into host fixed arrays |
| `descriptor` | private (re-exported) | `make_descriptor` — the `const` builder (the 9 arguments *are* the ABI, the 9th being `Option<FeatureList>`) + `FeatureList`, whose `const fn new` enforces the `clap.h` list law at compile time |
| `entry` | pub | `init_impl`/`deinit_impl`/`factory_for`, the `clap_entry!` macro, `is_initialized` |
| `events` | private (types re-exported) | `InEvents`/`OutEvents`/`ParamUpdate` — call-scoped event views |
| `host` | private (types re-exported) | `Host` (the out-bound calls) + `LogSeverity` |
| `plugin` | pub | the safe `Plugin` trait, `Instance<P>`, the vtable trampolines, `AudioPortInfo`/`ParamInfo`/`ParamText`/streams |
| `process` | private (types re-exported) | `ProcessView`/`ProcessStatus` — where CLAP memory meets `hathor-dsp-core` |
| `sys` | pub (re-export) | raw `clap-sys` 0.5.0, so plugin binaries name the entry static's type without depending on `clap-sys` themselves |

The whole plugin binary — the shape `hathor-analyzer` actually ships (safe Rust plus ONE
macro call; task-5 made the macro emit the exported static itself):

```rust
struct MyPlugin;              // impl hathor_clap::Plugin — no unsafe, no raw pointer seen
static DESC: clap_plugin_descriptor = hathor_clap::make_descriptor(/* id, name, vendor, … */);
hathor_clap::clap_entry!(entry_gen, MyPlugin);  // emits #[no_mangle] pub static clap_entry BY VALUE
pub use entry_gen::clap_entry;                   // optional: a Rust path for tests/drivers
```

**One `clap_entry!` call per crate**: the symbol name is fixed by the ABI, so the macro emits
it by value at that address (what a host's `dlsym` must land on) — a second call in one crate
is a link error, by design. One library, one entry.

`Plugin` requires five functions plus the descriptor const — `DESCRIPTOR`, `instantiate`
(must not talk to the host), `init(Host) -> bool`, `activate`, `deactivate`, and
`process(&mut ProcessView) -> ProcessStatus` — everything else defaults to an honest
"no extension" answer (0 params, one main stereo port each way, latency 0, realtime-capable),
so a minimal plugin stays minimal. `flush` defaults to feeding every `ParamUpdate` to
`set_param_value`; override for per-frame scheduling. **Log is host-only:** `clap.log` is an
extension the host exposes and the plugin *calls into* (`Host::log`, severity mapped to the
CLAP constants) — no plugin-side log extension exists in CLAP, and this wrapper invents
none. `Host` is `Copy` (one pointer) and
`Send` (a plugin created on the main thread may log from the audio thread) but deliberately
**not `Sync`** — the wrapper will not promise a host's vtable is reentrant from two threads
(hosts vary; expected, NOT VERIFIED). `ProcessView` maps frames, sample rate, `ctx()` (the
core `ProcessCtx`), `transport_snapshot()` (the core `Transport`), per-channel slices/blocks
both ways, `constant_mask` (exposed, **not interpreted**: every frame is processed
regardless — correct, not optimal), and per-port `latency`. `ProcessView::from_sys` is
**`pub const unsafe fn`** — public since task-5, the documented way to wrap a `clap_process`
OUTSIDE a host callback (which is how a consumer's tests drive the real `Plugin::process`).
Its Safety contract is spelled out: every buffer row non-null and sized by `frames_count`,
channel counts coherent with the arrays, everything alive and stable for `'a`. Inside a
callback the wrapper discharges this from the vtable contract; outside one, the caller
assumes the verification burden exactly like the host would — a mistyped `channel_count`
there is UB downstream, not a panic.

## Invariants

1. **No allocation on any entry/init/deinit path** — the only atomic, no heap. The audio
   contract holds where it must: `process` allocates nothing (asserted inside `AudioSection`
   STRICT by the mock lifecycle test, recipe below).
2. **The instance is recovered ONLY through `plugin_data`.** `Instance<P>` is a leaked
   `Box<…>` whose address the trampoline stores as `plugin_data`; the `base` header the host
   holds points *somewhere inside the same box* — and because `Instance` is `repr(Rust)`,
   `base` sits at a **compiler-chosen offset: measured 64, never assumed**. The
   heap-corruption bug that produced this rule *assumed offset 0*: it is the living proof,
   written into `plugin.rs`, that **no wrapper field may ever be located by a layout
   assumption**. `destroy` is the single reclaim point (`Box::from_raw`), after which no
   pointer into the instance is used again.
3. **Layout is a compile error, not a runtime surprise.** `abi` const-asserts the
   size/alignment of every struct the crate reinterprets (entry 40, factory 24,
   `clap_plugin` 96 — with desc at offset 0 and `plugin_data` at 8 — host 88, audio buffer
   32, process 64, param info 1320, port info 288, event header 16, wide param event 56,
   transport 104, the five plugin-side extension vtables, both stream types). A silent field
   change in `clap-sys` fails the **build**, and `layout_matches_measured_numbers` echoes the
   same numbers at runtime so a failure names the struct.
4. **Unsafe confinement is per-module and written down** — mirroring the `lib.rs` header
   (the STYLE §6 rule, same argument as the probe's `pump.rs`): `plugin` — the one leaked
   box, the `plugin_data` recovery, `destroy` as the unique reclaim; `entry` — the boundary
   `extern "C"` functions, null-safe string compare, no allocation on init paths; `host` —
   the borrowed `*const clap_host`: extensions fetched per call and **never cached**, every
   vtable slot null-checked (hosts legally omit them); `process` — host-owned arrays
   reborrowed into `ProcessView<'a>`, confined to the call's lifetime by the borrow itself,
   null `data32` and out-of-range indices answer `None`, never unwrap; `events` — a wide
   event is read only after its own header says **size, type and space match**, and only
   after alignment is verified ("a host that misaligns events gets no crash from us").
   Lint teeth: `unsafe_op_in_unsafe_fn = "deny"` (every inner unsafe call needs its own
   block and SAFETY comment), plus `unwrap_used`/`expect_used`/`todo`/`exit` denied in
   non-test code and `missing_docs` warned.
5. **Descriptors are `static`, never heap** — the host keeps the pointer for the life of the
   library, so a heap descriptor is either a leak or a dangling promise; `make_descriptor`
   is `const` for exactly that reason — and it now takes a ninth argument,
   `Option<FeatureList>`. A declared list is a `FeatureList`: its `const fn new` verifies the
   `clap.h` law — non-empty, last row NULL — **at compile time, so a malformed list never
   finishes assembling the descriptor**. The usage note, pinned where it bites: the
   raw-pointer array cannot itself be a `static` (pointers are not `Sync`), so the list is
   built **inline in the static's initializer** (const promotion); `FeatureList::new(&[…])`
   in *runtime* position is E0716, and the `DESC_FEAT` static in `descriptor.rs` is the
   shape that documents the rule. An empty list still publishes `features = null`
   (the `clap.h` rule reads NULL as the empty terminator; **a host treating null as
   malformed would be a plugin-loading bug we have not observed — NOT VERIFIED**).

## Verification (tests, not opinions)

* **25 unit** — `abi` 2 (the layout echo, the field-zero rule), `cstr` 3 (padded names
  terminate inside the host's array, the empty string, `clap_cstr!`'s static NUL),
  `descriptor` 3 (every field round-trips from its own `static`; address stability; a
  declared list survives assembly and **terminates NULL per clap.h** —
  `feature_list_survives_assembly_and_terminates`),
  `entry` 2 (`init_deinit_cycle_refuses_double_init`, `factory_answers_only_the_clap_factory_id`),
  `events` 4 (null lists are no-events not wrong-answers; foreign spaces/short headers
  skipped in order; missing vtable slots read empty, never panic; pushes go through the
  host's `try_push`), `host` 3 (null host rejected at the door, equality is pointer
  equality, severities map to the CLAP constants), `plugin` 3 (`format_f64` short-and-exact,
  padded-array writes, `ParamText` bounded by the host's capacity), `process` 5 (the
  no-copy bridge **proven** — `block_view_writes_land_in_the_hosts_own_arrays`; inputs are
  reads and out-of-range answers `None`; `ctx` maps the core contract; transport flags map;
  in-place pairs are reported).
* **2 integration** — `mock_host_full_lifecycle_and_alloc_contract`: the whole call order
  through `extern "C"`, sine in, output touched & non-NaN, then the allocation recipe:
  **warm-up `process` OUTSIDE the section first (lazy init must not hide inside the gate),
  then the timed blocks inside `AudioSection::enter()`, asserting `allocs_here() == 0` — two
  separate sections (process path, flush path)**. Serialized against the sibling test by an
  explicit mutex, because the entry flag is process-global.
  `create_refuses_foreign_ids_without_entry_state` deliberately touches no global and runs
  in parallel — the asymmetry is documented in the file.
* **1 doctest** — the `clap_cstr!` example in `cstr.rs` (a bare ` ``` ` block: it compiles and
  runs in `cargo test`, debug and release).

## Deferred (and why)

* **Interleaved audio: a declared choice, not a hole.** Interleaved is not a CLAP wire
  format at all — `data32` is planar by contract — so the wrapper maps planar and nothing
  else. A future host that only offers interleaved would need a *conversion* entering here
  (`process.rs` names it); it is deferred until one exists, not forgotten.
* **Port positioning**: audio ports advertise nothing beyond main stereo (surround/ambisonic
  types wait for a plugin that has those ports).
* **`constant_mask` optimization**: exposed, not interpreted; processing constant channels
  frame-by-frame is correct, and "no host we have tested sends one" is labeled *expected,
  NOT VERIFIED* at the site.
* **The `Node` wrapper lives in `hathor-host-graph`, the plugin lives in the consumer crate;
  the bridge between a live CLAP instance and the DAW graph is task-4's, not this crate's.**
  Real-host loading in Reaper/Bitwig is an **owner step**, not a deferred engineering item:
  it cannot be proxied away (see the Contract's bold rule).

## Honesty ledger

The proxy-vs-proof line is the crate's most important sentence and appears in the same words
in `lib.rs`, in this doc, and in the root README row: **mock-host green ≠ loads in Reaper**.
Every host-behavior assumption carries its *expected, NOT VERIFIED* label at the site (pointer
stability, call ordering, init-refusal interpretation, features-null reaction, quiet-host
behavior). The `repr(Rust)` offset lesson is recorded with the number it produced (64,
measured) rather than sanitized. The `hathor-dsp-core` normal-dependency is flagged in
Cargo.toml as an approved exception with its reason, not quietly. Test counts here are the
counts the workspace gate runs (24+2+1 = 27); this crate has **no bench target** — the cost
claim for a wrapped plugin belongs to `hathor-audio-probe`'s bench culture, and nothing here
pretends to have measured what it has not.
