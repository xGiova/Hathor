//! `run`: the live hardware test. This module owns everything about it except the
//! decision to open a device, which belongs to `main` and to the human holding the
//! command line.
//!
//! ## What a live run is, step by step (the same list `run` prints before it touches
//! ## hardware, so the human can read the plan while it is still a plan)
//!
//! 1. ask cpal for the **default output device** (or the one `--device` names, matched
//!    case-insensitively as a substring of the device name);
//! 2. read its `supported_output_configs()`, translate them one field per line into
//!    [`ConfigInfo`], and hand them to [`crate::device_plan`]: f32 before i16, the rate
//!    range containing the asked rate, stereo before mono;
//! 3. **print** the choice — channels, rate, format, the requested driver buffer and
//!    whether the device clamped it, the DSP chunk shape — before opening anything;
//! 4. build the [`crate::pump`]: three biquads in a `Chain` sized to the chunk shape ×
//!    the device's channel count, a 1024-frame FFT window, its bin store, the sine
//!    table, the atomics, the 16,384-sample timing ring. Every allocation in the run
//!    happens here, on the control thread, before the stream exists;
//! 5. put the guard in **measure** mode (`set_panic_on_audio_alloc(false)`): a
//!    violation must be counted and reported, not unwound through an `extern "C"`
//!    callback frame. Any count > 0 still FAILs the run;
//! 6. `device.build_output_stream::<f32, _, _>(&config, callback, error_callback, None)`;
//! 7. `stream.play()`, then wait on the control thread: sleep 20 ms, drain the timing
//!    ring, print a heartbeat every 500 ms;
//! 8. at `--seconds`, set the stop flag, `stream.pause()`, drop the stream — the
//!    device is free; every `String` in the report is formatted after this point;
//! 9. snapshot the counters, sort the cost samples, evaluate [`crate::run_report`]'s
//!    checks, print the plan-against-machine differences honestly, and let `main` turn
//!    the verdict into an exit code.
//!
//! ## Inside the callback (the zero-allocation part)
//!
//! One [`Pump::render`] per driver callback: `AudioSection::enter()` → chunk `data`
//! into blocks of at most `chunk_frames` → per chunk: `Instant::now()`, tone fill,
//! `Chain::process` on the planar block, exact levels, the 1024-point `RealFftPlan`
//! fed from the graph's **scratch door**, planar→interleaved copy into cpal's own
//! buffer, `elapsed()`, atomics + `rtrb::Producer::push` → read `allocs_here()` →
//! leave the section → count the callback. No `format!`, no `Vec`, no lock, no
//! syscall, nothing that can block.
//!
//! ## What this binary deliberately does NOT do, and why (a report requirement)
//!
//! * **`SetPriorityClass(HIGH_PRIORITY_CLASS)`** — not set. cpal already calls
//!   `SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)` on its own callback thread
//!   (`host/wasapi/stream.rs` in 0.15.3), and that is the priority that matters for a
//!   period. A *process*-wide class needs `windows-sys` — a dependency this crate may
//!   not add unilaterally — and would change the scheduling of every other thread in
//!   the probe for no measured benefit at 64/48k.
//! * **`timeBeginPeriod(1)`** — not set. It lowers the system timer resolution for
//!   every process on the machine, and the probe's clock is `std::time::Instant`
//!   (QueryPerformanceCounter), which does not depend on it. Leaving it alone leaves
//!   the machine as the human found it.
//! * **`AvSetMmThreadCharacteristics("Pro Audio")`** (MMCSS) — not set: a scheduling
//!   *policy* belongs to the host that owns the session (the M5 engine), not to a
//!   single-stream probe.
//! * **exclusive-mode WASAPI / ASIO** — not requested, because cpal 0.15.3 hardcodes
//!   `AUDCLNT_SHAREMODE_SHARED` (`host/wasapi/device.rs:698-706`). This is why the
//!   delivered block is the engine period and not 64, and the report says that instead
//!   of pretending otherwise.
//! * **an xrun count** — not reported, because shared mode exposes none. What is
//!   reported is the *proxy*: cpal error callbacks plus chunks whose own measured
//!   wallclock exceeded the budget, labelled as a proxy.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, Host, SampleRate, StreamConfig, SupportedBufferSize,
    SupportedStreamConfigRange,
};
use hathor_dsp_core::alloc_guard;

use crate::cli::{RunOptions, Tone};
use crate::counters::{Counters, CountersView};
use crate::cost_summary::{budget_ns, CostSummary};
use crate::device_plan::{
    plan_output, BufferLimit, BufferRequest, ConfigInfo, DevicePlan, FrameDecision, PlanError,
    RateDecision, StreamFormat, Wants,
};
use crate::probe_error::ProbeError;
use crate::pump::Pump;
use crate::run_report::{self, Check, Measured, Severity};
use crate::stamp;
use crate::timing_ring::TimingDrain;

/// The analysis window the live run pumps: one `RealFftPlan(1024)`'s worth of frames,
/// sliding, one transform per block once it is full.
pub const FFT_FRAMES: usize = 1_024;

/// The live result: the machine's answer, as data.
#[derive(Debug)]
pub struct RunResult {
    /// What was asked.
    pub options: RunOptions,
    /// The device we opened, as cpal named it.
    pub device_name: String,
    /// The plan printed before opening.
    pub plan: DevicePlan,
    /// Everything the callback observed.
    pub view: CountersView,
    /// The cost distribution over the samples that crossed the ring.
    pub cost: CostSummary,
    /// The guard's process-wide total (a cross-check of `view.allocs_sum`).
    pub guard_total: u64,
    /// Wallclock seconds the stream actually ran.
    pub real_seconds: f64,
    /// A node refused a block: the contract broke mid-run.
    pub node_error: Option<&'static str>,
    /// Node names, ring capacity and tone, captured before the pump moved into the
    /// callback (after that they are unreachable from the control thread).
    pub facts: PumpFacts,
}

/// What the pump can tell us before it is handed to the driver thread.
#[derive(Debug, Clone)]
pub struct PumpFacts {
    /// Node names in push order.
    pub node_names: Vec<String>,
    /// The frequency actually generated, after table quantisation.
    pub actual_tone_hz: f32,
    /// The frequency asked for.
    pub asked_tone_hz: f32,
    /// Whether the generator was silence.
    pub tone_silent: bool,
    /// Timing ring capacity in samples.
    pub ring_capacity: usize,
}

impl RunResult {
    /// The block budget for the shape the DSP actually pumped.
    #[must_use]
    pub fn budget_ns(&self) -> u64 {
        budget_ns(self.plan.chunk_frames, self.plan.rate)
    }

    /// The verdict. Pure, and tested in [`crate::run_report`].
    #[must_use]
    pub fn checks(&self) -> Vec<Check> {
        let m = Measured {
            view: &self.view,
            cost: &self.cost,
            requested_chunk: self.plan.chunk_frames,
            budget_ns: self.budget_ns(),
            asked_seconds: self.options.seconds,
            real_seconds: self.real_seconds,
            guard_allocs: self.guard_total,
            stream_errored: self.view.stream_errors > 0,
            node_refused: self.node_error.is_some(),
            fft_size: FFT_FRAMES,
        };
        run_report::evaluate(&m)
    }

    /// True when nothing failed.
    #[must_use]
    pub fn passed(&self) -> bool {
        run_report::passed(&self.checks())
    }

    /// The whole report as lines. Formatting happens here — after the stream is gone.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let checks = self.checks();
        let mut out = plan_report(&self.device_name, &self.plan, &self.options);
        out.push(format!(
            "pump: {} node(s) [{}], chunk {} frames, budget {:.2} µs/chunk, analysis {} frames, ring {} samples",
            self.facts.node_names.len(),
            self.facts.node_names.join(" → "),
            self.plan.chunk_frames,
            self.budget_ns() as f64 / 1e3,
            FFT_FRAMES,
            self.facts.ring_capacity,
        ));
        out.push(format!(
            "tone: {} (asked {} Hz), amplitude 0.2 (−14 dBFS)",
            if self.facts.tone_silent {
                "silence".to_string()
            } else {
                format!("{:.4} Hz", self.facts.actual_tone_hz)
            },
            self.facts.asked_tone_hz,
        ));
        if let Some(e) = self.node_error {
            out.push(format!("FAIL node refused the block: {e}"));
        }
        out.extend(observed_lines(&self.view, self.plan.chunk_frames));
        out.push(format!("cost: {}", self.cost));
        out.push(format!("  cost unit: {}", CostSummary::label()));
        out.push(format!(
            "guard: callback total {}, process-wide total {} (measure mode: a violation is counted, never unwound through a driver callback)",
            self.view.allocs_sum, self.guard_total
        ));
        out.push(format!(
            "duration: {:.1} s asked, {:.3} s real",
            self.options.seconds, self.real_seconds
        ));
        out.extend(run_report::lines(&checks));
        out.extend(windows_extras_lines());
        out.push(verdict_summary(&checks));
        out.extend(stamp::measured_block(stamp::utc_today()));
        out
    }
}

/// The gate's own text, as data: what `run` will do, printed *without* doing it.
///
/// Extracted so `main`'s refusal is testable and so the human who forgot the flag
/// still learns the plan. The steps mirror the module doc above; a `run` in front of
/// a person starts by telling them what is about to happen.
#[must_use]
pub fn consent_steps(o: &RunOptions) -> Vec<String> {
    vec![
        format!(
            "run would: open the {} output device (default, or --device match), ask for \
             {} Hz and a {}-frame driver buffer",
            "host's", o.rate, o.frames
        ),
        "  1. print the chosen config — channels, rate, format, driver buffer, whether \
         the device clamped it — BEFORE opening anything"
            .to_string(),
        format!(
            "  2. build the pump on this thread: 3 biquads in a Chain sized {}×{} \
             (frames×channels), a {}-frame FFT window and its bins, the sine table, the \
             atomics, a 16384-sample timing ring — every allocation in the run happens here",
            o.frames, o.rate, FFT_FRAMES
        ),
        "  3. set the allocation guard to measure mode (a violation is counted, never \
         unwound through a driver callback) and reset its counters"
            .to_string(),
        format!(
            "  4. build_output_stream::<f32> and play(); the callback then does \
             AudioSection::enter → chunk the buffer into ≤{}-frame blocks → tone fill → \
             Chain::process → exact levels → RealFftPlan({FFT_FRAMES}) fed from the graph's \
             scratch door → planar→interleaved into cpal's own buffer → Instant pairs around \
             the chunk → atomics + rtrb push → leave the section and count allocations. No \
             format!, no Vec, no lock, no syscall.",
            o.frames
        ),
        format!(
            "  5. wait {:.1} s, draining the ring every 20 ms with a heartbeat line every \
             500 ms",
            o.seconds
        ),
        "  6. set the stop flag, pause() and drop() the stream — the device is free \
         before anything is formatted"
            .to_string(),
        "  7. snapshot the counters, sort the cost samples, print every check as \
         ok/warn/FAIL (allocations must be 0), and exit non-zero if any check failed"
            .to_string(),
        "  8. the cost is wallclock (std::time::Instant, outside-to-outside around the \
         chunk), NOT CPU cycles"
            .to_string(),
        "  9. report the delivered callback sizes as measured, because shared-mode WASAPI \
         keeps the audio engine's period and exposes no xrun counter"
            .to_string(),
    ]
}

/// Translates one cpal config range into the probe's pure [`ConfigInfo`]: one field
/// per line, so "is this translation right?" is a ten-line question.
fn to_info(r: &SupportedStreamConfigRange) -> ConfigInfo {
    ConfigInfo {
        channels: r.channels(),
        min_rate: r.min_sample_rate().0,
        max_rate: r.max_sample_rate().0,
        buffer: match r.buffer_size() {
            SupportedBufferSize::Range { min, max } => BufferLimit::Range { min: *min, max: *max },
            SupportedBufferSize::Unknown => BufferLimit::Unknown,
        },
        format: match r.sample_format() {
            cpal::SampleFormat::F32 => StreamFormat::F32,
            cpal::SampleFormat::I16 => StreamFormat::I16,
            _ => StreamFormat::Unsupported,
        },
    }
}

/// The [`StreamConfig`] a plan implies. `buffer_size` is the **driver** request; the
/// DSP chunk shape is the pump's business and never travels in this struct.
#[must_use]
pub fn stream_config(plan: &DevicePlan) -> StreamConfig {
    StreamConfig {
        channels: plan.channels,
        sample_rate: SampleRate(plan.rate),
        buffer_size: match plan.buffer {
            BufferRequest::Fixed(n) => BufferSize::Fixed(n),
            BufferRequest::Default => BufferSize::Default,
        },
    }
}

/// Case-insensitive substring match over device names, so `--device speakers` finds
/// "Speakers (Realtek(R) Audio)" and a name that matches nothing is an error rather
/// than a silent fall back to the default.
#[must_use]
pub fn find_device(names: &[String], needle: &str) -> Option<usize> {
    let n = needle.to_lowercase();
    names.iter().position(|d| d.to_lowercase().contains(&n))
}

/// Picks the device: the default output, or the first whose name contains `wanted`.
///
/// # Errors
/// [`ProbeError::Device`] when there is no default output or no name matches.
pub fn select_device(host: &Host, wanted: Option<&str>) -> Result<Device, ProbeError> {
    match wanted {
        None => host
            .default_output_device()
            .ok_or_else(|| ProbeError::Device("this host has no default output device".into())),
        Some(sub) => {
            let devices = host
                .output_devices()
                .map_err(|e| ProbeError::Device(format!("output_devices: {e}")))?;
            // One pass, name and device together: cpal's iterator is consumed as it
            // goes, and matching by index across two passes would be a lie about
            // ordering.
            let mut found: Option<(String, Device)> = None;
            for d in devices {
                let name = d
                    .name()
                    .unwrap_or_else(|e| format!("<name failed: {e}>"));
                if found.is_none() && find_device(std::slice::from_ref(&name), sub).is_some() {
                    found = Some((name, d));
                }
            }
            found.map(|(name, d)| (d, name)).map_or_else(
                || {
                    Err(ProbeError::Device(format!(
                        "no output device name contains {sub:?} (run `devices` for the list)"
                    )))
                },
                |(d, _)| Ok(d),
            )
        }
    }
}

/// The configs a device reports, in probe vocabulary.
///
/// # Errors
/// [`ProbeError::Device`] when the enumeration itself fails.
pub fn configs_of(device: &Device) -> Result<Vec<ConfigInfo>, ProbeError> {
    device
        .supported_output_configs()
        .map(|rs| rs.map(|r| to_info(&r)).collect())
        .map_err(|e| ProbeError::Device(format!("supported_output_configs: {e}")))
}

/// The plan for a device at the requested rate/block.
///
/// # Errors
/// [`ProbeError::Device`] naming the configs that were available, when none is usable.
pub fn decide(configs: &[ConfigInfo], wants: &Wants) -> Result<DevicePlan, ProbeError> {
    plan_output(configs, wants).map_err(|e| describe_plan_error(&e, configs))
}

/// A refusal that names the configs it was given: "no usable format" alone is useless
/// to someone holding a console and a suspicious driver.
fn describe_plan_error(e: &PlanError, configs: &[ConfigInfo]) -> ProbeError {
    let seen: Vec<String> = configs
        .iter()
        .map(|c| {
            format!(
                "{}ch {}..{}Hz {}",
                c.channels,
                c.min_rate,
                c.max_rate,
                c.format.label()
            )
        })
        .collect();
    ProbeError::Device(format!("{e} — configs were: {}", seen.join(" | ")))
}

/// The lines `run` prints **before** touching hardware: the plan, the honesty note,
/// and the promise about the DSP chunk.
#[must_use]
pub fn plan_report(device_name: &str, plan: &DevicePlan, o: &RunOptions) -> Vec<String> {
    let mut v = vec![
        format!("device: {device_name}"),
        format!(
            "asking: {} Hz, {} frame block, {} s, tone {}",
            o.rate,
            o.frames,
            o.seconds,
            match o.tone {
                Tone::Silence => "silence".to_string(),
                Tone::Sine(hz) => format!("{hz} Hz"),
            }
        ),
    ];
    let rate = match plan.rate_decision {
        RateDecision::Exact { asked } => format!("{asked} Hz (as asked)"),
        RateDecision::Clamped {
            asked,
            got,
            range,
        } => format!(
            "{got} Hz (asked {asked}, device range {}..={})",
            range.0, range.1
        ),
    };
    let buffer = match plan.frame_decision {
        FrameDecision::Exact { asked } => format!(
            "{asked} frames requested (device reported {})",
            match plan.reported {
                BufferLimit::Range { min, max } => format!("{min}..={max}"),
                BufferLimit::Unknown => "no range".to_string(),
            }
        ),
        FrameDecision::Clamped {
            asked,
            got,
            range,
        } => format!(
            "{got} frames requested (asked {asked}, device range {}..={})",
            range.0, range.1
        ),
        FrameDecision::EngineDefault { asked } => format!(
            "device default requested (no range reported); DSP chunk stays {asked} frames"
        ),
    };
    v.push(format!(
        "config: #{} {}, {rate}, {buffer}",
        plan.config_index,
        plan.format.label()
    ));
    v.push(format!(
        "channels: {} (the chain is sized to what the device was opened with)",
        plan.channels
    ));
    v.push(format!(
        "DSP pumps {}-frame blocks regardless of the driver's period",
        plan.chunk_frames
    ));
    v.push(format!("note: {}", plan.honesty_note()));
    v
}

/// The counter block as lines. Separate so a unit test can feed it a view.
#[must_use]
pub fn observed_lines(v: &CountersView, chunk_frames: u32) -> Vec<String> {
    vec![
        format!(
            "observed: {} callback(s), {} chunk(s) of ≤{} frames, {} frame(s) pumped",
            v.callbacks, v.chunks, chunk_frames, v.frames
        ),
        format!(
            "callback sizes: {}..={} frames delivered",
            if v.cb_frames_min == u32::MAX {
                "none".to_string()
            } else {
                v.cb_frames_min.to_string()
            },
            v.cb_frames_max
        ),
        format!(
            "counts: stream errors (xrun proxy) {}, render failures {}, ring drops {}, allocs {} (max {} in one callback)",
            v.stream_errors, v.render_failures, v.ring_drops, v.allocs_sum, v.allocs_max_per_callback
        ),
        format!(
            "analysis: {} transform(s), {} skipped while the window filled, Σ|X|² {}",
            v.fft_runs,
            v.fft_warm_skips,
            CountersView::energy_from_bits_opt(v.energy_bits)
                .map_or_else(|| "non-finite".to_string(), |e| format!("{e:.1}"))
        ),
        format!(
            "levels: peak {} dBFS, rms {} dBFS, FTZ/DAZ {}",
            CountersView::from_bits_opt(v.peak_bits)
                .map_or_else(|| "−inf".to_string(), |p| format!("{p:.1}")),
            CountersView::from_bits_opt(v.rms_bits)
                .map_or_else(|| "−inf".to_string(), |r| format!("{r:.1}")),
            if v.ftz_set { "on" } else { "OFF" }
        ),
    ]
}

/// The Windows-level things this probe deliberately does **not** do, printed in the
/// report so nobody has to guess whether they were forgotten or refused.
#[must_use]
pub fn windows_extras_lines() -> Vec<String> {
    vec![
        "not set: SetPriorityClass(HIGH_PRIORITY_CLASS) — cpal already raises the \
         callback *thread* to THREAD_PRIORITY_TIME_CRITICAL; a process-wide class needs \
         windows-sys, a new dependency, which is the lead's call and not a probe's"
            .to_string(),
        "not set: timeBeginPeriod(1) — it changes the system timer resolution for every \
         other process, and this probe's clock is Instant (QueryPerformanceCounter), \
         which does not depend on it"
            .to_string(),
        "not set: AvSetMmThreadCharacteristics(\"Pro Audio\") — an MMCSS scheduling \
         claim is a policy for the M5 host to own, not for a single-stream probe"
            .to_string(),
        "not asked: exclusive-mode WASAPI / ASIO — cpal 0.15.3 hardcodes shared mode, so \
         the probe cannot ask for what its own dependency cannot grant; no xrun counter \
         exists in shared mode, so the report prints an explicitly labelled proxy"
            .to_string(),
    ]
}

/// The last line before the stamp: counts by severity, so a `PASS` cannot hide a
/// `FAIL` that scrolled off the console.
#[must_use]
pub fn verdict_summary(checks: &[Check]) -> String {
    let fails = checks.iter().filter(|c| c.severity == Severity::Fail).count();
    let warns = checks.iter().filter(|c| c.severity == Severity::Warn).count();
    format!(
        "verdict: {} FAIL, {} warn of {} checks → {}",
        fails,
        warns,
        checks.len(),
        if fails == 0 { "PASS" } else { "FAIL" },
    )
}

/// Opens the device, runs for `options.seconds`, and returns everything measured.
///
/// # Errors
/// Device/stream/setup failures as [`ProbeError`]. A *measured* violation (an
/// allocation, an over-budget block) is not an error here: it is a failed check in the
/// report, so the numbers still get printed. Only the absence of data aborts.
pub fn execute(o: &RunOptions, plan: DevicePlan, device: Device, name: String) -> Result<RunResult, ProbeError> {
    if plan.format == StreamFormat::Unsupported {
        return Err(ProbeError::Device(format!(
            "the chosen config is {}; this build's callback is typed for f32 and the \
             edge conversion is not measured here",
            plan.format.label()
        )));
    }
    let (mut pump, consumer) = Pump::setup(plan.rate, plan.channels, plan.chunk_frames, FFT_FRAMES, o.tone)?;
    let facts = PumpFacts {
        node_names: pump.chain_names(),
        actual_tone_hz: pump.actual_tone_hz(),
        asked_tone_hz: pump.asked_tone_hz(),
        tone_silent: pump.tone_is_silent(),
        ring_capacity: pump.ring_capacity(),
    };
    let counters: Arc<Counters> = pump.counters();
    let config = stream_config(&plan);
    let budget = budget_ns(plan.chunk_frames, plan.rate);

    // Measure mode: a violation is counted, never unwound through a driver callback.
    alloc_guard::set_panic_on_audio_alloc(false);
    alloc_guard::reset_counters();

    let cb_counters: Arc<Counters> = Arc::clone(&counters);
    let err_counters: Arc<Counters> = Arc::clone(&counters);
    let mut stream = device
        .build_output_stream::<f32, _, _>(
            &config,
            move |data: &mut [f32], _info| {
                // The stop flag first: after `request_stop` the driver still must get
                // *something*, and silence is the cheapest legal answer.
                if cb_counters.stopped() {
                    crate::pump::fill_silence(data);
                    return;
                }
                // Errors are counted, not carried: `render` has already done its own
                // bookkeeping, and the message is a `String` — an allocation inside the
                // window being measured. The label it would have held is one of the
                // fixed set in [`crate::pump::node_err`].
                if pump.render(data, false).is_err() {
                    cb_counters.note_render_failure();
                }
            },
            move |e: cpal::StreamError| {
                // Count only: `StreamError`'s Display allocates, and this runs on the
                // driver thread while allocations are being measured.
                err_counters.note_stream_error();
                let _ = e;
            },
            None,
        )
        .map_err(|e| ProbeError::Stream(format!("build_output_stream: {e}")))?;

    let mut drain = TimingDrain::new(consumer, 16_384);
    let started = Instant::now();
    stream
        .play()
        .map_err(|e| ProbeError::Stream(format!("Stream::play: {e}")))?;
    let want = Duration::from_secs_f64(o.seconds);
    let mut last_progress = Duration::ZERO;
    while started.elapsed() < want {
        std::thread::sleep(Duration::from_millis(20));
        let e = started.elapsed();
        if e - last_progress >= Duration::from_millis(500) {
            last_progress = e;
            // Draining on the way keeps the ring from filling and gives the room a
            // heartbeat that is measured rather than guessed.
            drain.drain();
            let v = counters.view();
            println!(
                "  {:.1}/{:.1} s: {} callbacks, {} chunks, {} allocs, worst {:.1} µs",
                e.as_secs_f64(),
                o.seconds,
                v.callbacks,
                v.chunks,
                v.allocs_sum,
                v.cost_max_ns as f64 / 1e3,
            );
        }
    }
    counters.request_stop();
    let _ = stream.pause();
    drop(stream);
    let real_seconds = started.elapsed().as_secs_f64();
    drain.drain();

    let view = counters.view();
    let guard_total = alloc_guard::allocs_total();
    let mut samples = drain.into_samples();
    let cost = CostSummary::from_samples(&mut samples, budget);
    Ok(RunResult {
        options: o.clone(),
        device_name: name,
        plan,
        view,
        cost,
        guard_total,
        real_seconds,
        node_error: (view.render_failures > 0)
            .then_some("a callback returned an error (see the render-failure count above)"),
        facts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wasapi_48k() -> ConfigInfo {
        ConfigInfo {
            channels: 2,
            min_rate: 44_100,
            max_rate: 192_000,
            buffer: BufferLimit::Range {
                min: 0,
                max: u32::MAX,
            },
            format: StreamFormat::F32,
        }
    }

    #[test]
    fn device_matching_is_case_insensitive_substring() {
        let names = vec![
            "Speakers (Realtek(R) Audio)".to_string(),
            "LG HDR HD (NVIDIA High Definition Audio)".to_string(),
        ];
        assert_eq!(find_device(&names, "speakers"), Some(0));
        assert_eq!(find_device(&names, "REALTEK"), Some(0));
        assert_eq!(find_device(&names, "nvidia"), Some(1));
        assert_eq!(
            find_device(&names, "focusrite"),
            None,
            "a name that matches nothing must not silently open the default"
        );
    }

    #[test]
    fn the_plan_report_names_requested_and_actual_separately() {
        let info = ConfigInfo {
            min_rate: 44_100,
            max_rate: 44_100,
            ..wasapi_48k()
        };
        let plan = decide(&[info], &Wants { rate: 48_000, frames: 64 }).expect("plan");
        let o = RunOptions::default();
        let lines = plan_report("Speakers", &plan, &o).join("\n");
        assert!(lines.contains("device: Speakers"), "{lines}");
        assert!(
            lines.contains("44100 Hz (asked 48000"),
            "the clamp must be visible, not smoothed over: {lines}"
        );
        assert!(lines.contains("asking: 48000 Hz, 64 frame block"), "{lines}");
    }

    #[test]
    fn a_clamped_driver_buffer_still_promises_the_dsp_chunk() {
        let info = ConfigInfo {
            min_rate: 48_000,
            max_rate: 48_000,
            buffer: BufferLimit::Range {
                min: 11_520,
                max: 11_520,
            },
            format: StreamFormat::F32,
        };
        let plan = decide(&[info], &Wants { rate: 48_000, frames: 64 }).expect("plan");
        let lines = plan_report("X", &plan, &RunOptions::default()).join("\n");
        assert!(lines.contains("11520 frames requested (asked 64"), "{lines}");
        assert!(
            lines.contains("DSP pumps 64-frame blocks regardless"),
            "the sentence the whole chunking design exists to be able to say: {lines}"
        );
    }

    #[test]
    fn the_stream_config_follows_the_plan_including_the_clamped_buffer() {
        let info = ConfigInfo {
            channels: 1,
            min_rate: 48_000,
            max_rate: 48_000,
            buffer: BufferLimit::Range { min: 256, max: 1_024 },
            format: StreamFormat::F32,
        };
        let plan = decide(&[info], &Wants { rate: 48_000, frames: 64 }).expect("plan");
        let cfg = stream_config(&plan);
        assert_eq!(cfg.channels, 1);
        assert_eq!(cfg.sample_rate, SampleRate(48_000));
        assert_eq!(cfg.buffer_size, BufferSize::Fixed(256), "the device's minimum");
        assert_eq!(plan.chunk_frames, 64, "and the DSP still pumps 64");
    }

    #[test]
    fn an_unknown_range_becomes_cpal_default_not_a_guess() {
        let info = ConfigInfo {
            buffer: BufferLimit::Unknown,
            ..wasapi_48k()
        };
        let plan = decide(&[info], &Wants { rate: 48_000, frames: 64 }).expect("plan");
        assert_eq!(stream_config(&plan).buffer_size, BufferSize::Default);
    }

    #[test]
    fn a_refused_plan_names_the_configs_it_was_given() {
        let info = ConfigInfo {
            format: StreamFormat::Unsupported,
            ..wasapi_48k()
        };
        let e = decide(&[info], &Wants { rate: 48_000, frames: 64 }).err().expect("refused");
        let s = e.to_string();
        assert_eq!(e.tag(), "device");
        assert!(s.contains("2ch 44100..192000Hz f32"), "configs listed: {s}");
    }

    #[test]
    fn observed_lines_carry_the_counter_block_and_never_a_false_range() {
        let c = Counters::new();
        c.note_callback(480, 0);
        for _ in 0..8 {
            c.note_chunk(100, 64, 533_333, 1_333_333);
        }
        c.note_levels(-14.0, -17.0);
        c.note_fft(true);
        c.note_energy(9.0);
        let ls = observed_lines(&c.view(), 64).join("\n");
        assert!(ls.contains("1 callback(s), 8 chunk(s) of ≤64 frames"), "{ls}");
        assert!(ls.contains("480..=480 frames delivered"), "{ls}");
        assert!(ls.contains("Σ|X|² 9.0"), "{ls}");
        assert!(ls.contains("FTZ/DAZ OFF"), "{ls}");
        assert!(ls.contains("xrun proxy"), "{ls}");
        let none = observed_lines(&Counters::new().view(), 64).join("\n");
        assert!(none.contains("none..=0 frames"), "no callback ever ran: {none}");
    }

    #[test]
    fn the_windows_extras_are_listed_with_their_reasons() {
        let ls = windows_extras_lines();
        assert!(ls.len() >= 4, "priority, timer, MMCSS, exclusive mode: {ls:?}");
        assert!(ls.iter().any(|l| l.contains("SetPriorityClass")), "{ls:?}");
        assert!(ls.iter().any(|l| l.contains("timeBeginPeriod")), "{ls:?}");
        assert!(ls.iter().any(|l| l.contains("Pro Audio")), "{ls:?}");
        assert!(ls.iter().any(|l| l.contains("windows-sys")), "{ls:?}");
        assert!(
            ls.iter().all(|l| l.starts_with("not ")),
            "each line must say what was refused: {ls:?}"
        );
    }

    #[test]
    fn verdict_summary_counts_by_severity_and_cannot_hide_a_fail() {
        let checks = vec![
            Check { name: "a", detail: String::new(), severity: Severity::Pass },
            Check { name: "b", detail: String::new(), severity: Severity::Warn },
            Check { name: "c", detail: String::new(), severity: Severity::Fail },
        ];
        assert_eq!(
            verdict_summary(&checks),
            "verdict: 1 FAIL, 1 warn of 3 checks → FAIL"
        );
        assert_eq!(
            verdict_summary(&[]),
            "verdict: 0 FAIL, 0 warn of 0 checks → PASS",
            "vacuous by construction: `evaluate` always pushes its checks, and an \
             empty counter set fails on its own numbers (see run_report's tests)"
        );
    }

    #[test]
    fn the_live_analysis_window_is_the_task_number() {
        assert_eq!(FFT_FRAMES, 1_024);
        assert!(FFT_FRAMES.is_power_of_two());
    }

    #[test]
    fn a_budget_of_the_pumped_shape_is_the_core_constant() {
        let plan = decide(&[wasapi_48k()], &Wants { rate: 48_000, frames: 64 }).expect("plan");
        let r = RunResult {
            options: RunOptions::default(),
            device_name: "X".to_string(),
            plan,
            view: Counters::new().view(),
            cost: CostSummary::from_samples(&mut [], 1_333_333),
            guard_total: 0,
            real_seconds: 0.0,
            node_error: None,
            facts: PumpFacts {
                node_names: vec!["a".to_string()],
                actual_tone_hz: 445.3125,
                asked_tone_hz: 440.0,
                tone_silent: false,
                ring_capacity: 16_384,
            },
        };
        assert_eq!(r.budget_ns(), 1_333_333);
        assert!(!r.passed(), "a run that measured nothing cannot pass");
        let lines = r.lines();
        assert!(lines.iter().any(|l| l.contains("tone: 445.3125 Hz")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("verdict:")),
            "the summary line must be there: {lines:?}"
        );
    }

    #[test]
    fn a_node_error_appears_in_the_report_as_a_fail() {
        let plan = decide(&[wasapi_48k()], &Wants { rate: 48_000, frames: 64 }).expect("plan");
        let r = RunResult {
            options: RunOptions::default(),
            device_name: "X".to_string(),
            plan,
            view: Counters::new().view(),
            cost: CostSummary::from_samples(&mut [], 1_333_333),
            guard_total: 0,
            real_seconds: 1.0,
            node_error: Some("block exceeds the chain's declared shape"),
            facts: PumpFacts {
                node_names: Vec::new(),
                actual_tone_hz: 0.0,
                asked_tone_hz: 0.0,
                tone_silent: true,
                ring_capacity: 0,
            },
        };
        let lines = r.lines().join("\n");
        assert!(lines.contains("FAIL node refused the block"), "{lines}");
        assert!(!r.passed());
    }
}
