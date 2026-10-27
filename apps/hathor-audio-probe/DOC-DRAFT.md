# Module: `hathor-audio-probe` (app, L5)

> Status: **written, tested 168, gates green (build/test ×debug+release, clippy `-D warnings`, fmt)**.
> Dependencies: `cpal` 0.15.3, `rtrb` 0.3.6, `hathor-dsp-core`, `hathor-dsp-filters`,
> `hathor-dsp-fft`, `hathor-dsp-measure`, `hathor-host-graph`. Not a library: one binary,
> four subcommands. `docs/**` is outside the builder's write scope, so this file is a
> **draft** delivered from `apps/hathor-audio-probe/DOC-DRAFT.md` for the lead to place.

## Purpose

Turn CONVENTION §2's realtime contract from prose into a number, and do it on the machine
that has to run the DAW. Three questions, three subcommands:

| Question | Subcommand | Touches hardware? |
| --- | --- | --- |
| What do these drivers actually offer? | `devices` | reads only |
| Does one block of real DSP, on a live stream, allocate nothing and stay inside budget? | `run` | **yes — consent-gated** |
| What does each kernel cost, measured, so Rust-vs-C++ is decided on data? | `bench` | no |
| Can the whole contract survive 20,000 blocks under the *strictest* guard setting? | `offline` | no |

The probe is the only place in the repo allowed to open an audio device, and the only place
allowed to say "measured".

## Command line

std-only parser (`cli.rs`), no dependency on a clap-like crate; strict about what it accepts.

```
hathor-audio-probe [devices|run|offline|bench|help|version] [flags]

devices                     list every cpal host/output device and its supported configs
run                         LIVE hardware test (consent-gated, see below)
  --rate <Hz>               default 48000               --frames <n>   default 64 (8..=8192)
  --seconds <s>             default 5.0 (0.5..=600)     --tone <hz|0>  default 440, 0 = silence
  --device <substr>         default = the host's default output device
  --i-have-user-consent     REQUIRED to open a stream at all
offline                     the same pump, no driver, guard in STRICT mode
  --cycles <n>              default 20000 (ROADMAP M0 criterion 2)
  --fft-size <n>            default 1024 (power of two, 4..=16384)
bench                       the offline cost table
  --iterations <n>          timed samples per row, default 512 (512..=262144)
```

Legacy aliases (`--list`, `--stream`, `--offline`, `--bench`) still parse. `--flag=value` is
**rejected**, not half-parsed, with a message that says the values are space separated. Every
numeric flag has a stated range; out of range is a usage error, never a clamped surprise.

## How it works

### The pump (`pump.rs`)

`Pump::setup(rate, channels, chunk_frames, fft_frames, tone)` builds, **on the control
thread, before the stream exists**, every byte the run will ever allocate:

* a `Chain` (`hathor-host-graph`) holding the three biquads in `CHAIN_SPEC`, sized to
  `chunk_frames × channels`;
* the planar working buffer, the FFT window (`FftWindow`, N = 1024) and its bin store
  (`RealFftPlan`), the sine table (4096 entries, `tone.rs`), the `Counters` atomics, and an
  `rtrb` SPSC ring (16,384 `u64`) for block costs.

`Chain` is `!Send`; cpal's callback must be `Send`. The wrapper `OwnedChain` carries the
crate's single `// SAFETY:` of that kind, with the argument written out: the chain is touched
by exactly one thread at a time (the driver thread while the stream runs, and never after
`drop(stream)`), which is the only reason a `Chain` outlives `setup` here.

Per callback, `render()` — and nothing else:

1. `AudioSection::enter()` (the guard's window),
2. split the driver's buffer into ≤ `chunk_frames` blocks (`⌈480/64⌉ = 8` on this machine),
3. tone fill → `Chain::process` (in place on the planar block) → `peak_db`/`rms_db` →
   `with_scratch` door → `RealFftPlan::process` → `interleave` into cpal's own buffer,
4. `Instant` pair outside-to-outside around each chunk, pushed into the ring,
5. levels/energy/max-min frame counts/allocations into atomics, `Drop` closes the section.

No `format!`, no `Vec`, no lock, no syscall inside the window. Formatting is entirely the
control thread's job after the stream is gone.

### Config choice (`device_plan.rs`, pure)

cpal's reported configs are turned into a `DevicePlan` by `plan_output(&[ConfigInfo], &Wants)`
with **no cpal types in sight** — that is what makes the whole decision unit-testable against
a fake device list (14 tests). Ranking is a 5-tuple: usable format (`f32` before `i16`), a
range containing the asked rate, stereo before mono before widest, rate distance, then the
index in cpal's own report order, so ties are deterministic. `chunk_frames` is *always* what
was asked: the DSP chunk is the probe's choice, never the driver's.

### The consent gate (`live_stream.rs`, `main.rs`)

`run` without `--i-have-user-consent` prints `G1 run pending — lead session`, then the exact
9-step list of what it would do, and exits **0** (a refusal is not an error). It never
constructs a `Host`. This is tested (`main.rs::run_without_consent_refuses_and_names_the_lead_session`),
so the gate cannot rot into a flag check that still opens the device.

### Report and verdict (`run_report.rs`)

Ten checks, each `ok`/`warn`/`FAIL`, printed after the device is released. `allocations
observed in the callback` is the hard gate: non-zero ⇒ FAIL. Worst-chunk-cost FAILs above
`warn_ns = 0.40 × budget` (ROADMAP §8 criterion 4), using `cost_summary::BUDGET_64_48K_NS =
(64 × 10⁹)/48000 = 1,333,333 ns`, which is the same 1333.33 µs as
`hathor_dsp_core::BUDGET_64_48K_US` spelled in ns.

## Measured on this machine (release, 2026-10-27)

`offline` — 20,000 cycles, STRICT guard, **PASS 10/10**: 0 allocations (callback total,
process-wide total, and max-in-one-callback all zero), 0 ring drops, worst chunk 54.50 µs =
**0.041× budget**, p99 11.10 µs, mean 6.15 µs, 161,006 chunks/s ≈ 47× real-time, 19,985
transforms with 15 warm-up skips, peak −9.8 dBFS / rms −15.0 dBFS, FTZ/DAZ installed.

`bench` headline (median of 512 timed batches per row, 16 warm-up reps discarded, every
batch inside its own `AudioSection`, **16/16 rows allocated-free**):

| row | per-unit | %budget |
| --- | --- | --- |
| biquad scalar mono, 64 frames | 3.2 ns/frame | — |
| biquad AVX2 8-lane broadcast (`process_detected`) | 0.3 ns/frame | — |
| AVX2 + planar↔interleaved transpose in/out | 0.6 ns/frame | — |
| chain LP8k→Peak1k+6→HP100, stereo, 64 frames | 16.8 ns/frame | — |
| `RealFftPlan(1024)` | 4,187 ns/transform | 0.31% |
| `RealFftPlan(4096)` | 19,233 ns/transform | — |
| `RealFftPlan(16384)` | 94,200 ns/transform | — |
| `FftPlan(4096)` complex | 34,280 ns/transform | — |
| one full pump chunk (tone+chain+door+FFT1024+interleave, stereo 64) | 5,976 ns/chunk | **0.45%** |
| `Instant::now()` + `elapsed()` pair | 42.7 ns/sample | — |

Units are wallclock nanoseconds from `std::time::Instant` pairs, **not CPU cycles**; the
`Instant` pair row exists so the reader can subtract the instrument from every number above
it. Batch lengths are calibrated once, cold, per process, so `min…max` is not comparable
between runs while `per-unit` is.

**Repeat-run spread, stated rather than hidden.** Five full `bench` runs on this machine: the
pump-chunk row agreed to **±2%** (5,958–6,061 ns/chunk), the scalar biquad to ±3%, but the
1024-point transform varied **−2%/+5%** (4,187–4,517) and the three-biquad chain **−9%/0%**
(16.7–18.3 ns/frame). So: treat a per-unit figure here as wallclock with roughly ±10% of
single-machine, single-session scatter. That is enough to decide Rust-vs-C++ at the order-of-
magnitude level and not enough to tune a kernel; a tuned number needs `--iterations` in the
thousands, a fixed power plan, and several processes.

The two numbers that matter for G1: one live-path chunk is **0.45% of the 64-frame/48 kHz
budget** offline-measured, and the same shape on a live device ran 20,000 blocks with
**zero allocations** under the strict guard.

## Honesty rules this binary enforces on itself

* **64-frame driver blocks are not expected.** cpal 0.15.3 hardcodes
  `AUDCLNT_SHAREMODE_SHARED` + `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`
  (`host/wasapi/device.rs:698-706`) and hands the callback whatever `frames_available` says
  (`stream.rs:450`) — on this machine ~480 frames. The run therefore requests 64, *reports the
  min…max it actually observed*, and pumps 64-frame DSP chunks inside whatever arrived. The
  check "driver callback sizes vs the requested block" is `warn`, not `fail`, on purpose: it
  is a finding about shared mode, not a mistake by the probe.
* **No xrun counter exists in WASAPI shared mode**, so the report labels the pair
  (`cpal::StreamError` callbacks, chunks over budget) **"xrun proxy"** instead of inventing a
  number.
* Cost is labelled *wallclock*, `Instant` outside-to-outside, timer read included. Never
  "cycles", never "CPU time".
* `run` uses **measure** mode, not strict mode: a guard panic would have to unwind through an
  `extern "system"` WASAPI callback frame. The count is still the verdict — a single allocation
  FAILs the run — and the strict assertion moves to `offline`, where a panic is just a panic.
* A zero is only printed next to a test that proves the instrument can see non-zero
  (`pump::the_guard_is_live_in_this_binary_negative_control` allocates a `Vec` inside a section
  and demands it be counted).
* Every table closes with `measured <date>, dev machine, <profile> build`, the profile read
  from `cfg!(debug_assertions)` and the date from the OS clock (labelled UTC). No claim
  without a measurement; no measurement without its provenance.

## Windows-level extras deliberately left out (and why)

Printed by the run report as four `not …` lines so the omission is visible, not silent:

* `SetPriorityClass(HIGH_PRIORITY_CLASS)` — needs a `windows-sys` dependency (not wired for
  this crate; adding it is a lead decision) **and** cpal already sets the callback thread to
  `THREAD_PRIORITY_TIME_CRITICAL` (`stream.rs:325-334`), which is the thread that matters.
  A process-wide class would also raise every other thread of the probe, including the one
  printing the report.
* `timeBeginPeriod(1)` — process-*global* timer resolution that other programs inherit.
  `std::time::Instant` is QPC and does not need it; belongs in M5's host, behind a flag.
* `AvSetMmThreadCharacteristics("Pro Audio")` (MMCSS) — a system-wide scheduling claim on a
  shared engine; the probe's job is to measure, not to renegotiate the machine.
* **Exclusive mode (`AUDCLNT_SHAREMODE_EXCLUSIVE`)** — cpal 0.15.3 cannot grant it, so asking
  would be a lie in the report rather than a flag.

## Deviations from the original brief

1. **A fourth subcommand, `offline`** (approved by the lead): the strict-guard 20,000-cycle
   rehearsal. It is the only way to assert "zero allocations" at ROADMAP M0 criterion 2
   without touching a device, and it is what makes `run`'s measure-mode choice defensible.
2. **A `render_failures` counter** was added to `Counters`: a cpal callback cannot return an
   error, so a chain that refused a block would otherwise be invisible in the report.
3. Legacy flag aliases (`--list`, `--stream`) are accepted; harmless, and the old notes in
   `docs/` use them.

## Tests (168, all inline — no dev-dependencies)

`run_report` 19, `pump` 17, `device_plan` 14, `cli` 13, `counters` 12, `live_stream` 12,
`bench` 11, `stamp` 10, `cost_summary` 9, `listing` 9, `tone` 8, `fft_window` 7,
`offline_run` 7, `interleave` 7, `timing_ring` 6, `main` 4, `probe_error` 3.

Guard-state tests share process-global state, so they serialise on
`main::guard_serial()` (a `Mutex<()>`, poison-tolerant per STYLE) instead of assuming the
harness runs one thread. Pure logic (config choice, stats, percentile, window shift, tone
quantisation, date math) is tested against hand-computed anchors — the `stamp` day numbers
against .NET's own date arithmetic, never against this crate's own formula.

## What `run` will do when the lead executes it

```
hathor-audio-probe run --seconds 5            # default device, 48 kHz, 64-frame ask
hathor-audio-probe run --device "SSL 2" --seconds 10 --tone 0   # silence, 10 s, named device
```

Expected on this machine: plan printed first (config #3, 2 ch @ 48000 Hz f32, driver buffer
request 64), then ~480-frame callbacks pumping 8 DSP chunks each, then the ten checks. The
`driver callback sizes` line will almost certainly be `warn` — that is the shared-mode finding,
and it is the single most useful thing the live run can tell M5 about this device.
