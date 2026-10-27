# Architecture

> System document. Per-module details live in [modules/](modules/).

---

## 1. The layers

The project is a stack of 6 layers. **The only dependency rule that matters: a layer may
know only the layers below it.** There are no callbacks from bottom to top except through
explicit queues.

```
┌──────────────────────────────────────────────────────────────────┐
│ L5  UI            arrange · mixer · editor · shortcut system     │  never realtime
├──────────────────────────────────────────────────────────────────┤
│ L4  HOST ENGINE   graph, transport, PDC, recording, project      │  real thread here
│                   state/serialization, offline render            │
├──────────────────────────────────────────────────────────────────┤
│ L3  PROCESSOR     Node (internal) · CLAP · 3rd-party VST3        │  boundary
├──────────────────────────────────────────────────────────────────┤
│ L2  DSP           fft · filters · time · dynamics · analysis     │  pure, no notion of "track"
├──────────────────────────────────────────────────────────────────┤
│ L1  hathor-dsp-core      Block · Node trait · Param · contracts  │  no DSP here
├──────────────────────────────────────────────────────────────────┤
│ L0  PLATFORM      WASAPI/ASIO · MIDI · filesystem · GPU          │
└──────────────────────────────────────────────────────────────────┘
```

### Why L2 must not know what a "track" is

It is the decision that holds everything else up. A compressor, in L2, is
`env_follow(input) → gain`, not "the compressor of track 4". The consequences:

* L2 is **testable in isolation** with a synthetic signal, without a host.
* L2 is **reused identically** inside the DAW, inside the CLAP plugin and inside the
  offline renderer. This parity is the only thing guaranteeing that *freeze* sounds like
  live (see §7).
* Third-party VST3/CLAP plugins in L3 occupy **the same slot** as our nodes, because the
  L1 `Node` trait does not know what is behind it.

If you find yourself passing a `TrackId` into `hathor-dsp-filters`, you have broken a layer.

---

## 2. Threading model

Four thread categories, with different privileges. This is the document to reread before
writing any `Arc<Mutex<`.

| Thread | Priority | Owns | Never |
|---|---|---|---|
| **audio** (1 per device) | real-time/high | the active L3-L4 graph, node state | allocating, locking, I/O, sleeping, syscalls |
| **UI** | normal | widgets, state snapshots | touching DSP node state directly |
| **worker/render pool** | normal | freeze, offline analysis, resample, export | touching the live graph (it works on a *clone* of the state) |
| **file/decode** | normal | decoding (symphonia), autosave | assuming the audio thread is stopped |

### What crosses the boundary, and how

The audio↔UI boundary is the source of every bug of this category. Three channels, and
only three:

1. **Continuous values (parameters)** → `AtomicU32` with the `f32` bit-cast
   (`hathor_dsp_core::param::AtomicParam`). Lock-free by construction on x86. The reader sees
   *a value that existed*: never tearing, never a lock held by somebody else. The UI
   writes `Release`, the audio reads `Acquire`.
2. **Discrete events** (note on/off, transport command, add clip, BPM change) → **lock-free
   SPSC ring buffer** (`rtrb`). A queue, not "last value": events are not idempotent,
   losing or duplicating them changes the music.
3. **Large structured data** (a new chain of nodes, a transient table, a waveform) → the
   pattern **"allocate on the UI thread, hand over by reference, free on the UI thread"**:
   an immutable `Arc<T>` published atomically. The audio thread *never frees*: when the
   `Arc` expires, whoever drops it last pays, and if the audio thread holds the last
   `Arc::drop` then you allocated/freed inside the section. The structural mitigation is
   **double buffering**: the worker prepares version N+1, publishes it, and version N stays
   alive on purpose until the worker reclaims it on the next cycle.

> This is the point where the borrow checker looks like an enemy. It is not: it is telling
> you that the case "who frees, and when" is not obvious. Languages with GC do not tell
> you, and the problem stays — it just becomes a *collect* in the middle of the callback.

### The audio thread is not a service

It is a **periodic task with a hard deadline**. The contract (see `hathor-dsp-core`):

* budget = `frames / sample_rate` (64/48k → 1333 µs; 96k/32 → 333 µs)
* `AudioSection::enter()` on entry → anti-allocation guard active
* FTZ/DAZ set **once** at thread startup (`denormal::disable_denormals`)
* elevated priority (cpal `realtime` feature)
* no `panic!` on the hot path, except the guard, which *wants* to blow up in debug

---

## 3. Where the truth of time lives

**The truth is the position in samples** (`u64`), not seconds, not BPM, not quarter notes.
Everything else is a *projection* computed by a function, and the function must be
**piecewise-linear and evaluated, not accumulated**.

```
sample_position ──tempo_map──▶ (beats, bars, seconds)
```

Why "evaluated, not accumulated" matters: a `beat += samples_per_beat` accumulated in f32
over a 3-hour session gives an error of a fraction of a beat that you *hear* as drift, and
it is not reproducible. With evaluation from a table, the position at bar 481 is the same
today and in three hours, and undo/redo is deterministic.

The `tempo_map` (M5) is a list of `(sample_start, bpm)`, with:
* linear **interpolation** between markers (a tempo ramp), not a step
* time signature as a *projection*, not as the measure of time
* swing as a *grid mapping*, not as a transport offset

The device clock (cpal/WASAPI) is the physical **reference**; the logical `sample_position`
locks onto it with drift correction, and when the device disappears the graph survives:
you reopen the stream, you do not rebuild the project.

---

## 4. The graph and latency compensation (PDC)

The graph is not a `Vec<Node>`: it is an **arena of indices** with explicit edges.

```
NodeSlot { id: NodeId, kind: NodeKind, latency_samples: usize,
           group_delay_max: usize, io: (in_port, out_port) }
```

Two decisions taken now and impossible to retrofit:

1. **Every node declares its latency** (`Node::latency_samples`), from day 1, even when it
   is 0. The sum along the *path* (not along the chain!) is what the mixer subtracts to
   align tracks.
2. **Declared latency ≠ group delay**. Two different numbers, both per node:
   * a minimum-phase IIR has `latency_samples = 0` and a *fractional* group delay → nothing
     to compensate, but the phase is skewed (and kick/bass is where you find out);
   * a linear-phase EQ has group delay = `N/2` samples, **which is a real latency** and must
     be compensated, otherwise a "transparent" EQ on an insert shifts your track by half an
     octave of delay.

If one day the kick/bass plugin lies by 1 ms, the cause is almost always here, not in the
phase mathematics.

### Input latency and recording

Recording with a chain of inserts that introduces `L` samples means the recorded file is
late by `L` + driver latency + converter latency. The model has **three** distinct and
non-negotiable latencies:

* `driver_latency` (from the device, measured, not trusting the reported value)
* `plugin_latency` (sum along the path, from §4)
* `user_compensation` (fine offset, in samples, for direct monitoring)

The file write happens with the buffer *already* compensated: you do not record "the dry
and then shift it", because a post-hoc shift is not bit-exact once a resample is involved.

---

## 5. Project state and serialization

The place where hobby DAWs die. It is not the audio: it is the **state**.

* **A typed model, not "a map of properties"**: `Project → Track → Clip → Region →
  Automation`, with `serde` and an explicit version. A file that does not parse must fail
  in a *local* way (one track skipped with a warning), not globally.
* **Crash safety**: `write(tmp) → fsync(tmp) → rename(tmp, target)`. `rename` is atomic on
  NTFS, so a `taskkill` halfway through an autosave leaves either the old or the new file,
  never a middle state. This is an M5 acceptance test, not a best practice.
* **Undo as a log of commands**, not as a deep copy of the project: one copy per step over
  40 tracks explodes RAM. Every command has `apply`/`invert`, and the inversion is tested
  (100 steps forward/back = bit-identical state).
* **Plugin state**: ours → documented serialization (named, versioned parameters).
  Third-party → an opaque `IStream` blob, which we treat as a `Vec<u8>` to hand back
  without understanding it. Never rebuild parameters "by hand".
* **`ABI_CONTRACT_VERSION`** (in `hathor-dsp-core`) written inside the file: a project opened by an
  engine with a different contract is **refused**, not interpreted by force.

---

## 6. The processor boundary (L3)

Three implementations of the same `Node` trait:

| Path | What | Why |
|---|---|---|
| **Native** | our own Rust nodes, compiled in | zero FFI, real inlining, the path your suite uses |
| **CLAP** | our plugins exported as CLAP (C-ABI) | they run outside (Reaper/Bitwig) to *validate* the DSP before your host exists |
| **3rd-party VST3** | `hathor-vst3-host` (minimal C++ bridge + vtable calls from Rust `unsafe`) | the "right away" requirement, which however lands only when a host exists to validate it |

The `Node` trait is deliberately **poorer** than VST3: no "extended I/O", no events in 12
kinds. The VST3 bridge does the *adaptation*, and its translation surface must stay small
and frozen. Every VST3 concept you let into the trait is a thing you will then have to
implement also for CLAP and for native nodes.

---

## 7. Live/offline parity (the hard property)

The offline renderer (freeze, export, bounce) must sound **bit-identical** to live. That is
a property, not an obviousness, and it breaks on three things:

1. **Different block size** → nodes that ramp coefficients per block behave differently.
   Mitigation: the ramp is defined *per frame*, and the renderer uses the same `frames` as
   live.
2. **Parameter update order** → in live, parameters are read at block entry; offline there
   is no real clock, so you need the same sampling point for automation (at block-start,
   not "when the worker gets to it").
3. **Nodes with non-reconditionable state** (a limiter with look-ahead, a reverb initialized
   "on first sound"): freeze must be able to start from a *serialized* state of the node,
   not from zero.

This is why the `Node` trait has separate `setup/process/reset` and an explicit `ProcessCtx`
with `sample_position`: the same signature serves both worlds.

---

## 8. Failure model

An audio engine does not "throw an exception": it degrades and reports.

* **NaN watchdog**: every channel leaving a node is sampled at block end (1 sample out of
  64 is enough, ~zero cost). A `NaN` in a sidechain chain is not seen, it is *heard* 40
  minutes later as permanent silence. A node producing NaN → mute + report + counter, never
  propagated to the mix.
* **Xrun**: signalled, counted, with `worst_callback_us`. Default strategy: do *not* change
  buffer size on its own (the user must know), but show it.
* **Device disappears**: the stream closes; the graph and the project remain; a device is
  reopened when available.
* **VST3 plugin that panics/faults in FFI**: the bridge must have an isolation perimeter (a
  `__try/__except` in the C++ loader where the SDK does arbitrary things) and a persistent
  "this plugin crashed the engine" label.

---

## 9. Decisions already taken, and why (so we don't reopen them every week)

| Decision | Alternative | Why this way |
|---|---|---|
| Rust in the DSP and in the engine | C++/JUCE | Just you + managed-code background + OSS: the compiler replaces the code review you cannot do. JUCE = GPL/fee + C++ everywhere. |
| C++ confined to `hathor-vst3-host` | JUCE for hosting | The VST3 SDK is **MIT** (verified): no legal obstacles. ~2k frozen lines, not the whole product. |
| Planar layout | interleaved | SIMD lanes run along the frames of the same channel. Interleaving exists only at the edges with the driver, and the cost of the transpose is **measured** by `hathor-audio-probe`, not ignored. |
| `Cell` TLS in the guard | manual assertions / Valgrind | Mechanical > mnemonic: a test that fails today instead of a click live in six months. |
| Atomic parameters, not `Mutex` | a lock on the parameter path | The lock is the bug: the worst case arrives while the UI is painting. |
| CLAP as the native ABI | VST3 as the internal format | C-ABI, permissive, trivial to call from Rust, and your plugins work outside immediately. |
| Coefficient ramping, not parameter ramping | recompute per frame | 5 adds/frame vs ~50 ns of `sin/cos/sqrt` **per frame** (expected): ~2.5–4 µs per filter per 64-frame block ≈ 0.2–0.3% of the 1333 µs budget — small for one filter, but it multiplies across bands × channels. |
| FTZ/DAZ on | ignoring denormals | The "it gets slow after 20 minutes" is not reproducible, so it must be removed structurally. |
| cpal `realtime` opt-in | always-high priority | Needed only in the real stream; the offline probe does not need it and must be able to run on CI without a device. |

---

## 10. What this document is missing (honesty)

Still missing:
* the **routing model** (send/return/bus) → written in M5, not earlier, because no host
  exists to connect it to;
* the **UI↔engine strategy for the mixer** (snapshot vs delta) → an M6 decision, with a
  mandatory prototype *before* writing 60 strips;
* **time-stretch/warping** (the "Ableton clip" wish) → not in the roadmap, it is serious DSP
  on its own and has to be decided;
* the **write-scope conflict resolution** when an edit of a clip arrives while the engine is
  reading it (copy-on-write of regions, not locks).

An architecture document that does not say what it *does not* know yet is a document that
lies.
