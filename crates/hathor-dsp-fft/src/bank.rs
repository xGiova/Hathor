//! The multi-rate bank: the SPAN-style analyzer core (phase M1 of the module doc).
//!
//! ## Why a *bank* and not "an FFT with a ring buffer"
//!
//! The time-frequency constraint cannot be cheated, only architected: one window
//! cannot be both reactive (a 256-sample frame: one block of delay at 64/48k) and
//! detailed at 20 Hz (that needs 16384 samples = 341 ms). The table in the module
//! doc is normative — four tiers, each updating at hop = N/4 (75% overlap):
//!
//! | tier size | updates every | serves |
//! |---|---|---|
//! | 256 | 64 frames | 4–24 kHz (reactivity) |
//! | 1024 | 256 frames | 500 Hz–4 kHz |
//! | 4096 | 1024 frames | 100–500 Hz |
//! | 16384 | 4096 frames | 20–100 Hz (resolution) |
//!
//! Each tier accumulates into its own ring, multiplies a precomputed Hann, runs
//! [`RealFftPlan`](crate::RealFftPlan), and spreads `|X|²` over the **shared**
//! 1024-bin log axis by two-point weights fixed at setup. A bin's level is the max
//! of the tier contributions: that choice is what makes the junction continuous —
//! every tier reads a bin-center sine within the same half-bin scalloping bound, so
//! the max is never worse than either tier alone, and the boundary-bin test against
//! a known signal (see the tests) pins the claim.
//!
//! ## The hot-path shape (this is *why* the structure looks like this)
//!
//! * [`FftBank::new`] allocates every byte the bank will ever own: rings, Hanns,
//!   mapping tables, smoothing state.
//! * [`FftBank::process`] is integer cursor arithmetic, fused multiply-adds and
//!   slice writes. No allocation, no lock, no transcendental: the exponential
//!   smoothing constants and the log-bin mapping (which would need `ln`) are f64
//!   decisions at setup narrowed to f32 multipliers and indices. A block crossing
//!   several hops refreshes a tier **once** against the newest frame — the analyzer
//!   shows the latest spectrum, and the per-call cost has a hard ceiling.
//! * `render_db` is deliberately **not** part of `process`: dB conversion is
//!   `log10` work that belongs to the display thread, never to a deadline. The
//!   audio thread produces linear `levels()`; the UI calls `render_db()` at frame
//!   rate and reads `db()`. (Decision reported to the lead: the task's "output =
//!   the dB buffer" is honored at the *view*, not inside the callback — the same
//!   split SPAN itself must do, and what keeps `process` transcendental-free.)

use crate::complex::Complex;
use crate::plan::bin_freq;
use crate::real::RealFftPlan;
use std::f64::consts::LN_10;

/// The normative tier sizes from the module doc's table (index = tier id).
pub const TIER_SIZES: [usize; 4] = [256, 1024, 4096, 16384];
/// Band served per tier, Hz — the table's third column, in tier order. Bands
/// declare *primary* ownership; the per-bin cross-tier max lets neighbouring
/// tiers contribute as well (that is the junction mechanism).
pub const TIER_BANDS_HZ: [(f32, f32); 4] = [
    (4_000.0, 24_000.0),
    (500.0, 4_000.0),
    (100.0, 500.0),
    (20.0, 100.0),
];
/// Bottom of the shared log axis, Hz.
pub const AXIS_F_LO: f32 = 20.0;
/// Top of the shared log axis, Hz — Nyquist at 48 kHz, the doc's table ceiling.
pub const AXIS_F_HI: f32 = 24_000.0;
/// Default shared-bin count (the doc's "~1024").
pub const DEFAULT_BINS: usize = 1024;

/// Per-tier smoothing constants. Every derived value (`α`, the decay multiplier)
/// is an exponential of these, computed once at setup: the struct crosses no hot
/// path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TierTuning {
    /// One-pole time constant of the moving average, seconds. `0.0` bypasses the
    /// average: every trigger replaces the state with its reading (test mode).
    pub avg_time_s: f32,
    /// Peak-hold decay in dB per second. `0.0` means the hold never falls (a
    /// max-of-history; constant-signal test mode).
    pub decay_db_per_s: f32,
}

/// Everything the bank needs to exist: fixed at setup, reachable afterwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BankParams {
    /// Sample rate of the driving stream: the mapping is in Hz, so the bank must
    /// be told (range asserted in [`FftBank::new`]).
    pub sample_rate: f32,
    /// Shared log-bin count.
    pub bins: usize,
    /// What `render_db` clamps silence to.
    pub floor_db: f32,
    /// Smoothing per tier.
    pub tuning: [TierTuning; 4],
}

impl BankParams {
    /// Defaults for `sample_rate`: the SPAN-like profile — reactive top, patient
    /// bottom. These are **starting points, not measured curves**: the M2
    /// listening pass tunes them against reference displays (flagged as expected,
    /// not measured, exactly as the project demands).
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            bins: DEFAULT_BINS,
            floor_db: -120.0,
            // [256, 1024, 4096, 16384]: τ fast→slow, decay steep→gentle.
            tuning: [
                TierTuning {
                    avg_time_s: 0.010,
                    decay_db_per_s: 45.0,
                },
                TierTuning {
                    avg_time_s: 0.030,
                    decay_db_per_s: 30.0,
                },
                TierTuning {
                    avg_time_s: 0.100,
                    decay_db_per_s: 20.0,
                },
                TierTuning {
                    avg_time_s: 0.250,
                    decay_db_per_s: 20.0,
                },
            ],
        }
    }

    /// Bypass tuning: instant average, non-decaying hold. The mode every analytic
    /// comparison in the tests runs in.
    pub fn bypass(sample_rate: f32) -> Self {
        let mut p = Self::new(sample_rate);
        p.tuning = [TierTuning {
            avg_time_s: 0.0,
            decay_db_per_s: 0.0,
        }; 4];
        p
    }
}

/// One FFT bin's destination on the log axis: a two-point spread
/// (`i0·w0 + i1·w1`, `w0 + w1 = 1`), weights decided at setup so no `ln` survives
/// into `process`. `k` is stored because bins outside the axis are skipped: the
/// map is *sparse* in k and can only be walked by its own entries.
#[derive(Debug, Clone, Copy)]
struct MapEntry {
    k: u16,
    i0: u16,
    w0: f32,
    i1: u16,
    w1: f32,
}

/// Axis arithmetic in one place (setup and tests): the log position of frequency
/// `f`, in bins. The tests reproduce the mapping through this formula — the
/// independent part of every reference is always the *spectrum*, never the axis.
pub(crate) fn log_position(f: f64, bins: usize) -> f64 {
    ((f / AXIS_F_LO as f64).ln() / (AXIS_F_HI as f64 / AXIS_F_LO as f64).ln()) * (bins - 1) as f64
}

/// One tier's whole world: ring, plan, window, mapping, smoothing state.
struct Tier {
    plan: RealFftPlan,
    /// Ring duplicated to 2N: the last-N frame from any cursor is one contiguous
    /// read (`ring[pos .. pos + N]`), no per-sample modulo in the gather.
    ring: Vec<f32>,
    hann: Vec<f32>,
    map: Vec<MapEntry>,
    /// Cursor into `ring` (the next write position), always `< N`.
    pos: usize,
    /// Absolute frame index at which this tier is next due to refresh.
    next_due: u64,
    /// Exponential moving-average and peak-hold state (linear power), per shared
    /// bin. Sized to the full axis so the cross-tier max is one flat read.
    sm: Vec<f32>,
    hold: Vec<f32>,
    /// One-pole step over one trigger gap: `1 − e^{−T/τ}` (τ = 0 → exactly 1).
    alpha: f32,
    /// Linear-power decay multiplier per trigger: `10^{−(dB/s)·T/10}`.
    decay_per_trigger: f32,
    /// `(4/N)²`: a full-scale sine reads power 1.0 (0 dBFS) at any tier. Hann
    /// halves the on-bin peak and the cosine halves it again: the bin holds N/4.
    norm: f32,
    /// First/last shared bin this tier's map touches (inclusive): the update
    /// window, so a trigger never walks the whole axis for nothing.
    lo: usize,
    hi: usize,
}

/// The multi-rate analyzer bank (module doc §Phase M1): feed it the frames of a
/// single channel (the caller mixes stereo down before here), read linear
/// [`FftBank::levels`] on the audio thread or the dB view on the display thread.
pub struct FftBank {
    params: BankParams,
    tiers: [Tier; 4],
    /// Total frames fed since construction/reset.
    written: u64,
    /// Shared scratch sized for the largest tier: the windowed gather frame and
    /// its half-spectrum bins.
    win: Vec<f32>,
    bins: Vec<Complex>,
    /// Per-trigger accumulation over the shared axis, cleared over the tier's
    /// touched range each time (no growth, no alloc).
    acc: Vec<f32>,
    /// Linear normalized power per shared bin: the audio-thread output.
    levels: Vec<f32>,
    /// The display buffer `render_db` fills — never touched by `process`.
    db: Vec<f32>,
}

impl FftBank {
    /// Builds four plans, four rings, four Hann windows, four mapping tables and
    /// all smoothing state. **The only allocation site of the type** (invariant 1;
    /// the same claim as [`FftPlan::new`](crate::FftPlan::new), guarded by the
    /// same mechanism — see `tests/alloc_contract.rs`).
    ///
    /// # Panics
    /// If `sample_rate` is outside `[44_100, 192_000]`, `bins` is not in
    /// `[16, 65535]` (the map stores `u16` indices), or a tuning time is negative.
    pub fn new(params: &BankParams) -> Self {
        assert!(
            (44_100.0..=192_000.0).contains(&params.sample_rate),
            "FftBank sample rate must be 44.1k–192k, got {}",
            params.sample_rate
        );
        assert!(
            (16..=u16::MAX as usize).contains(&params.bins),
            "FftBank shared bin count must be 16..=65535, got {}",
            params.bins
        );
        // Setup allocations, declared per CONVENTION §2.2. Every transcendental
        // this bank will ever need is right here: the Hanns, the ln()s of the
        // mapping, the exp()s of the smoothing.
        let f_lo = AXIS_F_LO as f64;
        let max_n = TIER_SIZES[3];
        assert!(
            max_n >= TIER_SIZES[0] && TIER_SIZES.windows(2).all(|w| w[0] < w[1]),
            "the tier table must ascend"
        );
        let tiers: [Tier; 4] = core::array::from_fn(|t| {
            let n = TIER_SIZES[t];
            let nc = n / 2;
            let hop = n / 4;
            let tuning = params.tuning[t];
            assert!(
                tuning.avg_time_s >= 0.0 && tuning.decay_db_per_s >= 0.0,
                "tier {t} tuning times must be non-negative"
            );
            let trigger_period_s = hop as f64 / params.sample_rate as f64;
            let alpha = if tuning.avg_time_s == 0.0 {
                1.0
            } else {
                1.0 - (-(trigger_period_s / tuning.avg_time_s as f64)).exp() as f32
            };
            let decay_per_trigger =
                (-(LN_10 * tuning.decay_db_per_s as f64 * trigger_period_s / 10.0)).exp() as f32;
            let hann: Vec<f32> = (0..n)
                .map(|j| (0.5 - 0.5 * (std::f64::consts::TAU * j as f64 / n as f64).cos()) as f32)
                .collect();
            // Mapping: FFT bins k = 1..=nc (k=0 is DC: not audio, and four tiers
            // clamping it to bin 0 would quadruple-count it), skipping frequencies
            // outside the axis.
            let mut map: Vec<MapEntry> = Vec::with_capacity(nc);
            let mut lo = usize::MAX;
            let mut hi = 0usize;
            for k in 1..=nc {
                let f = bin_freq(k, n, params.sample_rate) as f64;
                if !(f_lo..=AXIS_F_HI as f64).contains(&f) {
                    continue;
                }
                let p = log_position(f, params.bins).clamp(0.0, (params.bins - 1) as f64);
                let i0 = p as usize;
                let frac = (p - i0 as f64) as f32;
                if i0 + 1 < params.bins && frac > 0.0 {
                    map.push(MapEntry {
                        k: k as u16,
                        i0: i0 as u16,
                        w0: 1.0 - frac,
                        i1: (i0 + 1) as u16,
                        w1: frac,
                    });
                } else {
                    map.push(MapEntry {
                        k: k as u16,
                        i0: i0 as u16,
                        w0: 1.0,
                        i1: i0 as u16,
                        w1: 0.0,
                    });
                }
                lo = lo.min(i0);
                hi = hi.max((i0 + 1).min(params.bins - 1));
                // The map is sparse but k must fit the u16 the entry stores.
                debug_assert!(k <= u16::MAX as usize);
            }
            let norm = (4.0 / n as f64).powi(2) as f32;
            Tier {
                plan: RealFftPlan::new(n),
                ring: vec![0.0; 2 * n],
                hann,
                map,
                pos: 0,
                next_due: n as u64,
                sm: vec![0.0; params.bins],
                hold: vec![0.0; params.bins],
                alpha,
                decay_per_trigger,
                norm,
                lo: lo.min(hi),
                hi,
            }
        });
        Self {
            params: *params,
            tiers,
            written: 0,
            win: vec![0.0; max_n],
            bins: vec![Complex::ZERO; max_n / 2 + 1],
            acc: vec![0.0; params.bins],
            levels: vec![0.0; params.bins],
            db: vec![params.floor_db; params.bins],
        }
    }

    /// The setup parameters, reachable for the M2 masking meter and the UI.
    pub fn params(&self) -> &BankParams {
        &self.params
    }

    /// Shared log-bin count.
    pub fn bins(&self) -> usize {
        self.params.bins
    }

    /// Center frequency of shared log bin `i`: the invariant-3 helper for the
    /// *log* axis (the FFT-bin mapping stays [`crate::bin_freq`]). A `powf`, for
    /// display callers — never in the hot loop.
    pub fn log_bin_freq(&self, i: usize) -> f32 {
        let step = (AXIS_F_HI as f64 / AXIS_F_LO as f64).ln() / (self.params.bins - 1) as f64;
        (AXIS_F_LO as f64 * (i as f64 * step).exp()) as f32
    }

    /// Feed `frames` of one channel. Allocation-free, lock-free,
    /// transcendental-free; refreshes each due tier **once** against the newest
    /// full frame (a block covering several hops shows the latest spectrum:
    /// analyzer semantics plus a hard per-call cost ceiling).
    ///
    /// A block longer than the largest ring (16384 frames) is legal; the rings
    /// simply keep their tail.
    pub fn process(&mut self, frames: &[f32]) {
        if frames.is_empty() {
            return;
        }
        self.written += frames.len() as u64;
        for ti in 0..4 {
            let due_now = self.written >= self.tiers[ti].next_due;
            {
                // Ring writes: integer cursor, mirror into the duplicated half.
                let tier = &mut self.tiers[ti];
                let n = tier.ring.len() / 2;
                let mut pos = tier.pos;
                for &s in frames {
                    tier.ring[pos] = s;
                    tier.ring[pos + n] = s;
                    pos += 1;
                    if pos == n {
                        pos = 0;
                    }
                }
                tier.pos = pos;
                if due_now {
                    // Skip missed hops arithmetically (at most one refresh per
                    // call). `pos` is the oldest sample of the last-N frame for
                    // any trigger phase, because hop divides N.
                    let overdue = self.written - tier.next_due;
                    tier.next_due += (overdue / (n as u64 / 4) + 1) * (n as u64 / 4);
                }
            }
            if due_now {
                self.refresh(ti);
            }
        }
    }

    /// Linear normalized power per shared bin (full-scale sine = 1.0): the
    /// audio-thread product. Zero allocation.
    pub fn levels(&self) -> &[f32] {
        &self.levels
    }

    /// Fill the dB view from `levels` — `log10` work, so this belongs to the
    /// **display thread**, never the callback (see the module header).
    pub fn render_db(&mut self) {
        let floor = self.params.floor_db;
        for (slot, &p) in self.db.iter_mut().zip(self.levels.iter()) {
            *slot = if p > 0.0 {
                (10.0 * p.log10()).max(floor)
            } else {
                floor
            };
        }
    }

    /// The shared dB buffer, as of the last `render_db`.
    pub fn db(&self) -> &[f32] {
        &self.db
    }

    /// Clear every accumulator (rings, cursors, schedules, smoothing state)
    /// without reallocating — the `Node::reset` contract of the core crate,
    /// ready for the M1 plugin wrapper.
    pub fn reset(&mut self) {
        self.written = 0;
        for t in self.tiers.iter_mut() {
            t.pos = 0;
            t.next_due = t.ring.len() as u64 / 2;
            t.ring.fill(0.0);
            t.sm.fill(0.0);
            t.hold.fill(0.0);
        }
        self.acc.fill(0.0);
        self.levels.fill(0.0);
        self.db.fill(self.params.floor_db);
    }

    /// The one FFT + spread + smooth sequence of tier `ti` against its newest
    /// full frame. The phase trick of this method is the borrow structure:
    /// `tiers[ti]`, `win`, `bins`, `acc`, `levels` are disjoint fields, so local
    /// bindings split the borrow and no clone or temp is needed.
    fn refresh(&mut self, ti: usize) {
        let (n, lo, hi) = {
            let t = &self.tiers[ti];
            (t.ring.len() / 2, t.lo, t.hi)
        };
        // Gather: the last-N frame, windowed — one linear read of the 2N ring.
        {
            let t = &self.tiers[ti];
            let (ring, hann) = (&t.ring, &t.hann);
            let start = t.pos;
            let win = &mut self.win;
            for j in 0..n {
                win[j] = ring[start + j] * hann[j];
            }
        }
        self.tiers[ti]
            .plan
            .process(&self.win[..n], &mut self.bins[..n / 2 + 1]);
        {
            let t = &self.tiers[ti];
            let acc = &mut self.acc;
            let bins = &self.bins;
            if t.hi >= t.lo {
                acc[lo..=hi].fill(0.0);
                for e in &t.map {
                    let p = bins[e.k as usize].norm_sqr();
                    acc[e.i0 as usize] += p * e.w0;
                    acc[e.i1 as usize] += p * e.w1;
                }
            }
        }
        // Smoothing of this tier; the cross-tier max is a second pass so the
        // other tiers are read while nothing is written through them.
        // Justified allow (the real.rs pattern): `i` is the bin — the subject of
        // the formula, indexing parallel states (acc in, sm/hold through), not a
        // walk.
        #[allow(clippy::needless_range_loop)]
        {
            let t = &mut self.tiers[ti];
            let acc = &self.acc;
            let (alpha, decay, norm) = (t.alpha, t.decay_per_trigger, t.norm);
            for i in lo..=hi {
                let v = acc[i] * norm;
                t.sm[i] += alpha * (v - t.sm[i]);
                let h = t.hold[i] * decay;
                t.hold[i] = h.max(t.sm[i]);
            }
        }
        // The cross-tier max: same justified-allow shape, `i` is the bin.
        #[allow(clippy::needless_range_loop)]
        {
            let levels = &mut self.levels;
            let tiers = &self.tiers;
            for i in lo..=hi {
                levels[i] = tiers
                    .iter()
                    .map(|t| t.sm[i].max(t.hold[i]))
                    .fold(0.0f32, f32::max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{expected_tier_bin, sine_frame};

    fn drive(bank: &mut FftBank, freq: f32, frames: usize) -> Vec<f32> {
        let mut buf = vec![0.0f32; 64];
        let mut fed = 0usize;
        let mut all: Vec<f32> = Vec::with_capacity(frames);
        while fed < frames {
            let take = 64.min(frames - fed);
            for (i, slot) in buf[..take].iter_mut().enumerate() {
                *slot = sine_frame(freq, (fed + i) as u64, 48_000.0);
            }
            bank.process(&buf[..take]);
            all.extend_from_slice(&buf[..take]);
            fed += take;
        }
        all
    }

    /// Independent expectation for one shared bin: the cross-tier max of the
    /// per-tier predictions — direct f64 DFT powers through the axis arithmetic
    /// re-derived in `testsupport::expected_tier_bin` (never an FFT, never our
    /// kernel; the max is the bank's rule, re-derived here as the rule, not as
    /// an implementation).
    fn expected_bin(stream: &[f32], i: usize, bins: usize) -> f64 {
        TIER_SIZES
            .iter()
            .map(|&n| expected_tier_bin(stream, n, 48_000.0, i, bins))
            .fold(0.0f64, f64::max)
    }

    /// Bins touched by a tier's *edge* FFT bins (k ≤ 2 or k ≥ nc−2) are exempt
    /// from the tight equality. A real signal's bin `k` and its negative-frequency
    /// mirror `n−k` fold into each other when they come within ~2 bins, and the
    /// fold is phase-dependent — it rotates with every hop — so the
    /// max-of-history hold legitimately sits above any single frame. Measured
    /// during development (per-tier dump): interior bins match the prediction to
    /// under 0.1%, edge bins read up to 1.13×. The bracket: never below the
    /// current frame, never more than 20% above it.
    fn edge_touched(bank: &FftBank, j: usize) -> bool {
        bank.tiers.iter().any(|t| {
            let nc = (t.ring.len() / 4) as u16;
            t.map.iter().any(|e| {
                (e.k <= 2 || e.k + 2 >= nc) && ((e.i0 as usize) == j || (e.i1 as usize) == j)
            })
        })
    }

    /// The normative table itself: code must equal the doc, or the doc drifts.
    #[test]
    fn the_tier_table_is_the_doc_table() {
        assert_eq!(TIER_SIZES, [256, 1024, 4096, 16384]);
        assert_eq!(
            TIER_BANDS_HZ,
            [
                (4_000.0, 24_000.0),
                (500.0, 4_000.0),
                (100.0, 500.0),
                (20.0, 100.0)
            ]
        );
    }

    /// Invariant 4's "monotone": the log axis strictly ascends and spans exactly
    /// [20, 24000] Hz.
    #[test]
    fn log_axis_is_monotone_and_exact_at_the_ends() {
        let bank = FftBank::new(&BankParams::new(48_000.0));
        assert!((bank.log_bin_freq(0) - AXIS_F_LO).abs() <= 1e-3, "bottom");
        assert!(
            (bank.log_bin_freq(bank.bins() - 1) - AXIS_F_HI).abs() <= 0.5,
            "top: {}",
            bank.log_bin_freq(bank.bins() - 1)
        );
        for i in 1..bank.bins() {
            assert!(
                bank.log_bin_freq(i) > bank.log_bin_freq(i - 1),
                "axis not monotone at bin {i}"
            );
        }
    }

    /// Flat/no-window sanity at bank level: a pure-DC stream can only light the
    /// two bins each mapped k=1 spreads into — the Hann window's own spectrum is
    /// three exact deltas (0.5N at DC, −0.25N at ±1, zero beyond), so a DC
    /// offset reads its one-bin skirt — up to full power 1.0 there, times the
    /// spread weight — and numerical zero anywhere else. The skipped k=0 never
    /// stacks anything at the display floor, and no bin outside the skirt moves.
    #[test]
    fn a_pure_dc_stream_only_touches_the_k1_skirt() {
        let mut bank = FftBank::new(&BankParams::bypass(48_000.0));
        let dc = [1.0f32; 64];
        for _ in 0..320 {
            bank.process(&dc);
        }
        let mut lit: Vec<usize> = Vec::new();
        for t in &bank.tiers {
            for e in &t.map {
                if e.k == 1 {
                    lit.push(e.i0 as usize);
                    lit.push(e.i1 as usize);
                }
            }
        }
        assert!(
            !lit.contains(&0),
            "a mapped k=1 touches the floor bin: {lit:?}"
        );
        for (i, &v) in bank.levels().iter().enumerate() {
            if lit.contains(&i) {
                assert!(
                    v <= 1.0001,
                    "DC skirt bin {i}: {v} exceeds the one-bin Hann skirt theory 1.0·w"
                );
            } else {
                assert!(v < 1e-3, "DC lit bin {i} outside the k=1 skirt: {v}");
            }
        }
    }

    /// Junction continuity, the spec's last M1 sentence: at 100/500/4k/24k a known
    /// sine must light the straddling bins *exactly as the independent prediction
    /// says* (direct-DFT powers through the re-derived mapping), edge-fold bins
    /// bracketed per [`edge_touched`]. If a tier boundary jumps in dB, the
    /// straddling bins disagree here.
    #[test]
    fn every_junction_is_continuous_on_a_known_sine() {
        for &fj in &[100.0f32, 500.0, 4_000.0, 23_990.0] {
            let mut bank = FftBank::new(&BankParams::bypass(48_000.0));
            let frames = 20_480; // multiple of every hop: all tiers fresh
            let stream = drive(&mut bank, fj, frames);
            let lv = bank.levels();
            let i = log_position(fj as f64, bank.bins()).round() as usize;
            let mut reported = 0.0f64;
            let (j_lo, j_hi) = (i.saturating_sub(1), (i + 1).min(bank.bins() - 1));
            for (k, &v) in lv[j_lo..=j_hi].iter().enumerate() {
                let j = j_lo + k;
                let want = expected_bin(&stream, j, bank.bins());
                let got = v as f64;
                if edge_touched(&bank, j) {
                    assert!(
                        (want * 0.99..=want * 1.20).contains(&got),
                        "junction {fj} Hz edge bin {j}: bank {got:.6} outside the mirror-fold \
                         bracket [{:.6}, {:.6}] around the frame prediction {want:.6}",
                        want * 0.99,
                        want * 1.20
                    );
                } else {
                    let rel = if want > 1e-12 {
                        (got / want - 1.0).abs()
                    } else {
                        got.abs()
                    };
                    assert!(
                        rel <= 0.02,
                        "junction {fj} Hz bin {j}: bank {got:.6}, independent prediction \
                         {want:.6} (rel err {rel:.4})"
                    );
                }
                reported = reported.max(want);
            }
            assert!(
                reported > 0.3,
                "junction {fj}: known sine must light the bins"
            );
        }
    }

    /// Per-tier energy placement: a mid-band sine must light the bins around its
    /// frequency *exactly as the independent prediction says* (window of
    /// ±12 log bins — the coarsest tier's own resolution can shift the argmax
    /// below the tone's bin, so value-matching replaces an argmax guess), and
    /// nothing 40 log bins away. (The FFT underneath is pinned against rustfft in
    /// `plan.rs`/`real.rs`; what is *bank* is placement + normalization.)
    #[test]
    fn each_tier_lights_its_own_band() {
        for &f in &[10_000.0f32, 1_500.0, 250.0, 50.0] {
            let mut bank = FftBank::new(&BankParams::bypass(48_000.0));
            let frames = 20_480;
            let stream = drive(&mut bank, f, frames);
            let lv = bank.levels();
            let i = log_position(f as f64, bank.bins()).round() as usize;
            let (w_lo, w_hi) = (i.saturating_sub(12), (i + 12).min(bank.bins() - 1));
            let mut window_max = 0.0f32;
            for (k, &v) in lv[w_lo..=w_hi].iter().enumerate() {
                let j = w_lo + k;
                let want = expected_bin(&stream, j, bank.bins());
                let got = v as f64;
                if edge_touched(&bank, j) {
                    assert!(
                        (want * 0.99..=want * 1.20).contains(&got),
                        "sine {f} Hz edge bin {j}: bank {got:.6} outside the mirror-fold \
                         bracket around the prediction {want:.6}"
                    );
                } else {
                    let rel = if want > 1e-12 {
                        (got / want - 1.0).abs()
                    } else {
                        got.abs()
                    };
                    assert!(
                        rel <= 0.02,
                        "sine {f} Hz bin {j}: bank {got:.6}, prediction {want:.6} (rel {rel:.4})"
                    );
                }
                window_max = window_max.max(v);
            }
            assert!(
                window_max > 0.3,
                "sine {f} Hz: window around bin {i} stayed dark ({window_max})"
            );
            let far = if i + 40 < lv.len() {
                lv[i + 40]
            } else {
                lv[i - 40]
            };
            assert!(
                far < window_max * 0.01,
                "sine {f} Hz: leak at +40 bins {far:.6} vs window peak {window_max}"
            );
        }
    }

    /// The mapping conserves power entry-wise: each spread is a two-point
    /// interpolation with `w0 + w1 = 1` landing on the axis. A mis-normalized
    /// entry would silently shrink or inflate the spectrum — this table is the
    /// mapping's own Parseval. Why entry-wise and not a full-spectrum energy
    /// check: the honest anchor for the finest tier is a *direct* 16384-point
    /// DFT (8192 bins × 16384 samples = 134M transcendental ops) — several
    /// seconds in debug — so the structure is tested here at the entry level and
    /// the *placement* is tested externally (and cheaply) by the junction and
    /// band tests.
    #[test]
    fn mapping_weights_conserve_power_entrywise() {
        let bank = FftBank::new(&BankParams::bypass(48_000.0));
        for (t, tier) in bank.tiers.iter().enumerate() {
            assert!(!tier.map.is_empty(), "tier {t} maps nothing");
            let mut prev_k = 0u16;
            for e in &tier.map {
                assert!(e.k > prev_k, "tier {t}: map not ascending in k");
                prev_k = e.k;
                assert!(
                    (e.w0 + e.w1 - 1.0).abs() <= 1e-6,
                    "tier {t} bin {}: weights {} + {} ≠ 1",
                    e.k,
                    e.w0,
                    e.w1
                );
                assert!(
                    e.i1 == e.i0 || e.i1 == e.i0 + 1,
                    "tier {t} bin {}: spread lands {}→{}, must be adjacent",
                    e.k,
                    e.i0,
                    e.i1
                );
                assert!((e.i1 as usize) < bank.bins(), "tier {t} off-axis entry");
            }
            // The touched window must contain the table's extremes.
            let first = *tier.map.first().expect("non-empty checked above");
            let last = *tier.map.last().expect("non-empty checked above");
            assert!(
                first.i0 as usize >= tier.lo,
                "tier {t}: lo above first entry"
            );
            assert!(last.i1 as usize <= tier.hi, "tier {t}: hi below last entry");
        }
    }

    /// Peak-hold decays after the tone stops — qualitative shape with an honest
    /// bracket. The *exact* arithmetic of the derived constants lives in
    /// `smoothing_constants_are_derived_from_the_formulas`; here the tier's newest
    /// frame legitimately still contains 75% of the tone after one 4096-frame
    /// silence hop (frame 16384 ≫ hop 4096), and that partial reading re-raises
    /// the hold — the naive single-hop expectation (1.707 dB) is wrong physics.
    /// Measured with this drive: 0.79 dB for the hop; bracket [0.3, 2.2].
    #[test]
    fn hold_decays_after_the_signal_stops() {
        let mut params = BankParams::new(48_000.0);
        // Tier 2 and 3 bypassed except for a shared 20 dB/s, so the cross-tier
        // max tracks one clean rate.
        params.tuning[2] = TierTuning {
            avg_time_s: 0.0,
            decay_db_per_s: 20.0,
        };
        params.tuning[3] = TierTuning {
            avg_time_s: 0.0,
            decay_db_per_s: 20.0,
        };
        let mut bank = FftBank::new(&params);
        drive(&mut bank, 50.0, 20_480);
        let i = log_position(50.0, bank.bins()).round() as usize;
        bank.render_db();
        let before = bank.db()[i];
        assert!(before > -30.0, "tone should light the bin, got {before} dB");
        let silence = [0.0f32; 64];
        for _ in 0..64 {
            bank.process(&silence); // exactly +4096 frames: one hop for tier 3
        }
        bank.render_db();
        let after = bank.db()[i];
        let drop = before - after;
        assert!(after < before, "hold must fall: {before} → {after} dB");
        assert!(
            (0.3..=2.2).contains(&drop),
            "measured drop {drop:.3} dB outside the honest bracket [0.3, 2.2] \
             (naive one-hop 1.707 dB is wrong: partial frame re-raises the hold)"
        );
    }

    /// The derivation itself: `α = 1 − e^{−T/τ}` and `decay = 10^{−(dB/s)·T/10}`
    /// re-evaluated in f64 in the test against the bank's f32 fields, at each
    /// tier's exact trigger period. This is what "measured, not opined" means for
    /// a smoothing time: the claim is arithmetic the test repeats, not prose.
    #[test]
    fn smoothing_constants_are_derived_from_the_formulas() {
        let mut p = BankParams::new(48_000.0);
        let tunings = [
            (0.010f32, 45.0f32),
            (0.030, 30.0),
            (0.100, 20.0),
            (0.250, 20.0),
        ];
        for (t, (tau, dec)) in tunings.into_iter().enumerate() {
            p.tuning[t] = TierTuning {
                avg_time_s: tau,
                decay_db_per_s: dec,
            };
        }
        let bank = FftBank::new(&p);
        for t in 0..4 {
            let trigger_s = (TIER_SIZES[t] / 4) as f64 / 48_000.0;
            let (tau, dec) = tunings[t];
            let a = 1.0 - (-(trigger_s / tau as f64)).exp();
            let d = (-(LN_10 * dec as f64 * trigger_s / 10.0)).exp();
            let tier = &bank.tiers[t];
            assert!(
                (tier.alpha - a as f32).abs() <= 1e-6,
                "tier {t}: alpha {} vs formula {}",
                tier.alpha,
                a
            );
            assert!(
                (tier.decay_per_trigger - d as f32).abs() <= 1e-6,
                "tier {t}: decay {} vs formula {}",
                tier.decay_per_trigger,
                d
            );
        }
    }

    /// The dB view: silence sits at the floor; a full-scale sine reads close to
    /// 0 dBFS (within the two-point spread's worst dilution: a half-bin-centered
    /// tone splits 50/50, −3 dB).
    #[test]
    fn render_db_maps_levels_and_floor() {
        let mut bank = FftBank::new(&BankParams::bypass(48_000.0));
        bank.render_db();
        assert!(
            bank.db()
                .iter()
                .all(|&d| (d - bank.params().floor_db).abs() <= 1e-6),
            "silence must sit at the floor"
        );
        drive(&mut bank, 1_000.0, 20_480);
        bank.render_db();
        let peak_db = bank.db().iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(
            peak_db > -3.2 && peak_db <= 0.1,
            "full-scale sine should read ≈0 dBFS (spread down to −3), got {peak_db}"
        );
    }

    #[test]
    #[should_panic(expected = "sample rate")]
    fn new_rejects_an_absurd_rate() {
        FftBank::new(&BankParams::new(8_000.0));
    }

    #[test]
    #[should_panic(expected = "bin count")]
    fn new_rejects_too_many_shared_bins() {
        let mut p = BankParams::new(48_000.0);
        p.bins = 70_000;
        FftBank::new(&p);
    }

    /// Invariant 4 in its mechanical form, correctness half: a reset clears the
    /// state and the same instance then re-derives the same reading — no stale
    /// accumulator haunting the second cycle (allocation half: the strict guard
    /// test in tests/alloc_contract.rs).
    #[test]
    fn reset_clears_state_and_the_second_cycle_is_identical() {
        let mut bank = FftBank::new(&BankParams::bypass(48_000.0));
        drive(&mut bank, 300.0, 20_480);
        let first = bank.levels()[log_position(300.0, bank.bins()).round() as usize];
        assert!(first > 0.1, "tone before reset");
        bank.reset();
        assert!(bank.levels().iter().all(|&v| v == 0.0), "reset must clear");
        drive(&mut bank, 300.0, 20_480);
        let second = bank.levels()[log_position(300.0, bank.bins()).round() as usize];
        assert!(
            (second - first).abs() <= 1e-4,
            "identical cycles after reset: {first} vs {second}"
        );
    }
}
