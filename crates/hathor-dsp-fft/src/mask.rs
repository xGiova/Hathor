//! The masking meter: SPAN-style A↔B diff *with* a first-class hearing approximation,
//! layered on [`FftBank`](crate::FftBank)'s shared log axis (M2, headless).
//!
//! Roadmap context: the analyzer's A/B tool asks "what of the difference between two
//! sounds is actually audible?". Raw A−B per bin answers the first half; this module
//! answers the second — the fraction of each spectrum that the *other* spectrum
//! buries. Two banks feed two level vectors; the meter never owns a bank, which is
//! what keeps this layer additive over the frozen M1 API.
//!
//! ## The model — chosen, documented, and *not* a psychoacoustic standard
//!
//! This is a deliberately small, testable math layer: asymmetric exponential skirts
//! over log bins with power-add masking. It is **not** ISO 532, not a Moore-Glasgow
//! auditory filter, not a fitted masking surface — it is a first-class approximation
//! whose acceptance is *agreement with listening in M3*. Until that pass happens the
//! defaults below are starting points, not claims. What the code does promise: the
//! formula here is the contract, it is implemented exactly, and the tests
//! re-derive it independently in f64 (`testsupport::mask_threshold_oracle` —
//! never the same helper twice).
//!
//! All spectra are **linear power in [`FftBank::levels`](crate::FftBank::levels)
//! units** (the `(4/N)²` normalization: full-scale sine reads 1.0). For a spectrum
//! `X` over the shared axis of `bins` log bins, the masked threshold is:
//!
//! ```text
//! T(k) = AT + Σ_{d=1..D} [ X(k−d)·W↑(d) + X(k+d)·W↓(d) ]      (out-of-axis terms skipped)
//! AT   = 10^(floor_db/10)                        — absolute threshold: the bank's
//!                                                   floor, flat (see caveat below)
//! W↑(d) = weight · exp(−d/spread_up_bins)        — a masker d bins BELOW k raises k
//! W↓(d) = weight · exp(−d/spread_down_bins)      — a masker d bins ABOVE k raises k
//! D     = min(bins − 1, ceil(max(spread_up_bins, spread_down_bins) · ln(1e5)))
//! ```
//!
//! The choices, each with a reason:
//!
//! * **No self-masking.** `X(k)` never contributes to `T(k)`: a tone masks its
//!   *neighbors*, not its own bin — with `d ≥ 1` only, a lone tone leaves its own
//!   threshold at the floor (test: `a_bin_never_masks_itself`).
//! * **Asymmetric skirts.** Masking reaches upward (low masker → high bin) about
//!   twice as far as downward; `spread_up_bins > spread_down_bins` encodes it. The
//!   distance is in log bins — on the bank's axis ≈ 100 bins/octave, so 15 bins ≈
//!   −43 dB one octave up (exponential), a coarse stand-in for critical-band width.
//! * **Power-add.** Neighboring maskers *sum in linear power* (masking energy is
//!   summed, not maxed): simple, commutative, and the source of the "power-add"
//!   label in the M2 contract.
//! * **Truncation.** The exponential tail is dropped where it falls below 1e-5 of a
//!   masker's power: the geometric tail past `D` contributes < 1.6e-4 relative in
//!   the worst (widest) direction — far below the f32 accumulation noise of the
//!   sum itself. `D` is decided at setup and exposed as [`skirt_radius`](MaskingMeter::skirt_radius).
//! * **Absolute threshold caveat.** Real hearing thresholds curve up at both axis
//!   ends; a flat `AT` from the bank floor (−120 dBFS) is honest for a *relative*
//!   A/B tool (the floor cancels in comparisons near it) and wrong for absolute
//!   audibility at very low levels — recorded here so nobody "discovers" it in M3.
//!
//! Derived spectra, per bin, with A masking B and vice versa:
//!
//! ```text
//! hidden_A(k) = min(X_A(k), T_B(k))   — the energy of A buried under B's threshold
//! hidden_B(k) = min(X_B(k), T_A(k))
//! difference(k) = 10·log10(X_A(k)) − 10·log10(X_B(k))   (dB, floor-clamped) — the
//!                 SPAN-style A↔B diff, before the masking view decides audibility.
//! ```
//!
//! ## Hot-path doctrine (same as the bank, invariants unchanged)
//!
//! [`MaskingMeter::new`] is the only allocation site; [`update`](MaskingMeter::update)
//! is pure indexed mul/add (the skirt tables are built once at setup, in f64,
//! narrowed once) and allocates nothing under `AudioSection` STRICT — the guard test
//! in `tests/alloc_contract.rs` proves it. The `log10` the dB views need lives in
//! [`render_db`](MaskingMeter::render_db), a display-thread call, exactly like the
//! bank's. Parameters are fixed at setup ([`MaskingParams`], same doctrine as
//! [`BankParams`](crate::BankParams)); the meter re-derives nothing per frame.

use crate::bank::DEFAULT_BINS;

/// Spread-table truncation: drop the exponential skirt below `1e-5` of a masker.
/// The radius is `ceil(max_width · MASK_TRUNC)`; the constant is `ln(1e5)` spelled
/// as a literal — float methods are not `const` at our MSRV, and the oracle test
/// re-derives the radius from `100_000.0_f64.ln()` and asserts it matches, so the
/// two spellings cannot drift silently.
const MASK_TRUNC: f64 = 11.512_925_464_970_229;

/// Setup-fixed parameters of [`MaskingMeter`]. Distances are in **log bins of the
/// bank's shared axis** (≈ 100 bins/octave), never Hz: the meter is sample-rate
/// agnostic by construction, and two banks at different rates over the same axis
/// compare cleanly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaskingParams {
    /// Shared-axis length; must equal the banks' `bins()` for levels to line up.
    pub bins: usize,
    /// Absolute threshold, dB — the flat floor standing in for the hearing
    /// threshold curve (caveat documented in the module header). Default matches
    /// the bank's floor.
    pub floor_db: f32,
    /// Skirt width upward (maskers *below* a bin), in bins. 0 disables that side.
    pub spread_up_bins: f32,
    /// Skirt width downward (maskers *above* a bin), in bins. 0 disables that side.
    pub spread_down_bins: f32,
    /// Gain applied to every masker's power before it hits a neighbor's threshold.
    /// 1.0 is "the other spectrum masks at its own level"; M3's listening pass may
    /// want more or less — it is a knob on purpose, not a constant in disguise.
    pub weight: f32,
}

impl Default for MaskingParams {
    fn default() -> Self {
        Self::new()
    }
}

impl MaskingParams {
    /// The defaults: shared axis at [`DEFAULT_BINS`], the bank's floor, and skirt
    /// widths 15/7 bins (≈ −43 dB one octave up, faster down). **Starting points
    /// for the M3 listening pass — NOT psychoacoustically fitted** (the honest
    /// label the module doc promises).
    #[must_use]
    pub fn new() -> Self {
        Self {
            bins: DEFAULT_BINS,
            floor_db: -120.0,
            spread_up_bins: 15.0,
            spread_down_bins: 7.0,
            weight: 1.0,
        }
    }
}

/// The A↔B masking meter: two linear-power spectra in, five read-only dB views out.
///
/// Constructed once ([`new`](MaskingMeter::new) is the only allocation site), then
/// [`update`](MaskingMeter::update) per recomputation and [`render_db`](MaskingMeter::render_db)
/// per display tick. Total memory at the default axis: twelve `bins`-long `f32`
/// buffers plus two skirt tables ≈ 48 KB — sized at setup, never grown (invariant 4
/// of the module doc, inherited here).
pub struct MaskingMeter {
    p: MaskingParams,
    at_pow: f32,
    /// `w_up[d-1] = weight · exp(-d / spread_up_bins)` — empty when the width is 0.
    w_up: Vec<f32>,
    w_down: Vec<f32>,
    // Linear state (written by `update`): input copies first, so `render_db` needs
    // no caller data.
    xa: Vec<f32>,
    xb: Vec<f32>,
    ta: Vec<f32>,
    tb: Vec<f32>,
    ha: Vec<f32>,
    hb: Vec<f32>,
    // dB views (written by `render_db`, read by the UI thread through `&self`).
    mask_a_db: Vec<f32>,
    mask_b_db: Vec<f32>,
    hidden_a_db: Vec<f32>,
    hidden_b_db: Vec<f32>,
    diff_db: Vec<f32>,
}

impl MaskingMeter {
    /// Builds the two skirt tables and every buffer. **The only allocation site of
    /// the type**; guarded like [`FftBank::new`](crate::FftBank::new) in
    /// `tests/alloc_contract.rs`.
    ///
    /// # Panics
    /// If `bins < 2`, a spread width or weight is negative, or `floor_db > 0.0`
    /// (an absolute threshold above full scale is a configuration error).
    #[must_use]
    pub fn new(params: &MaskingParams) -> Self {
        assert!(params.bins >= 2, "MaskingMeter needs at least 2 bins");
        assert!(
            params.spread_up_bins >= 0.0 && params.spread_down_bins >= 0.0,
            "spread widths must be non-negative (0 disables that skirt side)"
        );
        assert!(
            params.weight >= 0.0,
            "masking weight must be non-negative, got {}",
            params.weight
        );
        assert!(
            params.floor_db <= 0.0,
            "the absolute threshold must sit at or below full scale, got {} dB",
            params.floor_db
        );
        let n = params.bins;
        // Setup transcendentals live here only: the tables in f64, narrowed once.
        let radius = ((params.spread_up_bins.max(params.spread_down_bins) as f64 * MASK_TRUNC)
            .ceil() as usize)
            .min(n - 1);
        let table = |width: f32| -> Vec<f32> {
            if width <= 0.0 {
                return Vec::new();
            }
            (1..=radius)
                .map(|d| (params.weight as f64 * (-(d as f64) / width as f64).exp()) as f32)
                .collect()
        };
        let w_up = table(params.spread_up_bins);
        let w_down = table(params.spread_down_bins);
        let at_pow = (10.0_f64.powf(params.floor_db as f64 / 10.0)) as f32;
        Self {
            at_pow,
            w_up,
            w_down,
            p: *params,
            xa: vec![0.0; n],
            xb: vec![0.0; n],
            ta: vec![0.0; n],
            tb: vec![0.0; n],
            ha: vec![0.0; n],
            hb: vec![0.0; n],
            mask_a_db: vec![params.floor_db; n],
            mask_b_db: vec![params.floor_db; n],
            hidden_a_db: vec![params.floor_db; n],
            hidden_b_db: vec![params.floor_db; n],
            diff_db: vec![0.0; n],
        }
    }

    /// Recompute thresholds and buried fractions from two level spectra (linear
    /// power, bank units). Zero-allocation, no transcendentals, no panic paths:
    /// a view whose length does not match the axis is *ignored* — the previous
    /// state survives and release builds keep running. The bank's guard tests and
    /// `update_ignores_mismatched_views_and_keeps_state` pin both halves.
    pub fn update(&mut self, a: &[f32], b: &[f32]) {
        if a.len() != self.p.bins || b.len() != self.p.bins {
            return;
        }
        // Destructure to disjoint fields: the borrow checker is the architecture
        // note here — no clone, no temp, no method-call re-borrow of `self`.
        let Self {
            at_pow,
            w_up,
            w_down,
            xa,
            xb,
            ta,
            tb,
            ha,
            hb,
            ..
        } = self;
        xa.copy_from_slice(a);
        xb.copy_from_slice(b);
        accumulate(xa, w_up, w_down, *at_pow, ta);
        accumulate(xb, w_up, w_down, *at_pow, tb);
        // Buried fraction: min of the opponent's own power and *this* side's
        // threshold — hidden_A(k) = min(X_A(k), T_B(k)).
        for ((h, x), t) in ha.iter_mut().zip(xa.iter()).zip(tb.iter()) {
            *h = (*x).min(*t);
        }
        for ((h, x), t) in hb.iter_mut().zip(xb.iter()).zip(ta.iter()) {
            *h = (*x).min(*t);
        }
    }

    /// Fill the five dB views from the linear state — display-thread call, the
    /// `log10`s live here and nowhere in `update` (same split as the bank's
    /// `levels()`/`render_db()`).
    pub fn render_db(&mut self) {
        let floor = self.p.floor_db;
        let Self {
            xa,
            xb,
            ta,
            tb,
            ha,
            hb,
            mask_a_db,
            mask_b_db,
            hidden_a_db,
            hidden_b_db,
            diff_db,
            ..
        } = self;
        for (m, d) in ta.iter().zip(mask_a_db.iter_mut()) {
            *d = to_db(*m, floor);
        }
        for (m, d) in tb.iter().zip(mask_b_db.iter_mut()) {
            *d = to_db(*m, floor);
        }
        for (h, d) in ha.iter().zip(hidden_a_db.iter_mut()) {
            *d = to_db(*h, floor);
        }
        for (h, d) in hb.iter().zip(hidden_b_db.iter_mut()) {
            *d = to_db(*h, floor);
        }
        for ((d, x), y) in diff_db.iter_mut().zip(xa.iter()).zip(xb.iter()) {
            *d = to_db(*x, floor) - to_db(*y, floor);
        }
    }

    /// The masked threshold created by signal A, dB, as of the last `render_db`.
    #[must_use]
    pub fn mask_a(&self) -> &[f32] {
        &self.mask_a_db
    }

    /// The masked threshold created by signal B, dB.
    #[must_use]
    pub fn mask_b(&self) -> &[f32] {
        &self.mask_b_db
    }

    /// The fraction of A's energy buried under B's threshold, dB.
    #[must_use]
    pub fn hidden_a(&self) -> &[f32] {
        &self.hidden_a_db
    }

    /// The fraction of B's energy buried under A's threshold, dB.
    #[must_use]
    pub fn hidden_b(&self) -> &[f32] {
        &self.hidden_b_db
    }

    /// The SPAN-style A↔B difference, dB per bin (A − B), as of the last
    /// `render_db`. Floor-clamped sides give ±(the floor span) where one spectrum
    /// is silent — a silent bin reads the floor, so a lone tone diffs against it.
    #[must_use]
    pub fn difference(&self) -> &[f32] {
        &self.diff_db
    }

    /// The skirt radius `D` in bins (the module doc's formula, decided at setup).
    #[must_use]
    pub fn skirt_radius(&self) -> usize {
        self.w_up.len().max(self.w_down.len())
    }

    /// The setup parameters this meter was built with.
    #[must_use]
    pub fn params(&self) -> &MaskingParams {
        &self.p
    }

    /// Axis length this meter is wired to.
    #[must_use]
    pub fn bins(&self) -> usize {
        self.p.bins
    }

    /// Clear every buffer to the as-new state (no history — the meter is a pure
    /// function of the last `update`, but the *views* hold that last answer and
    /// `reset` takes it back to "nothing said yet").
    pub fn reset(&mut self) {
        for v in [&mut self.xa, &mut self.xb, &mut self.ta, &mut self.tb] {
            v.fill(0.0);
        }
        for v in [&mut self.ha, &mut self.hb] {
            v.fill(0.0);
        }
        self.mask_a_db.fill(self.p.floor_db);
        self.mask_b_db.fill(self.p.floor_db);
        self.hidden_a_db.fill(self.p.floor_db);
        self.hidden_b_db.fill(self.p.floor_db);
        self.diff_db.fill(0.0);
    }
}

/// `T(k)` of the module doc: `fill(AT)` then one pass of table-spread adds.
/// Pure mul/add/index — this is the hot loop and it contains no transcendental,
/// no allocation, and no index that can go out of bounds (the distance loops
/// `break` at the axis edge). `pub(crate)` only so the oracle's doc link resolves.
pub(crate) fn accumulate(x: &[f32], w_up: &[f32], w_down: &[f32], at: f32, t: &mut [f32]) {
    t.fill(at);
    let n = x.len();
    for (j, &pv) in x.iter().enumerate() {
        if pv <= 0.0 {
            continue;
        }
        // Masker at j raises every bin above it (upward skirt) ...
        for (d0, &w) in w_up.iter().enumerate() {
            let i = j + 1 + d0;
            if i >= n {
                break;
            }
            t[i] += pv * w;
        }
        // ... and every bin below it (downward skirt).
        for (d0, &w) in w_down.iter().enumerate() {
            let i = match j.checked_sub(1 + d0) {
                Some(i) => i,
                None => break,
            };
            t[i] += pv * w;
        }
    }
}

/// The single dB conversion of the meter: floor-clamped like the bank's
/// `render_db` (a zero reads the floor, never −∞).
fn to_db(power: f32, floor: f32) -> f32 {
    if power > 0.0 {
        (10.0 * power.log10()).max(floor)
    } else {
        floor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{mask_threshold_oracle, MaskingOracleParams};

    /// dB views of the floor drift by f32 `powf`→`log10` round-trip noise
    /// (≈1e-6 dB around −120); comparisons allow it rather than pretend.
    fn near_floor(v: f32, floor: f32) -> bool {
        (v - floor).abs() <= 1e-3
    }

    /// The formula itself, against an **independent** f64 implementation written
    /// from the module doc (direct `exp()` per term, never the meter's tables —
    /// house law of `testsupport`). Dense deterministic spectra exercise every
    /// skirt tap twice.
    #[test]
    fn the_truncated_formula_matches_the_independent_f64_oracle() {
        let p = MaskingParams::new();
        let mut meter = MaskingMeter::new(&p);
        // Deterministic dense pattern (CONVENTION §5: no RNG in numeric work):
        // each bin a different power, no zeros so the whole table is summed.
        let a: Vec<f32> = (0..p.bins)
            .map(|k| {
                let m = (k % 97) as f32 + 1.0;
                (m * m * 1e-4).min(1.0)
            })
            .collect();
        let b: Vec<f32> = (0..p.bins).map(|k| (k % 53) as f32 * 1e-3 + 3e-4).collect();
        meter.update(&a, &b);
        meter.render_db();
        // The radius recomputed from the doc's formula, independently:
        let d_max = ((p.spread_up_bins.max(p.spread_down_bins) as f64 * 100_000.0_f64.ln()).ceil()
            as usize)
            .min(p.bins - 1);
        assert_eq!(
            meter.skirt_radius(),
            d_max,
            "radius must be the doc formula"
        );
        let at = 10f64.powf(p.floor_db as f64 / 10.0);
        let opa = MaskingOracleParams {
            up: p.spread_up_bins,
            down: p.spread_down_bins,
            weight: p.weight,
            at,
            d_max,
        };
        let ta = mask_threshold_oracle(&a, &opa);
        let tb = mask_threshold_oracle(&b, &opa);
        let floor = p.floor_db as f64;
        for k in 0..p.bins {
            let want_a = (10.0 * ta[k].log10()).max(floor);
            let want_b = (10.0 * tb[k].log10()).max(floor);
            assert!(
                (want_a - meter.mask_a()[k] as f64).abs() <= 0.01,
                "mask_a bin {k}: oracle {want_a:.4} dB, meter {}",
                meter.mask_a()[k]
            );
            assert!(
                (want_b - meter.mask_b()[k] as f64).abs() <= 0.01,
                "mask_b bin {k}: oracle {want_b:.4} dB, meter {}",
                meter.mask_b()[k]
            );
            // Buried fraction: min(x, T_other), same independent path.
            let ha = (10.0 * (a[k] as f64).min(tb[k]).log10()).max(floor);
            let hb = (10.0 * (b[k] as f64).min(ta[k]).log10()).max(floor);
            assert!(
                (ha - meter.hidden_a()[k] as f64).abs() <= 0.01,
                "hidden_a bin {k}: oracle {ha:.4}, meter {}",
                meter.hidden_a()[k]
            );
            assert!(
                (hb - meter.hidden_b()[k] as f64).abs() <= 0.01,
                "hidden_b bin {k}: oracle {hb:.4}, meter {}",
                meter.hidden_b()[k]
            );
        }
    }

    /// Silence vs silence: everything is the floor, the difference is zero.
    #[test]
    fn silence_against_silence_reads_the_floor_everywhere() {
        let mut meter = MaskingMeter::new(&MaskingParams::new());
        let z = vec![0.0f32; meter.bins()];
        meter.update(&z, &z);
        meter.render_db();
        let floor = meter.params().floor_db;
        assert!(meter.mask_a().iter().all(|&v| near_floor(v, floor)));
        assert!(meter.mask_b().iter().all(|&v| near_floor(v, floor)));
        assert!(meter.hidden_a().iter().all(|&v| v == floor));
        assert!(meter.hidden_b().iter().all(|&v| v == floor));
        assert!(meter.difference().iter().all(|&v| v == 0.0));
    }

    /// Silence vs a tone, **both directions**: a lone tone spreads its threshold
    /// over the neighbors but never hides a silent opponent, and never touches its
    /// own bin.
    #[test]
    fn a_lone_tone_masks_the_neighborhood_in_both_directions() {
        for (tone_is_a, j) in [(true, 500usize), (false, 300usize)] {
            let mut meter = MaskingMeter::new(&MaskingParams::new());
            let n = meter.bins();
            let mut x = vec![0.0f32; n];
            x[j] = 1.0; // full-scale sine power, bank units
            let z = vec![0.0f32; n];
            let (a, b) = if tone_is_a { (&x, &z) } else { (&z, &x) };
            meter.update(a, b);
            meter.render_db();
            let floor = meter.params().floor_db;
            // The tone's own threshold stays at the floor: no self-masking.
            let mask = if tone_is_a {
                meter.mask_a()
            } else {
                meter.mask_b()
            };
            assert!(
                near_floor(mask[j], floor),
                "lone tone must not mask its own bin: {} dB",
                mask[j]
            );
            // One bin up: the exp(-1/15) skirt, from the doc formula, not the table.
            let want = 10f64 * (10f64.powf(floor as f64 / 10.0) + (-(1f64 / 15.0)).exp()).log10();
            let got = mask[j + 1] as f64;
            assert!(
                (got - want.max(floor as f64)).abs() <= 0.02,
                "skirt one bin up: want {want:.4} dB, got {got:.4}"
            );
            // The silent side is buried by nothing: hidden(silence) == floor.
            let hidden_silent = if tone_is_a {
                meter.hidden_b()
            } else {
                meter.hidden_a()
            };
            assert!(hidden_silent.iter().all(|&v| v == floor));
            // The tone itself is hidden only down to the opponent's floor.
            let hidden_tone = if tone_is_a {
                meter.hidden_a()
            } else {
                meter.hidden_b()
            };
            assert!(
                (hidden_tone[j] - floor).abs() <= 1e-3,
                "a lone tone is not hidden: got {}",
                hidden_tone[j]
            );
            // Difference at the tone bin reads the full floor span, sign by side.
            let d = meter.difference()[j];
            assert!(
                (d.abs() - (0.0 - floor)).abs() <= 1e-3,
                "tone vs silence difference: {d}"
            );
        }
    }

    /// The asymmetry is real: at equal bin distance an upward skirt (masker below)
    /// reads wider than a downward one, exactly by the exp ratio of the doc.
    #[test]
    fn upward_masking_reaches_farther_than_downward() {
        let mut meter = MaskingMeter::new(&MaskingParams::new());
        let n = meter.bins();
        let mut x = vec![0.0f32; n];
        x[500] = 1.0;
        let z = vec![0.0f32; n];
        meter.update(&x, &z);
        // Linear thresholds above the floor, 20 bins either side of the masker:
        let up20 = (meter.ta[520] - meter.at_pow) as f64;
        let down20 = (meter.ta[480] - meter.at_pow) as f64;
        let ratio = (-(20f64 / 15.0)).exp() / (-(20f64 / 7.0)).exp();
        assert!(
            up20 > 4.0 * down20,
            "up 20 bins {up20} vs down 20 bins {down20}: theory ratio {ratio:.3}, \
             claimed asymmetry not present"
        );
    }

    /// Same test the doc promises: a masker never raises its own bin's threshold,
    /// however dense the spectrum around it.
    #[test]
    fn a_bin_never_masks_itself() {
        let p = MaskingParams::new();
        let mut meter = MaskingMeter::new(&p);
        let all_ones: Vec<f32> = vec![0.5; p.bins];
        meter.update(&all_ones, &vec![0.0; p.bins]);
        // Every bin's threshold = AT + neighbors (never its own 0.5): the oracle
        // proves structure — T(k) < 0.5 for at_pow tiny only when neighbors are
        // excluded... they are NOT excluded (dense), so pin the exact delta:
        meter.render_db();
        let at = 10f64.powf(-12.0);
        let opa = MaskingOracleParams {
            up: p.spread_up_bins,
            down: p.spread_down_bins,
            weight: p.weight,
            at,
            d_max: meter.skirt_radius(),
        };
        let t = mask_threshold_oracle(&all_ones, &opa);
        let mid = p.bins / 2;
        // With self included by mistake, T(mid) would carry 0.5·w(0)=0.5 extra;
        // the oracle has no d=0 term, so equality already pins "no self-masking".
        let got = meter.mask_a()[mid] as f64;
        assert!(
            (got - (10.0 * t[mid].log10()).max(-120.0)).abs() <= 0.01,
            "self term leaked: meter {got}, oracle {} dB",
            10.0 * t[mid].log10()
        );
    }

    /// The `weight` knob scales the *skirt* only — the absolute threshold is not
    /// weighted (it is the listener, not the masker).
    #[test]
    fn weight_scales_the_skirt_and_only_the_skirt() {
        let p1 = MaskingParams::new();
        let p2 = MaskingParams { weight: 2.0, ..p1 };
        let mut m1 = MaskingMeter::new(&p1);
        let mut m2 = MaskingMeter::new(&p2);
        let mut x = vec![0.0f32; p1.bins];
        x[400] = 1.0;
        let z = vec![0.0f32; p1.bins];
        m1.update(&x, &z);
        m2.update(&x, &z);
        // Linear thresholds (private state, same-module tests may reach in):
        for k in 0..p1.bins {
            let one = (m1.ta[k] - m1.at_pow) as f64;
            let two = (m2.ta[k] - m2.at_pow) as f64;
            assert!(
                (two - 2.0 * one).abs() <= 2e-6 * two.max(1.0),
                "bin {k}: 2·T1 {} != T2-AT {two}",
                2.0 * one
            );
        }
    }

    /// Zero widths degenerate honestly: no skirt at all, the meter is the flat
    /// floor everywhere whatever screams.
    #[test]
    fn zero_width_means_no_spread() {
        let p = MaskingParams {
            spread_up_bins: 0.0,
            spread_down_bins: 0.0,
            ..MaskingParams::new()
        };
        let mut meter = MaskingMeter::new(&p);
        assert_eq!(meter.skirt_radius(), 0);
        let mut x = vec![0.0f32; p.bins];
        for v in x.iter_mut().take(100) {
            *v = 1.0;
        }
        meter.update(&x, &x);
        meter.render_db();
        let floor = p.floor_db;
        assert!(meter.mask_a().iter().all(|&v| near_floor(v, floor)));
        // But the tone is still *heard*: hidden = min(x, AT) = AT.
        assert!(
            (meter.hidden_a()[50] - floor).abs() <= 1e-3,
            "hidden with no skirt must sink to the floor: {}",
            meter.hidden_a()[50]
        );
    }

    /// A↔B honesty: swapping the two spectra flips the sign of every difference.
    #[test]
    fn difference_flips_sign_under_swap() {
        let mut meter = MaskingMeter::new(&MaskingParams::new());
        let (a, b): (Vec<f32>, Vec<f32>) = (0..meter.bins())
            .map(|k| ((k % 31) as f32 * 2e-3 + 1e-3, (k % 17) as f32 * 3e-3))
            .unzip();
        meter.update(&a, &b);
        meter.render_db();
        let ab: Vec<f32> = meter.difference().to_vec();
        meter.update(&b, &a);
        meter.render_db();
        for (x, y) in ab.iter().zip(meter.difference()) {
            assert!(
                (x + y).abs() <= 1e-3,
                "difference not antisymmetric: {x} vs {y}"
            );
        }
    }

    /// The hot-path contract again from the other side: a wrong-length view is
    /// ignored without panic (in release), keeping the previous answers.
    #[test]
    fn update_ignores_mismatched_views_and_keeps_state() {
        let mut meter = MaskingMeter::new(&MaskingParams::new());
        let z = vec![0.0f32; meter.bins()];
        let mut x = z.clone();
        x[10] = 1.0;
        meter.update(&x, &z);
        meter.render_db();
        let before: Vec<f32> = meter.mask_a().to_vec();
        meter.update(&x[..8], &z);
        meter.update(&z, &z[..4]);
        meter.render_db();
        assert_eq!(before, meter.mask_a(), "bad views must not alter state");
    }

    /// `reset` returns exactly the as-new view state (the meter holds only the
    /// last answer — reset means "no answer yet").
    #[test]
    fn reset_reproduces_the_as_new_state() {
        let fresh = MaskingMeter::new(&MaskingParams::new());
        let mut meter = MaskingMeter::new(&MaskingParams::new());
        let (a, b): (Vec<f32>, Vec<f32>) = (0..meter.bins())
            .map(|k| ((k % 13) as f32 * 1e-2, (k % 7) as f32 * 5e-3))
            .unzip();
        meter.update(&a, &b);
        meter.render_db();
        meter.reset();
        assert_eq!(meter.mask_a().to_vec(), fresh.mask_a().to_vec());
        assert_eq!(meter.hidden_b().to_vec(), fresh.hidden_b().to_vec());
        assert_eq!(meter.difference().to_vec(), fresh.difference().to_vec());
        assert!(meter.ta.iter().all(|&v| v == 0.0));
    }

    #[test]
    #[should_panic(expected = "at least 2 bins")]
    fn new_rejects_a_one_bin_axis() {
        let _ = MaskingMeter::new(&MaskingParams {
            bins: 1,
            ..MaskingParams::new()
        });
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn new_rejects_negative_widths() {
        let _ = MaskingMeter::new(&MaskingParams {
            spread_up_bins: -1.0,
            ..MaskingParams::new()
        });
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn new_rejects_negative_weight() {
        let _ = MaskingMeter::new(&MaskingParams {
            weight: -0.5,
            ..MaskingParams::new()
        });
    }

    #[test]
    #[should_panic(expected = "full scale")]
    fn new_rejects_a_threshold_above_full_scale() {
        let _ = MaskingMeter::new(&MaskingParams {
            floor_db: 3.0,
            ..MaskingParams::new()
        });
    }
}
