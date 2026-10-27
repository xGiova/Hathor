# Module: `hathor-dsp-core`

> Status: **written and tested** (M0, 20/20 green debug+release). Dependencies: none. Dependents: everything else.

## Purpose

It contains no DSP. It contains the **contracts** that make DSP safe: the planar buffer,
the `Node` trait, lock-free parameters, the anti-allocation guard, denormal handling. It is
layer L1 of [architecture.md](../architecture.md): every other crate depends on here, and
this depends on no one — not even on `std` beyond the strict minimum (the TLS and the
atomics).

If this module is wrong, every module above inherits the error. That is why it is small.

## How it works

### `buffer.rs` — the audio block

```
ChannelBuffer (owner, allocates in setup)
   └── block() ──▶ Block<'a> (view, zero cost, NEVER allocates)
```

* **Planar layout**: all samples of channel 0 contiguous, then channel 1, and so on. One
  flat `Vec<f32>` + `Shape{channels, frames}`. The per-channel views are
  `chunks_exact_mut(frames)`: zero allocations, zero bound checks in the release loop.
* Why planar and not interleaved: typical DSP loops walk *one channel at a time* (one
  biquad per channel) and vectorization wants contiguous memory. Interleaved is only needed
  at the edges (driver) and by the lane-SIMD kernel (see `hathor-dsp-filters`).
* `copy_from`/`mix_add` handle different shapes (partial mono→stereo) without panicking:
  they take the `min` of the two. This is deliberate: a mono node in a stereo chain is a
  use case, not an error.

### `node.rs` — the processor contract

```rust
trait Node {
    fn setup(&mut self, ctx, hints) -> Result<(), NodeError>;  // the ONLY place where allocation happens
    fn process(&mut self, ctx, block) -> Result<(), NodeError>; // zero alloc, zero lock
    fn latency_samples(&self) -> usize;                         // PDC: from day 1
    fn set_param(&mut self, id, value);                         // numeric id, no String
    fn reset(&mut self);
}
```

* `ProcessCtx` is `Copy` and carries `sample_position: u64`: the truth of time
  (architecture §3) travels *inside* every call, not as hidden global state.
* `NodeError` carries a `&'static str`, never a `String`: allocating to say "I could not
  allocate" is the paradox this type forbids by construction.
* `latency_samples()` vs `max_latency_samples()`: the second lets the graph pre-allocate the
  compensation delay lines at the maximum possible (a delay whose time changes must not make
  the PDC recompose at runtime).

### `param.rs` — UI→audio without locks, and ramps without clicks

Three different problems, three tools:

| Problem | Tool | Mechanism |
|---|---|---|
| The UI writes, audio reads, without a mutex | `AtomicParam` | `AtomicU32` + `f32::to_bits/from_bits`; store `Release` / load `Acquire`; on x86 = one move |
| The value must not jump (zipper) | `SmoothedParam` | one-pole `v += coef·(target−v)` per frame; `coef` computed in `set_time` (setup), not per frame |
| It must arrive *exactly* on a given frame | `LinearRamp` | constant step, forced closure on the target at `remaining == 0` (no f32 residue) |

`ParamId(u32)`: parameters are identified by a stable number, never by name — the
serialization (M5) will map numbers↔names exactly once.

**The f32 stall (bug found by the tests)**: the one-pole `value += coef·(target−value)` in
f32 *never converges* — when the increment drops below the ULP of `value`, the addition
rounds back and the value stays parked ~ULP/coef away from the target (≈2e-6 for coef≈0.026).
A parameter permanently "in transition" keeps the DSP awake and makes `settled()` a lie.
`step()` therefore **snaps** to the target when the increment is below resolution: a ~1e-6
relative deviation, inaudible, and real convergence. The general lesson lives in
[../it/dsp.md](../it/dsp.md) §10: *the asymptote does not exist in floating point*.

### `alloc_guard.rs` — rule nº1 becomes mechanical

Per-thread state in a `thread_local!` that is **const-init** as a `Cell<u8>` (no Drop → no
hidden allocation, no TLS destructor — see [../it/rust.md](../it/rust.md) §5):

```
S_IDLE ──enter()──▶ S_AUDIO ──(alloc!)──▶ S_REPORTING ──panic──▶ unwind ──Drop──▶ S_IDLE
```

* Every `alloc` of the `CheckedAllocator` reads the TLS state: one read, ~1 cycle (expected).
* In `S_AUDIO`: it counts (TLS `Cell<u64>`), and if `PANIC_MODE` → panic. The state moves to
  `S_REPORTING` **before** the panic because the panic machinery allocates: without that
  switch, infinite alloc→panic→alloc recursion.
* `Drop for AudioSection` publishes the count into the global atomic and returns to `S_IDLE`
  even during unwinding (the test `audio_section_resets_state_after_panic` exists exactly
  for this).
* A nested `enter()` = `Err(AlreadyInside)`: nesting sections masks the counts, it is a
  caller bug, not a legal state.
* `dealloc` is **not** checked, deliberately: a free without a previous allocation in the
  block is legitimate (buffers prepared by the UI). The counter measures allocations, which
  are the thing that breaks realtime.
* **Negative tests must allocate through the unwind-safe path**: `vec![..]` goes through
  `Box`/`exchange_malloc`, whose failure path is nounwind in release, so the guard's panic
  escapes `catch_unwind`; `Vec::new()+push` (`__rust_alloc`) is catchable — the runnable
  proof is `examples/guard_probe.rs` (`cargo run --release -p hathor-dsp-core --example
  guard_probe -- vec|push`): green in debug proves nothing about unwinding, the release run
  is the gate.

### `denormal.rs` — one register write, one production bug less

`stmxcsr`/`ldmxcsr` via `asm!` (the `_mm_getcsr` intrinsics are deprecated): sets FTZ
(bit 15) + DAZ (bit 6) **once per audio thread**. `count_denormals` exists for the reverb
regression tests (M4): an FDN that produces denormals is an irreproducible performance drop.

## Invariants (violating one = bug, not style)

1. No method of `Block`/`View` allocates. If you add one that has to, it does not belong to
   `Block`.
2. `Node::process` does not allocate, does not lock, does no I/O. The guard verifies it at
   runtime; the tests of every crate assume it.
3. `ProcessCtx`/`Shape`/`Coeffs`/errors are `Copy` or static: they cross the callback by
   value.
4. The guard is **process-global** (one `#[global_allocator]` per binary): the binary that
   wants the enforcement installs it; libraries cannot.
5. `disable_denormals()` is called on the audio thread at startup, never per block (it is a
   CSR write, ~20 cycles (expected): not huge but not free, and per-block it is noise).

## Rust concepts embodied

`thread_local!` const-init (§5 of [docs/it/rust.md](../it/rust.md)) · the `GlobalAlloc` trait · `Cell` vs
atomics vs `Mutex` (§4) · `Drop` as a cleanup guarantee even during unwinding · the
`f32↔u32` bit-cast for atomics · `Result` with static errors · the `unsafe_op_in_unsafe_fn`
lint (the `asm!` in denormal.rs is the crate's only raw unsafe, with a SAFETY comment; the
`GlobalAlloc` impl in alloc_guard.rs only forwards to the wrapped allocator).

## DSP concepts embodied

Denormal/FTZ/DAZ (§10 of [docs/it/dsp.md](../it/dsp.md)) · declared latency vs group delay (§5) · zipper
noise and why parameters are ramped (§4) · lock-free as a realtime requirement, not as an
optimization.

## Deferred (and why)

* **`Scratch`/the graph's buffer arena** → M1: it is needed when the graph has nodes with
  more inputs (sidechain, send). The design (a `BufferId` handle + access through a closure)
  is sketched in [hathor-host-graph.md](hathor-host-graph.md); building it now would mean guessing the API
  without a second real consumer.
* **NaN watchdog** → M5 (with the real graph): the check is trivial, the *policy*
  (mute? report? bypass?) must be decided together with the host.
* **Audio→UI telemetry** (peak/RMS meters from the callback) → M1 with the analyzer: same
  channel (atomics + ring), but it only makes sense once there is something to measure.

## Exercises

1. Add an `#[test]` that enters an `AudioSection`, calls `Box::new(0u8)` and verifies that in
   measure mode the counter is ≥1 and in strict mode it panics. (It already exists
   partially: make it explicit for `Box`, not only for `Vec`.)
2. Make `LinearRamp` bipolar-safe: what happens if `glide_to` is called again in the middle
   of a ramp? (Hint: look at how `Biquad::ramp_to` handles the same case.)
3. Measure with `std::time::Instant` the cost of 1M guard TLS reads against 1M reads of a
   `static AtomicU8`: quantify why TLS wins (hint: shared cache line = false sharing between
   cores).
