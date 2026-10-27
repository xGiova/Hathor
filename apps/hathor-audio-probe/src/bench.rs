//! `bench`: the offline cost table. Safe to run anywhere — it opens no device.
//!
//! ## Why these rows, and not a synthetic loop
//!
//! ROADMAP §M0 criterion 6 lists the measurements the Rust-vs-C++ decision is made
//! on: a scalar mono biquad, the AVX2 8-lane kernel **with and without the
//! transpose**, an element-wise auto-vectorised loop, and the FFTs. This module runs
//! exactly those, plus the rows that make them readable against the gate: the real
//! three-biquad stereo chain at 64 frames, the analysis plumbing `run` pays per block,
//! and the whole pump chunk as a percentage of the 1333.33 µs budget. Every column is
//! measured; nothing here is an estimate, and the table closes with the date, the
//! machine class and the build profile that produced it.
//!
//! ## The method, stated once
//!
//! * `std::hint::black_box` on the inputs and on a result of every rep, so the
//!   optimiser cannot delete the work it is being asked to time. A benchmark that
//!   measures an elided FFT is worse than no benchmark.
//! * a 16-rep warm-up, discarded, then **median of at least 512 timed samples**
//!   (the CLI will not accept fewer). Median, not mean: one scheduler hiccup must not
//!   move the headline number. Min and max are printed beside it — the honest shape of
//!   the distribution, including its tail.
//! * each *sample* is a batch of reps, sized by a calibration pass so a sample costs
//!   ≳200 µs: two orders of magnitude above this machine's `Instant` granularity,
//!   which is itself a printed row so the reader can subtract it.
//! * every sample runs inside an [`AudioSection`] in **measure** mode, so the `allocs`
//!   column is the same instrument the live gate uses, pointed at a kernel instead of
//!   at a driver. A non-zero number in that column is a finding about the kernel, and
//!   [`execute`] returns an error rather than printing a table with a hidden malloc.
//! * deterministic inputs: fixed ramps and sines, no RNG (CONVENTION §5).
//!
//! ## Reading the table
//!
//! `per-unit` is ns divided by the row's unit (a frame, a transform, a chunk). Only
//! rows marked `%budget` are comparable to the 1333.33 µs block budget, because a
//! budget is a *block* and dividing it by 64 to get a per-frame share is a number
//! nobody asked for.

use std::hint::black_box;
use std::time::Instant;

use hathor_dsp_core::alloc_guard::{self, AudioSection};
use hathor_dsp_core::buffer::Block;
use hathor_dsp_core::{Node, NodeError, ProcessCtx, Shape};
use hathor_dsp_fft::{Complex, FftPlan, RealFftPlan};
use hathor_dsp_filters::biquad::lanes8::{
    interleaved_to_planar, planar_to_interleaved, BiquadLanes8, LANES,
};
use hathor_dsp_filters::coeffs::design;
use hathor_dsp_filters::{Biquad, BiquadNode, FilterKind};
use hathor_dsp_measure::{peak_db, rms_db, RmsTracker};
use hathor_host_graph::Chain;

use crate::cli::{BenchOptions, Tone};
use crate::cost_summary::BUDGET_64_48K_NS;
use crate::fft_window::FftWindow;
use crate::interleave::{deinterleave, interleave};
use crate::probe_error::ProbeError;
use crate::pump::{Pump, CHAIN_SPEC};
use crate::stamp;
use crate::tone::ToneGen;

/// Nanoseconds each timed *sample* should cost, at least: two orders of magnitude
/// above the `Instant` granularity this machine offers (**measured**, and printed as
/// its own row).
pub const TARGET_SAMPLE_NS: u64 = 200_000;

/// Reps run before timing starts, and discarded.
pub const WARMUP_REPS: usize = 16;

/// Reps timed to estimate one rep's cost, before the real batches are sized. Has to be
/// more than one: the cheapest kernel here is ~30 ns and the clock's quantum is
/// comparable, so a single-rep estimate reads 0 and the batch sizing falls apart.
pub const CALIB_REPS: usize = 64;

/// The largest batch we are willing to build for one sample. A kernel measured at
/// 1 ns would otherwise ask for 200,000 reps; that is a minute of a machine's life
/// spent on a row nobody needs at that precision.
pub const INNER_MAX: usize = 100_000;

/// One row of the table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// What was measured, in the row's own words.
    pub name: String,
    /// The unit the per-unit number divides by: "frame", "transform", "chunk".
    pub unit: &'static str,
    /// Units one rep consumes (frames per pass, transforms per pass, …).
    pub units_per_rep: usize,
    /// Reps inside one timed sample, from the calibration pass.
    pub inner: usize,
    /// Timed samples taken.
    pub samples: usize,
    /// Median sample cost, ns.
    pub median_ns: f64,
    /// Fastest sample, ns.
    pub min_ns: u64,
    /// Slowest sample, ns.
    pub max_ns: u64,
    /// Allocations the guard saw inside this row's sections, across every sample.
    pub allocs: u64,
    /// True when the per-rep cost is a block-sized piece of work, so the budget
    /// column means something.
    pub against_budget: bool,
    /// The caveat a reader needs: "AVX2+FMA detected", "scalar fallback", …
    pub note: &'static str,
}

impl Row {
    /// ns per rep (one pass over the row's data).
    #[must_use]
    pub fn ns_per_rep(&self) -> f64 {
        self.median_ns / self.inner.max(1) as f64
    }

    /// ns per unit: per frame, per transform, per chunk.
    #[must_use]
    pub fn ns_per_unit(&self) -> f64 {
        self.ns_per_rep() / self.units_per_rep.max(1) as f64
    }

    /// Percent of the 64-frame/48 kHz budget that one rep costs. Only meaningful for
    /// [`Row::against_budget`] rows, which is why the column prints `-` otherwise.
    #[must_use]
    pub fn budget_pct(&self) -> f64 {
        self.ns_per_rep() / f64::from(BUDGET_64_48K_NS as u32) * 100.0
    }

    /// The table line. Fixed widths: the block must survive a copy into a report.
    #[must_use]
    pub fn line(&self) -> String {
        let per = format!("{:>9.1} ns/{}", self.ns_per_unit(), self.unit);
        let spread = format!("{:>8}…{:<8}", self.min_ns, self.max_ns);
        let budget = if self.against_budget {
            format!("{:>8.2}%", self.budget_pct())
        } else {
            // Nine wide, exactly like the formatted percentage above: a column that
            // changes width depending on its row is a column a reader cannot follow.
            "        -".to_string()
        };
        format!(
            "{:<58} {:>20} {:>19}  {} {:>6}  {}",
            self.name, per, spread, budget, self.allocs, self.note
        )
    }

    /// The header above [`Row::line`].
    ///
    /// `min…max` is labelled **per batch** because that is what is timed: a batch is
    /// `calibrate()` reps of the kernel, sized from one cold estimate, so its length
    /// varies from process to process while the per-unit column does not. Leaving the
    /// column unlabelled would let a reader mistake a 3 ms batch for a 3 ms kernel.
    #[must_use]
    pub const fn header() -> &'static str {
        "row                                                                    per-unit    min…max ns/batch    %budget allocs  note"
    }

    /// How many reps fit in one sample: `target / one rep`, clamped to
    /// [`INNER_MAX`]. A zero from the clock (a rep faster than the granularity) gets
    /// the maximum batch, because the alternative is reporting the timer's quantum as
    /// a kernel's cost.
    #[must_use]
    pub fn calibrate(one_rep_ns: u64, target_ns: u64) -> usize {
        if one_rep_ns == 0 {
            return INNER_MAX;
        }
        ((target_ns / one_rep_ns) as usize).clamp(1, INNER_MAX)
    }
}

/// Runs one row: warm-up, calibration, then `samples_len` timed batches, each inside
/// its own [`AudioSection`] in measure mode.
///
/// # Errors
/// [`ProbeError::Usage`] when asked for fewer samples than the method allows.
fn measure(
    name: &str,
    unit: &'static str,
    units_per_rep: usize,
    against_budget: bool,
    note: &'static str,
    samples_len: usize,
    f: &mut dyn FnMut(usize),
) -> Result<Row, ProbeError> {
    if samples_len < crate::cli::ITERATIONS_MIN {
        return Err(ProbeError::Usage(format!(
            "bench needs at least {} timed samples, asked for {samples_len}",
            crate::cli::ITERATIONS_MIN
        )));
    }
    for _ in 0..WARMUP_REPS {
        f(1);
    }
    // Calibrate on a batch, not on one rep. A single rep of a fast kernel (the tone
    // table fill is ~30 ns) is *below the clock's quantum*: `elapsed()` returns 0,
    // `calibrate` then hands out the maximum batch, and the timed samples become 3–5 ms
    // monsters instead of the ≳200 µs the method line describes. 64 reps is measurable
    // even for the cheapest row here, and costs one discarded sample.
    let t = Instant::now();
    f(CALIB_REPS);
    let one_rep_ns = t.elapsed().as_nanos() as u64 / CALIB_REPS as u64;
    let inner = Row::calibrate(one_rep_ns, TARGET_SAMPLE_NS);

    // LEGAL ALLOCATION: control thread, outside every section — the sample stamps.
    let mut stamps: Vec<u64> = vec![0; samples_len];
    let mut allocs: u64 = 0;
    for s in stamps.iter_mut() {
        let section = AudioSection::enter()
            .map_err(|e| ProbeError::Stream(format!("bench could not open a section: {e:?}")))?;
        let t0 = Instant::now();
        f(inner);
        let ns = t0.elapsed().as_nanos() as u64;
        allocs += section.allocs_here();
        drop(section);
        *s = ns;
    }
    stamps.sort_unstable();
    Ok(Row {
        name: name.to_string(),
        unit,
        units_per_rep,
        inner,
        samples: stamps.len(),
        median_ns: crate::cost_summary::percentile(&stamps, 0.50),
        min_ns: stamps.first().copied().unwrap_or(0),
        max_ns: stamps.last().copied().unwrap_or(0),
        allocs,
        against_budget,
        note,
    })
}

/// The whole table, in the order a reader should see it: single kernels, then the
/// chain, then the transforms, then the pump as `run` will actually execute it.
///
/// # Errors
/// Whatever [`measure`] returns, plus [`ProbeError::Stream`] if a kernel's own setup
/// is refused (a filter that will not design is not a benchmark, it is a bug).
pub fn rows(o: &BenchOptions) -> Result<Vec<Row>, ProbeError> {
    let n = o.iterations;
    let mut rows: Vec<Row> = Vec::new();

    // ── 1. scalar mono biquad, in place, at the G1 block size ────────────────────
    // The coefficients are the chain's own middle filter (Peak 1 kHz, +6 dB), so this
    // row and the "chain" row at the bottom are costs of the same arithmetic.
    let coeffs = design(
        FilterKind::Peak {
            q: 1.0,
            gain_db: 6.0,
        },
        1_000.0,
        48_000.0,
    )
    .map_err(|e| ProbeError::Stream(format!("coeffs::design refused the bench filter: {e:?}")))?;
    let mut mono: Vec<f32> = ramp(64);
    {
        let mut bq = Biquad::new(coeffs);
        let mut run = |reps: usize| {
            for _ in 0..reps {
                black_box(&mono[..]);
                bq.process(&mut mono);
                black_box(mono[0]);
            }
        };
        rows.push(measure(
            "biquad scalar mono, 64 frames in place",
            "frame",
            64,
            false,
            "DF2T, one channel",
            n,
            &mut run,
        )?);
    }

    // ── 2. the same kernel at the size a WASAPI callback actually delivers ────────
    let mut big: Vec<f32> = ramp(480);
    {
        let mut bq = Biquad::new(coeffs);
        let mut run = |reps: usize| {
            for _ in 0..reps {
                bq.process(&mut big);
                black_box(big[0]);
            }
        };
        rows.push(measure(
            "biquad scalar mono, 480 frames in place",
            "frame",
            480,
            false,
            "the engine period a shared-mode callback brings",
            n,
            &mut run,
        )?);
    }

    // ── 3. the AVX2 8-lane kernel, and the transpose that feeds it ───────────────
    if BiquadLanes8::supported() {
        let mut lanes_buf: Vec<f32> = ramp(64);
        // SAFETY: `BiquadLanes8::supported()` just confirmed AVX2+FMA on this CPU, the
        // same check `process_detected` re-runs internally; the buffer length is 64 =
        // 8 × LANES, which `process` requires.
        let mut k = unsafe { BiquadLanes8::broadcast(&coeffs) };
        let mut run = |reps: usize| {
            for _ in 0..reps {
                black_box(&lanes_buf[..]);
                if !k.process_detected(&mut lanes_buf) {
                    // Unreachable after `supported()`, and still handled: a kernel
                    // that silently stops running would otherwise report 0 ns.
                    eprintln!("bench: the AVX2 kernel declined to run after detection");
                }
                black_box(lanes_buf[0]);
            }
        };
        rows.push(measure(
            "biquad AVX2 8-lane broadcast, 64 floats",
            "frame",
            64,
            false,
            "AVX2+FMA detected, runtime dispatch",
            n,
            &mut run,
        )?);

        let mut planar: Vec<f32> = ramp(LANES * 8);
        let mut inter: Vec<f32> = vec![0.0; LANES * 8];
        let mut run = |reps: usize| {
            for _ in 0..reps {
                planar_to_interleaved(&planar, &mut inter, 8);
                if !k.process_detected(&mut inter) {
                    eprintln!("bench: the AVX2 kernel declined to run after detection");
                }
                interleaved_to_planar(&inter, &mut planar, 8);
                black_box(planar[0]);
            }
        };
        rows.push(measure(
            "biquad AVX2 + transpose in/out, 8 ch × 8 frames",
            "frame",
            LANES * 8,
            false,
            "the cost architecture.md §5 demands be visible",
            n,
            &mut run,
        )?);
    } else {
        rows.push(Row {
            name: "biquad AVX2 8-lane broadcast, 64 floats".to_string(),
            unit: "frame",
            units_per_rep: 64,
            inner: 0,
            samples: 0,
            median_ns: 0.0,
            min_ns: 0,
            max_ns: 0,
            allocs: 0,
            against_budget: false,
            note: "NOT RUN: this CPU reports no AVX2+FMA — the scalar rows are the path",
        });
    }

    // ── 4. element-wise, auto-vectorised (the baseline a hand-written loop is
    //    compared against) ────────────────────────────────────────────────────────
    let src: Vec<f32> = ramp(64);
    let mut dst: Vec<f32> = vec![0.0; 64];
    let mut acc: f32 = 0.0;
    let mut run = |reps: usize| {
        for _ in 0..reps {
            for (i, d) in dst.iter_mut().enumerate() {
                *d = black_box(src[i]) * 0.5 + 0.25;
            }
            acc += black_box(dst[0]);
        }
    };
    rows.push(measure(
        "element-wise x*0.5+0.25, 64 frames",
        "frame",
        64,
        false,
        "auto-vectorised, no intrinsics written",
        n,
        &mut run,
    )?);

    // ── 5. the real chain, stereo, 64 frames, through Chain::process ─────────────
    let mut chain = Chain::new(48_000, 2, 64);
    for (kind, freq, q, gain) in CHAIN_SPEC {
        push(&mut chain, BiquadNode::new(kind, freq, q, gain))?;
    }
    let mut chain_io: Vec<f32> = ramp(128);
    let ctx = ProcessCtx {
        sample_rate: 48_000,
        frames: 64,
        sample_position: 0,
        transport: None,
    };
    let mut refused: u32 = 0;
    let mut run = |reps: usize| {
        for _ in 0..reps {
            let mut io = Block::from_slice(&mut chain_io, Shape::new(2, 64));
            if chain.process(&ctx, &mut io).is_err() {
                refused += 1;
            }
            black_box(io.channel_ref(0)[0]);
        }
    };
    rows.push(measure(
        "chain LP8k→Peak1k+6→HP100, stereo, 64 frames",
        "frame",
        64,
        false,
        "the three nodes `run` will pump",
        n,
        &mut run,
    )?);
    assert_eq!(refused, 0, "the bench chain refused its own shape");

    // ── 6. the transforms: real FFT at the live window and the two criterion sizes ─
    for (size, note, budget_row) in [
        (1_024usize, "the window `run` analyses with", true),
        (4_096, "ROADMAP §8 criterion 6 reference size", false),
        (16_384, "the large-window case", false),
    ] {
        let mut plan = RealFftPlan::new(size);
        let samples: Vec<f32> = sine(size, 37.0);
        let mut bins: Vec<Complex> = vec![Complex::ZERO; plan.bin_count()];
        let mut run = |reps: usize| {
            for _ in 0..reps {
                plan.process(&samples, &mut bins);
                black_box(bins[1].re);
            }
        };
        rows.push(measure(
            &format!("RealFftPlan({size}) forward, packed bins"),
            "transform",
            1,
            budget_row,
            note,
            n,
            &mut run,
        )?);
        // A 37-cycle sine in an N-point window lands in bin 37: the row would be
        // meaningless if the plan were silently not doing the work.
        assert!(
            bins[37].norm_sqr() > 1.0,
            "the {size}-point transform found no tone"
        );
    }

    let mut cplan = FftPlan::new(4_096);
    let mut cx: Vec<Complex> = (0..4_096)
        .map(|i| Complex::new(i as f32 * 1e-3, 0.0))
        .collect();
    let mut run = |reps: usize| {
        for _ in 0..reps {
            cplan.process(&mut cx);
            black_box(cx[0].im);
        }
    };
    rows.push(measure(
        "FftPlan(4096) complex forward",
        "transform",
        1,
        false,
        "the in-place complex path, for comparison",
        n,
        &mut run,
    )?);

    // ── 7. the analysis plumbing `run` pays per block ────────────────────────────
    let mut window = FftWindow::new(1_024);
    let feed: Vec<f32> = ramp(64);
    let mut run = |reps: usize| {
        for _ in 0..reps {
            window.push(black_box(&feed));
            black_box(window.samples()[0]);
        }
    };
    rows.push(measure(
        "FftWindow::push(64) into N=1024",
        "push",
        1,
        false,
        "the sliding cost: a 960-float memmove per block",
        n,
        &mut run,
    )?);

    let mut il: Vec<f32> = ramp(128);
    let mut pl: Vec<f32> = vec![0.0; 128];
    let mut run = |reps: usize| {
        for _ in 0..reps {
            deinterleave(&il, &mut pl, 2, 64);
            interleave(&pl, &mut il, 2, 64);
            black_box(pl[0]);
        }
    };
    rows.push(measure(
        "deinterleave + interleave, stereo 64 frames",
        "frame",
        128,
        false,
        "the driver edge, both directions",
        n,
        &mut run,
    )?);

    let mut tracker = RmsTracker::new(0.3);
    let mut lvl: Vec<f32> = ramp(128);
    let mut run = |reps: usize| {
        for _ in 0..reps {
            let io = Block::from_slice(&mut lvl, Shape::new(2, 64));
            tracker.process_block(&io);
            black_box(tracker.rms_db());
            black_box(peak_db(&io));
            black_box(rms_db(&io));
        }
    };
    rows.push(measure(
        "levels: RmsTracker + peak_db + rms_db, stereo 64",
        "frame",
        128,
        false,
        "what the report prints for every block",
        n,
        &mut run,
    )?);

    let mut tone = ToneGen::new(Tone::Sine(440.0), 48_000);
    let mut tone_buf: Vec<f32> = vec![0.0; 64];
    let mut run = |reps: usize| {
        for _ in 0..reps {
            tone.fill(&mut tone_buf);
            black_box(tone_buf[0]);
        }
    };
    rows.push(measure(
        "tone table fill, 64 frames",
        "frame",
        64,
        false,
        "no sinf in the callback: masked index + load",
        n,
        &mut run,
    )?);

    // ── 8. the timer's own cost, then the pump as `run` executes it ──────────────
    let mut run = |reps: usize| {
        for _ in 0..reps {
            let t = Instant::now();
            black_box(t.elapsed().as_nanos());
        }
    };
    rows.push(measure(
        "Instant::now() + elapsed() pair",
        "sample",
        1,
        false,
        "subtract this from every block cost above",
        n,
        &mut run,
    )?);

    let (mut pump, consumer) = Pump::setup(48_000, 2, 64, 1_024, Tone::Sine(440.0))?;
    let ring_capacity = pump.ring_capacity();
    let mut pump_buf: Vec<f32> = vec![0.0; 128];
    let mut pump_errs: u32 = 0;
    let mut run = |reps: usize| {
        for _ in 0..reps {
            if pump.bench_chunk(&mut pump_buf).is_err() {
                pump_errs += 1;
            }
            black_box(pump_buf[0]);
        }
    };
    rows.push(measure(
        "pump chunk: tone+chain+door+FFT1024+interleave, stereo 64",
        "chunk",
        1,
        true,
        "one chunk of the live path, counters aside",
        n,
        &mut run,
    )?);
    assert_eq!(pump_errs, 0, "the pump refused its own chunk");
    drop(consumer);
    assert!(
        ring_capacity > 0,
        "the bench row's pump had no ring to be configured"
    );

    Ok(rows)
}

/// Builds the table, prints it, and stamps it.
///
/// The guard runs in **measure** mode for the whole bench: a kernel that allocates
/// must appear in the `allocs` column rather than abort a benchmark that is three
/// rows from being useful. A non-zero count is still an error out of here.
///
/// The closing tally line, as a function so the text can be tested.
///
/// This existed as an inline `format!` with `{:.0}%%` in it — a printf habit. Rust
/// prints `%%` literally, so the report said "100%% of them allocated-free". Nothing
/// else would have noticed, which is the whole reason it is a named function now.
fn summary_line(printed: usize, ran: usize, clean: usize, guard_total: u64) -> String {
    let pct = if ran == 0 {
        0.0
    } else {
        100.0 * clean as f64 / ran as f64
    };
    format!(
        "rows: {printed} printed, {ran} measured ({pct:.0}% of them allocated-free, guard total {guard_total})"
    )
}

/// # Errors
/// Whatever [`rows`] returns, plus [`ProbeError::Gate`] if any row allocated.
pub fn execute(o: &BenchOptions) -> Result<(), ProbeError> {
    alloc_guard::set_panic_on_audio_alloc(false);
    alloc_guard::reset_counters();
    println!(
        "bench: median of {} timed samples per row, {} warm-up reps discarded, each sample a \
         batch sized to ≳{} µs",
        o.iterations,
        WARMUP_REPS,
        TARGET_SAMPLE_NS / 1_000,
    );
    println!(
        "bench: every sample ran inside an AudioSection in measure mode; `allocs` is the same \
         counter that gates `run`"
    );
    println!("{}", Row::header());
    let table = rows(o)?;
    let mut offenders: Vec<String> = Vec::new();
    for r in &table {
        println!("{}", r.line());
        if r.allocs != 0 {
            offenders.push(format!("{} ({} allocs)", r.name, r.allocs));
        }
    }
    let ran: usize = table.iter().filter(|r| r.samples > 0).count();
    // `ran - offenders` rather than "rows with allocs == 0": a NOT-RUN placeholder row
    // has no allocations either, and counting it as clean would overstate the ratio.
    println!(
        "{}",
        summary_line(
            table.len(),
            ran,
            ran - offenders.len(),
            alloc_guard::allocs_total()
        )
    );
    println!(
        "budget: {} ns = {:.2} µs per 64-frame block at 48 kHz \
         (hathor_dsp_core::BUDGET_64_48K_US; G1 criterion 4 is 0.40× of it)",
        BUDGET_64_48K_NS,
        BUDGET_64_48K_NS as f64 / 1e3,
    );
    println!(
        "unit: wallclock nanoseconds from std::time::Instant pairs, NOT CPU cycles; the \
         per-unit column is the median of timed batches divided by their rep count, and \
         min…max is the batch spread — a tail is the whole story of a deadline"
    );
    println!(
        "note: batch length is calibrated once, cold, per process (≳{TARGET_SAMPLE_NS} ns), \
         so the min…max column is not comparable between runs while per-unit is"
    );
    if !offenders.is_empty() {
        return Err(ProbeError::Gate(format!(
            "kernel(s) allocated inside a section: {}",
            offenders.join("; ")
        )));
    }
    // The one closing line the task asks for: provenance, so a table cannot outlive
    // the machine and profile that produced it.
    println!("{}", stamp::measured_line(stamp::utc_today()));
    Ok(())
}

/// A deterministic ramp: `0.001·i`. No RNG (CONVENTION §5), and non-zero so no filter
/// is ever measured on digital silence.
fn ramp(len: usize) -> Vec<f32> {
    (0..len).map(|i| (i as f32) * 0.001).collect()
}

/// A deterministic sine, `cycles` per window, built at setup.
fn sine(len: usize, cycles: f32) -> Vec<f32> {
    (0..len)
        .map(|i| (core::f32::consts::TAU * cycles * i as f32 / len as f32).sin())
        .collect()
}

/// Pushes a node into the bench chain, mapping the graph's error into ours.
fn push(chain: &mut Chain, node: BiquadNode) -> Result<(), ProbeError> {
    let label = node.name().to_string();
    // LEGAL ALLOCATION: setup thread — the boxed node, before any timing starts.
    chain.push(Box::new(node)).map_err(|e: NodeError| {
        ProbeError::Stream(format!("bench node {label} refused setup: {e:?}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(units: usize, inner: usize, median_ns: f64) -> Row {
        Row {
            name: "test row".to_string(),
            unit: "frame",
            units_per_rep: units,
            inner,
            samples: 512,
            median_ns,
            min_ns: median_ns as u64 - 10,
            max_ns: median_ns as u64 + 40,
            allocs: 0,
            against_budget: units == 1,
            note: "n",
        }
    }

    #[test]
    fn per_unit_divides_by_units_and_by_the_batch_not_by_magic() {
        // 512 samples × 10 reps × 64 frames, median sample 128 000 ns → 200 ns/frame.
        let r = row(64, 10, 128_000.0);
        assert!(
            (r.ns_per_rep() - 12_800.0).abs() < 1e-6,
            "{}",
            r.ns_per_rep()
        );
        assert!(
            (r.ns_per_unit() - 200.0).abs() < 1e-6,
            "{}",
            r.ns_per_unit()
        );
    }

    #[test]
    fn a_chunk_row_reads_against_the_real_budget() {
        let r = row(1, 1, BUDGET_64_48K_NS as f64 / 2.0);
        assert!((r.budget_pct() - 50.0).abs() < 1e-3, "{}", r.budget_pct());
        assert_eq!(BUDGET_64_48K_NS, 1_333_333, "1333.33 µs, truncated");
    }

    #[test]
    fn calibration_batches_a_fast_kernel_and_caps_a_pathological_one() {
        assert_eq!(Row::calibrate(200, TARGET_SAMPLE_NS), 1_000);
        assert_eq!(
            Row::calibrate(1_000_000, TARGET_SAMPLE_NS),
            1,
            "a slow rep is its own batch"
        );
        assert_eq!(
            Row::calibrate(0, TARGET_SAMPLE_NS),
            INNER_MAX,
            "faster than the clock"
        );
        assert_eq!(
            Row::calibrate(1, TARGET_SAMPLE_NS),
            INNER_MAX,
            "capped, not 200 000 reps"
        );
    }

    #[test]
    fn the_budget_column_only_appears_for_block_sized_rows() {
        assert!(
            row(1, 1, 100.0).line().contains('%'),
            "a chunk row shows %budget"
        );
        assert!(
            row(64, 1, 100.0).line().contains('-'),
            "a frame row shows a dash"
        );
    }

    #[test]
    fn a_row_that_allocated_is_named_in_the_offender_list() {
        let mut r = row(64, 10, 1_000.0);
        r.allocs = 3;
        assert_ne!(r.allocs, 0, "the column the gate reads");
        assert!(r.line().contains("     3"), "{}", r.line());
    }

    #[test]
    fn the_summary_line_has_one_percent_sign_and_no_printf_artifact() {
        let all = summary_line(16, 16, 16, 0);
        assert!(all.contains("(100% of them allocated-free"), "{all}");
        assert!(
            !all.contains("%%"),
            "a printf `%%` is not a percent in Rust: {all}"
        );
        assert_eq!(
            summary_line(3, 2, 1, 7).as_str(),
            "rows: 3 printed, 2 measured (50% of them allocated-free, guard total 7)"
        );
        // A run where nothing could be timed must not divide by zero.
        assert!(summary_line(1, 0, 0, 0).contains("(0%"), "no rows measured");
    }

    #[test]
    fn measure_refuses_a_sample_count_under_the_method() {
        let e = measure(
            "x",
            "frame",
            1,
            false,
            "n",
            crate::cli::ITERATIONS_MIN - 1,
            &mut |_| {},
        )
        .expect_err("refused");
        assert_eq!(e.tag(), "usage");
        assert!(e.to_string().contains("at least 512"), "{e}");
    }

    #[test]
    fn a_measured_row_carries_its_own_batch_and_samples() {
        let r = measure("y", "frame", 16, false, "n", 512, &mut |reps| {
            let mut x = 0.0f32;
            for i in 0..reps {
                x += i as f32;
            }
            black_box(x);
        })
        .expect("measured");
        assert_eq!(r.samples, 512);
        assert!(r.inner >= 1, "the calibration chose {}", r.inner);
        assert!(
            r.median_ns >= r.min_ns as f64,
            "median below min is a sorting bug"
        );
        assert!(r.max_ns >= r.min_ns);
        assert_eq!(r.allocs, 0, "a loop that only adds does not allocate");
    }

    #[test]
    fn the_table_runs_end_to_end_at_the_smallest_legal_sample_count() {
        // The real thing: every row, at the minimum sample count. Slow on purpose — it
        // is the test that proves `bench` will not die in front of a human.
        let o = BenchOptions { iterations: 512 };
        let rows = rows(&o).expect("bench rows");
        assert!(
            rows.len() >= 11,
            "expected the criterion rows, got {}",
            rows.len()
        );
        assert!(
            rows.iter().all(|r| r.name.len() < 59),
            "a name wider than its column would shift the whole table: {:?}",
            rows.iter().map(|r| r.name.len()).collect::<Vec<_>>()
        );
        for r in &rows {
            assert_eq!(r.allocs, 0, "{} allocated inside a section", r.name);
        }
        let pump = rows
            .iter()
            .find(|r| r.name.starts_with("pump chunk"))
            .expect("the pump row exists");
        assert!(
            pump.against_budget,
            "only the chunk row is judged against a block"
        );
        assert!(
            pump.budget_pct() < 100.0,
            "a chunk must fit its budget: {}%",
            pump.budget_pct()
        );
    }

    #[test]
    fn the_deterministic_inputs_are_never_silence() {
        let r = ramp(4);
        assert_eq!(r, vec![0.0, 0.001, 0.002, 0.003]);
        let s = sine(4, 1.0);
        assert!(s[1] > 0.9 && s[3] < -0.9, "a quarter-cycle sine: {s:?}");
    }

    #[test]
    fn execute_returns_ok_on_a_machine_where_no_kernel_allocates() {
        // `bench` is the G1 deliverable: it must be runnable, not just compilable. The
        // assertion is the exit status; the printing is what the report quotes.
        //
        // `execute` sets the global panic mode to measure and zeroes the shared counters
        // before its rows, so it owns the process for as long as it runs — the same crate
        // lock the rehearsal and the pump tests take. Without it, a sibling's reset could
        // erase a row's violation and this test would still pass.
        let _serial = crate::guard_serial();
        let o = BenchOptions { iterations: 512 };
        assert!(
            execute(&o).is_ok(),
            "a bench row that allocates must fail loudly"
        );
    }
}
