//! The EQ plugin: [`EqPlugin`] implements the wrapper's safe
//! [`Plugin`](hathor_clap::Plugin) trait over the filters crate's biquads.
//!
//! ## Raw pointers here: none
//!
//! Pure safe Rust, same containment as the analyzer: rows arrive as slices
//! through [`ProcessView`](hathor_clap::ProcessView); the 16 biquad lanes
//! (8 bands x 2 channels) live only between `activate` and `deactivate`.
//! One lane PER CHANNEL: sharing one filter between L and R would be a fake
//! stereo filter with two different states — the filters crate names this
//! trap; obeying it is a correctness property, not a nicety.
//!
//! ## The coefficient doctrine (the part that decides the sound under
//! automation)
//!
//! `set_param_value` (main thread, or host automation) runs the RBJ design —
//! `sin/cos/sqrt` in f64, no allocation — and starts a LINEAR COEFFICIENT
//! RAMP (`Biquad::ramp_to`, [`RAMP_FRAMES`] frames) toward the new set.
//! `process` only walks the ramp: 5 fma per frame per section, zero
//! transcendental, zero allocation. The alternative (design in `activate`
//! only, like the analyzer) would make an EQ useless live; the alternative
//! people actually write (design per block) breaks the bounded-cycle
//! guarantee. Neither is chosen here.
//!
//! ## Bypass semantics (labeled)
//!
//! Bypass freezes the filter STATE: lanes do not advance, output is the
//! untouched pass-through copy, so bypassing is bit-exact. Param edits
//! while bypassed still set ramp targets (pure field writes); the ramp
//! resumes exactly where it stopped. A project restored on a bypassed
//! plugin therefore boots with its bands' sound on the first
//! un-bypassed block — no stale clicks.
//!
//! ## State doctrine
//!
//! 272 bytes: the 34 params, little-endian f64. Filter memory (z1/z2) and
//! in-flight ramps are transient by the same CONVENTION §8.2 logic that
//! frees the analyzer's smoothing history — reloaded projects rebuild them
//! by listening.

use core::ffi::CStr;

use hathor_clap::sys::ext::params::CLAP_PARAM_IS_AUTOMATABLE;
use hathor_clap::sys::plugin::clap_plugin_descriptor;
use hathor_clap::{
    clap_cstr, make_descriptor, AudioPortInfo, FeatureList, Host, InputStream, LogSeverity,
    OutputStream, ParamInfo, ParamText, Plugin, ProcessStatus, ProcessView, SampleFormat,
};

use hathor_dsp_filters::biquad::Biquad;
use hathor_dsp_filters::coeffs::{db_to_lin, design, Coeffs, FilterKind};

/// Number of parametric bands.
pub const BANDS: usize = 8;
/// Params per band: Type, Freq, Gain, Q — the band template.
pub const PARAMS_PER_BAND: usize = 4;
/// Global bypass (0 = processing, 1 = frozen pass-through).
pub const PARAM_BYPASS: u32 = 0;
/// Master trim in dB.
pub const PARAM_MASTER_GAIN: u32 = 1;
/// First band's param id.
pub const PARAM_BAND_BASE: u32 = 2;
/// 2 globals + 8 x 4.
pub const PARAM_COUNT: usize = 2 + BANDS * PARAMS_PER_BAND;

/// The type menu a host sees as the numeric `Type` param (0..6).
pub const TYPE_NAMES: [&str; 7] = [
    "Off",
    "LowPass",
    "HighPass",
    "Peak",
    "Notch",
    "LowShelf",
    "HighShelf",
];

/// Frames over which a coefficient change is interpolated. 128 ≈ 2.7 ms at
/// 48 kHz: long enough to kill zippers on a resonant, short enough to feel
/// instant. The ramp is per-coefficient, never per-parameter value.
const RAMP_FRAMES: usize = 128;

/// Bytes of saved state: every param as an f64.
const STATE_BYTES: usize = PARAM_COUNT * 8;

/// Range/name metadata for one param (also our clamp, so a hostile value
/// can never reach a filter design).
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    /// Display name.
    pub name: &'static str,
    /// Display module/group.
    pub module: &'static str,
    /// Inclusive low end.
    pub min: f64,
    /// Inclusive high end.
    pub max: f64,
}

const fn band_module(b: usize) -> &'static str {
    match b {
        0 => "Band 1",
        1 => "Band 2",
        2 => "Band 3",
        3 => "Band 4",
        4 => "Band 5",
        5 => "Band 6",
        6 => "Band 7",
        _ => "Band 8",
    }
}

const PLACEHOLDER: ParamSpec = ParamSpec {
    name: "",
    module: "",
    min: 0.0,
    max: 0.0,
};

const fn build_specs() -> [ParamSpec; PARAM_COUNT] {
    let mut a = [PLACEHOLDER; PARAM_COUNT];
    a[0] = ParamSpec {
        name: "Bypass",
        module: "Global",
        min: 0.0,
        max: 1.0,
    };
    a[1] = ParamSpec {
        name: "Gain",
        module: "Global",
        min: -24.0,
        max: 24.0,
    };
    let mut b = 0usize;
    while b < BANDS {
        let base = 2 + b * PARAMS_PER_BAND;
        a[base] = ParamSpec {
            name: "Type",
            module: band_module(b),
            min: 0.0,
            max: (TYPE_NAMES.len() - 1) as f64,
        };
        a[base + 1] = ParamSpec {
            name: "Freq",
            module: band_module(b),
            min: 10.0,
            max: 20_000.0,
        };
        a[base + 2] = ParamSpec {
            name: "Gain",
            module: band_module(b),
            min: -24.0,
            max: 24.0,
        };
        a[base + 3] = ParamSpec {
            name: "Q",
            module: band_module(b),
            min: 0.1,
            max: 20.0,
        };
        b += 1;
    }
    a
}

/// The normative v0 param table. The `Type` param selects through
/// [`TYPE_NAMES`] (0 = Off); `Gain` is meaningful for Peak and the shelves,
/// `Q` for everything but the shelves (RBJ slope is fixed at `S = 1.0`,
/// maximally flat — a v0 decision, documented in the report, not hidden).
pub const SPECS: [ParamSpec; PARAM_COUNT] = build_specs();

/// Default param values: identity. Bypass off, trim 0 dB, every band Off —
/// "installed and hearing nothing" is bit-exact, and the tests say so.
pub const fn default_params() -> [f64; PARAM_COUNT] {
    let mut a = [0.0f64; PARAM_COUNT];
    a[1] = 0.0; // master gain dB
    let mut b = 0usize;
    while b < BANDS {
        let base = 2 + b * PARAMS_PER_BAND;
        a[base] = 0.0; // Off
        a[base + 1] = 1_000.0; // freq Hz
        a[base + 2] = 0.0; // gain dB
        a[base + 3] = 1.0; // Q
        b += 1;
    }
    a
}

/// A per-band snapshot for diagnostics and the future (design-gated) UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandState {
    /// Index into [`TYPE_NAMES`].
    pub type_ix: usize,
    /// Center frequency in Hz.
    pub freq_hz: f64,
    /// Gain in dB (Peak/shelves).
    pub gain_db: f64,
    /// Q (everything but the shelves).
    pub q: f64,
}

fn spec_index(id: u32) -> Option<usize> {
    let i = id as usize;
    (i < PARAM_COUNT).then_some(i)
}

/// Clamp + sanitize: a value out of range is a clamp, a non-finite one is
/// the default. Nothing hostile ever reaches a `design()` call.
fn sanitize(value: f64, i: usize) -> f64 {
    let spec = SPECS[i];
    if !value.is_finite() {
        return default_params()[i];
    }
    value.clamp(spec.min, spec.max)
}

fn type_ix(raw: f64) -> usize {
    (raw.round().clamp(0.0, (TYPE_NAMES.len() - 1) as f64)) as usize
}

fn kind_for(ix: usize, gain_db: f32, q: f32) -> Option<FilterKind> {
    match ix {
        0 => None, // Off
        1 => Some(FilterKind::LowPass { q }),
        2 => Some(FilterKind::HighPass { q }),
        3 => Some(FilterKind::Peak { q, gain_db }),
        4 => Some(FilterKind::Notch { q }),
        5 => Some(FilterKind::LowShelf {
            gain_db,
            slope: 1.0,
        }),
        6 => Some(FilterKind::HighShelf {
            gain_db,
            slope: 1.0,
        }),
        _ => None,
    }
}

/// The shipped plugin identity: pinned forever, byte-for-byte tripwired in
/// tests/lifecycle.rs — see the same law documented at the analyzer. The
/// id is `dev.hathor.eq`; `equalizer` is the CLAP feature tag for it.
static DESCRIPTOR: clap_plugin_descriptor = make_descriptor(
    clap_cstr!("dev.hathor.eq"),
    clap_cstr!("Hathor EQ"),
    clap_cstr!("Hathor"),
    clap_cstr!("https://github.com/xGiova/Hathor"),
    clap_cstr!("https://github.com/xGiova/Hathor"),
    clap_cstr!("https://github.com/xGiova/Hathor"),
    // Bump with the milestone tags; the workspace crate version stays a
    // 0.0.0 placeholder by lead decision — this string is the host-visible
    // plugin version, not cargo's.
    clap_cstr!("0.1.0"),
    clap_cstr!("8-band parametric EQ (headless v0; GUI pending design)."),
    Some(FeatureList::new(&[
        hathor_clap::sys::plugin_features::CLAP_PLUGIN_FEATURE_AUDIO_EFFECT.as_ptr(),
        hathor_clap::sys::plugin_features::CLAP_PLUGIN_FEATURE_EQUALIZER.as_ptr(),
        core::ptr::null(),
    ])),
);

/// The plugin: 34 params over 8 bands x 2 channels of biquad lanes.
pub struct EqPlugin {
    host: Option<Host>,
    params: [f64; PARAM_COUNT],
    /// Lane `2*b + c`: band `b`, channel `c`. Sized at `activate`, empty
    /// otherwise; `process` never touches the allocator.
    lanes: Vec<Biquad>,
    /// Master trim as a LINEAR gain, refreshed wherever the dB param moves
    /// (never a `powf` in the hot loop; `db_to_lin(0.0)` is exactly 1.0).
    master_lin: f32,
    sample_rate: f32,
    max_frames: usize,
    active: bool,
    /// 64-bit host path (task-11): one f32 row of scratch per channel, sized
    /// at `activate` like `lanes`. A 64-bit host's f64 rows meet the f32
    /// biquad lanes here: convert in, filter, convert out — declared setup
    /// allocation, zero allocations in `process`, the analyzer's mono-scratch
    /// doctrine with one row each.
    scratch_a: Vec<f32>,
    scratch_b: Vec<f32>,
    /// True only when all 16 lanes rest at `IDENTITY`, no ramp runs, and the
    /// trim is exactly 1.0: the process flow then copies rows at the host's
    /// own width and freezes the lanes — see [`Self::refresh_neutral_flag`],
    /// the ratified task-11 invariant. Derived state, never saved.
    neutral_fast: bool,
}

impl EqPlugin {
    /// The design target of one band under the CURRENT stored params.
    /// Frequency is clamped below 0.45 x fs (a design at exactly Nyquist is
    /// the edge RBJ handles worst); a refused design falls back to
    /// `IDENTITY`, never to a poisoned filter: `design()` itself documents
    /// that a NaN filter poisons the whole graph, and the blame would not
    /// be its.
    fn band_target(&self, b: usize) -> Coeffs {
        let base = 2 + b * PARAMS_PER_BAND;
        let gain = self.params[base + 2] as f32;
        let q = self.params[base + 3] as f32;
        let f0 = (self.params[base + 1] as f32).clamp(1.0, self.sample_rate * 0.45);
        match kind_for(type_ix(self.params[base]), gain, q) {
            None => Coeffs::IDENTITY,
            Some(kind) => design(kind, f0, self.sample_rate).unwrap_or(Coeffs::IDENTITY),
        }
    }

    /// Push one changed param into the DSP state. Called from the main
    /// thread (UI/host) and from host automation; both are outside the
    /// frame-by-frame budget by construction: this is a handful of
    /// transcendentals per EVENT, never per sample.
    fn apply_param(&mut self, i: usize) {
        if !self.active {
            return; // params are stored; activate() settles every lane
        }
        match i {
            0 => {} // bypass: pure process-flow flag, nothing to design
            1 => self.master_lin = db_to_lin(self.params[1] as f32),
            _ => {
                let b = (i - 2) / PARAMS_PER_BAND;
                let target = self.band_target(b);
                self.lanes[2 * b].ramp_to(target, RAMP_FRAMES);
                self.lanes[2 * b + 1].ramp_to(target, RAMP_FRAMES);
            }
        }
        if i != 0 {
            // Any lane/design edit re-evaluates the fast flag (mid-ramp the
            // test fails → false; edits back to neutral flip it once the
            // ramps land — see `refresh_neutral_flag`).
            self.refresh_neutral_flag();
        }
    }

    /// Re-evaluate the neutral fast-path flag (task-11, ratified invariant):
    /// neutral means every lane at rest on `IDENTITY` coefficients, no ramp
    /// running, and the trim exactly 1.0. While the flag is on a 64-bit host
    /// gets a direct f64→f64 row copy — bit-exact even for samples the f32
    /// scratch could not represent (1/3, sqrt(2), subnormals) — and an f32
    /// host gets the same bytes from the identity copy without ever touching
    /// a filter recursion. Lanes frozen here is the exact doctrine the
    /// shipping bypass freeze already establishes (task-10): nothing ramps,
    /// so nothing advances; the first edit after a neutral stretch ramps
    /// from these frozen coefficients, same as after any bypassed stretch.
    fn refresh_neutral_flag(&mut self) {
        self.neutral_fast = !self.lanes.is_empty()
            && !self.lanes.iter().any(Biquad::is_ramping)
            && self.lanes.iter().all(|l| l.coeffs() == Coeffs::IDENTITY)
            && self.master_lin == 1.0;
    }

    /// The 34 params, little-endian — the whole saved state.
    fn serialize(&self) -> [u8; STATE_BYTES] {
        let mut blob = [0u8; STATE_BYTES];
        for (i, v) in self.params.iter().enumerate() {
            blob[i * 8..(i + 1) * 8].copy_from_slice(&v.to_le_bytes());
        }
        blob
    }

    /// Restore params from a blob: every value goes through the same
    /// sanitize a live `set_param_value` would apply, then every lane is
    /// RAMPED (not jumped) to the restored targets — loading a project must
    /// not click.
    fn apply_state(&mut self, blob: &[u8; STATE_BYTES]) {
        for i in 0..PARAM_COUNT {
            let mut word = [0u8; 8];
            word.copy_from_slice(&blob[i * 8..(i + 1) * 8]);
            let raw = f64::from_le_bytes(word);
            self.params[i] = sanitize(raw, i);
        }
        if self.active {
            self.master_lin = db_to_lin(self.params[1] as f32);
            for b in 0..BANDS {
                let target = self.band_target(b);
                self.lanes[2 * b].ramp_to(target, RAMP_FRAMES);
                self.lanes[2 * b + 1].ramp_to(target, RAMP_FRAMES);
            }
            // Restoring a neutral project leaves ramps running; the flag
            // re-arms itself on the first settled block (process tail).
            self.refresh_neutral_flag();
        }
    }

    /// Diagnostic snapshot of one band (0-based).
    pub fn band_state(&self, b: usize) -> Option<BandState> {
        if b >= BANDS {
            return None;
        }
        let base = 2 + b * PARAMS_PER_BAND;
        Some(BandState {
            type_ix: type_ix(self.params[base]),
            freq_hz: self.params[base + 1],
            gain_db: self.params[base + 2],
            q: self.params[base + 3],
        })
    }

    /// True if the plugin currently rides bypassed (frozen pass-through).
    pub fn is_bypassed(&self) -> bool {
        self.params[PASS_IDX] >= 0.5
    }

    /// Diagnostic (task-11): is the neutral copy fast-path armed? The strict
    /// zero-allocation tests assert `false` here while bands are engaged —
    /// the flag is the proof-of-path the guard needs: with it down, the
    /// biquad recursions are what ran; with it up, only copies did.
    pub fn is_neutral_fast(&self) -> bool {
        self.neutral_fast
    }

    /// The f32 world: unchanged from the day the crate shipped, plus the
    /// neutral fast flag (both are copies, never recursions).
    fn run32(&mut self, io: &mut ProcessView<'_>, in_ch: usize, out_ch: usize) -> ProcessStatus {
        // Input rows first: `&'a` slices independent of further `io` borrows.
        let Some(l) = io.input_channel(0, 0) else {
            return ProcessStatus::Error;
        };
        let r = if in_ch >= 2 {
            io.input_channel(0, 1)
        } else {
            None
        };
        if !io.out_aliases_in() {
            let Some(o0) = io.output_channel(0, 0) else {
                return ProcessStatus::Error;
            };
            o0.copy_from_slice(l);
            if out_ch >= 2 {
                let Some(o1) = io.output_channel(0, 1) else {
                    return ProcessStatus::Error;
                };
                match r {
                    Some(rr) => o1.copy_from_slice(rr),
                    // Mono in, stereo out: silence on the right, same
                    // doctrine as the analyzer — an EQ does not decide
                    // routing, the host does.
                    None => o1.fill(0.0),
                }
            }
        }
        if self.is_bypassed() {
            // Bypassed: lanes frozen, output already the untouched copy.
            return ProcessStatus::Continue;
        }
        if self.neutral_fast {
            // The host's bytes ARE the identity chain's output (task-11
            // invariant): skip the recursion, lanes stay frozen.
            return ProcessStatus::Continue;
        }
        // One mutable row at a time (the view's discipline), one lane set
        // per channel (the filters crate's discipline).
        if let Some(o0) = io.output_channel(0, 0) {
            for b in 0..BANDS {
                self.lanes[2 * b].process(o0);
            }
            if self.master_lin != 1.0 {
                for x in o0.iter_mut() {
                    *x *= self.master_lin;
                }
            }
        }
        if out_ch >= 2 {
            if let Some(o1) = io.output_channel(0, 1) {
                for b in 0..BANDS {
                    self.lanes[2 * b + 1].process(o1);
                }
                if self.master_lin != 1.0 {
                    for x in o1.iter_mut() {
                        *x *= self.master_lin;
                    }
                }
            }
        }
        self.refresh_neutral_flag();
        ProcessStatus::Continue
    }

    /// The f64 world: the same lanes, reached through the activate-sized
    /// scratch. In-place pairs keep their double-width pass-through (frozen
    /// bytes when bypassed or neutral); hot blocks convert row→scratch→row.
    /// `0.5` of a value representable in f32 stays bit-exact across both
    /// casts — but the invariant test uses values NOT representable in f32,
    /// because those are the ones that would catch a stray detour.
    fn run64(&mut self, io: &mut ProcessView<'_>, in_ch: usize, out_ch: usize) -> ProcessStatus {
        let n = io.frames();
        let Some(l) = io.input_channel_f64(0, 0) else {
            return ProcessStatus::Error;
        };
        let r = if in_ch >= 2 {
            io.input_channel_f64(0, 1)
        } else {
            None
        };
        if self.is_bypassed() {
            // Frozen pass-through at double width: the host's own bytes, no
            // scratch, no ramp advance.
            if !io.out_aliases_in() {
                let Some(o0) = io.output_channel_f64(0, 0) else {
                    return ProcessStatus::Error;
                };
                o0.copy_from_slice(l);
                if out_ch >= 2 {
                    let Some(o1) = io.output_channel_f64(0, 1) else {
                        return ProcessStatus::Error;
                    };
                    match r {
                        Some(rr) => o1.copy_from_slice(rr),
                        None => o1.fill(0.0),
                    }
                }
            }
            return ProcessStatus::Continue;
        }
        let aliased = io.out_aliases_in();
        if self.neutral_fast {
            // Ratified invariant (task-11): the neutral 64-bit chain never
            // detours through the f32 scratch — copy at the host's own
            // width (or, in-place, leave the host's bytes alone), exactly
            // like the bypass branch above. A conversion here would round
            // 1/3, sqrt(2) and subnormals; the hostile-values test proves
            // the detour is absent by construction.
            if !aliased {
                let Some(o0) = io.output_channel_f64(0, 0) else {
                    return ProcessStatus::Error;
                };
                o0.copy_from_slice(l);
                if out_ch >= 2 {
                    let Some(o1) = io.output_channel_f64(0, 1) else {
                        return ProcessStatus::Error;
                    };
                    match r {
                        Some(rr) => o1.copy_from_slice(rr),
                        None => o1.fill(0.0),
                    }
                }
            }
            return ProcessStatus::Continue;
        }
        // Channel 0. The scratch borrows `self.scratch_a`, the lanes borrow
        // `self.lanes` — disjoint fields, the borrow checker is fine with the
        // interleaving as written below (one scope per channel).
        {
            let sc = &mut self.scratch_a[..n];
            if aliased {
                // In-place: the output row already carries the input.
                let Some(o0) = io.output_channel_f64(0, 0) else {
                    return ProcessStatus::Error;
                };
                for (s, &x) in sc.iter_mut().zip(o0.iter()) {
                    *s = x as f32;
                }
            } else {
                for (s, &x) in sc.iter_mut().zip(l.iter()) {
                    *s = x as f32;
                }
            }
            for b in 0..BANDS {
                self.lanes[2 * b].process(sc);
            }
            if self.master_lin != 1.0 {
                for x in sc.iter_mut() {
                    *x *= self.master_lin;
                }
            }
        }
        let Some(o0) = io.output_channel_f64(0, 0) else {
            return ProcessStatus::Error;
        };
        for (d, &s) in o0.iter_mut().zip(self.scratch_a[..n].iter()) {
            *d = s as f64;
        }
        // Channel 1 (stereo out): sourced from the right row, or silence for
        // mono in — the routing doctrine, unchanged at either width.
        if out_ch >= 2 {
            {
                let sc = &mut self.scratch_b[..n];
                if aliased {
                    let Some(o1) = io.output_channel_f64(0, 1) else {
                        return ProcessStatus::Error;
                    };
                    for (s, &x) in sc.iter_mut().zip(o1.iter()) {
                        *s = x as f32;
                    }
                } else {
                    match r {
                        Some(rr) => {
                            for (s, &x) in sc.iter_mut().zip(rr.iter()) {
                                *s = x as f32;
                            }
                        }
                        None => {
                            for s in sc.iter_mut() {
                                *s = 0.0;
                            }
                        }
                    }
                }
                for b in 0..BANDS {
                    self.lanes[2 * b + 1].process(sc);
                }
                if self.master_lin != 1.0 {
                    for x in sc.iter_mut() {
                        *x *= self.master_lin;
                    }
                }
            }
            let Some(o1) = io.output_channel_f64(0, 1) else {
                return ProcessStatus::Error;
            };
            for (d, &s) in o1.iter_mut().zip(self.scratch_b[..n].iter()) {
                *d = s as f64;
            }
        }
        self.refresh_neutral_flag();
        ProcessStatus::Continue
    }
}

const PASS_IDX: usize = PARAM_BYPASS as usize;

impl Plugin for EqPlugin {
    const DESCRIPTOR: &'static clap_plugin_descriptor = &DESCRIPTOR;

    fn instantiate() -> Self {
        Self {
            host: None,
            params: default_params(),
            lanes: Vec::new(),
            master_lin: 1.0,
            sample_rate: 0.0,
            max_frames: 0,
            active: false,
            scratch_a: Vec::new(),
            scratch_b: Vec::new(),
            neutral_fast: false,
        }
    }

    fn init(&mut self, host: Host) -> bool {
        let logged = host.log(LogSeverity::Info, clap_cstr!("hathor-eq init"));
        self.host = Some(host);
        // A host without `clap.log` is still a valid host: log failure is
        // not an init failure.
        let _ = logged;
        true
    }

    fn activate(&mut self, sample_rate: f64, _min_frames: u32, max_frames: u32) -> bool {
        // The honest operating envelope for a fixed-topology biquad EQ.
        // Below 8 kHz an EQ is a rumor; above 192 kHz this v0 is untested
        // territory — refuse rather than pretend.
        if !sample_rate.is_finite() || !(8_000.0..=192_000.0).contains(&sample_rate) {
            return false;
        }
        if max_frames == 0 || max_frames > 1 << 20 {
            return false;
        }
        self.sample_rate = sample_rate as f32;
        self.max_frames = max_frames as usize;
        // Setup allocation, declared and bounded: 16 lanes + two scratch rows, once.
        let mut lanes = Vec::with_capacity(BANDS * 2);
        for b in 0..BANDS {
            let target = self.band_target(b);
            lanes.push(Biquad::new(target));
            lanes.push(Biquad::new(target));
        }
        self.lanes = lanes;
        self.scratch_a = vec![0.0f32; self.max_frames];
        self.scratch_b = vec![0.0f32; self.max_frames];
        self.master_lin = db_to_lin(self.params[1] as f32);
        self.active = true;
        self.refresh_neutral_flag();
        true
    }

    fn deactivate(&mut self) {
        self.lanes = Vec::new();
        self.scratch_a = Vec::new();
        self.scratch_b = Vec::new();
        self.neutral_fast = false;
        self.active = false;
    }

    fn reset(&mut self) {
        for lane in &mut self.lanes {
            lane.reset();
        }
    }

    /// THE audio path: copy (unless the host handed the in-place pair),
    /// then per output row its channel's 8 lanes, then the master trim.
    /// With every band `Off` the lanes are `IDENTITY` biquads and the trim
    /// is exactly 1.0, so the default chain is bit-exact — which is what
    /// the integration test asserts over random audio.
    ///
    /// Both wire widths are served, chosen from each port's pointer pair
    /// (task-11): the f32 world runs the shipping path; the f64 world flows
    /// through the scratch — convert in, the SAME lanes, convert out. Bands
    /// ramp identically in both (the ramps ride the lanes, not the rows). A
    /// port that breaks the XOR or a mixed-width pair is refused (Error):
    /// no guessing on a malformed host's behalf.
    fn process(&mut self, io: &mut ProcessView<'_>) -> ProcessStatus {
        if !self.active {
            return ProcessStatus::Error;
        }
        let frames = io.frames();
        if frames > self.max_frames {
            // Host exceeded the max block promised at `activate` (spec
            // violation; hosts expected NOT VERIFIED): refuse BEFORE
            // writing, so a failed block is an untouched block.
            return ProcessStatus::Error;
        }
        let in_ch = match io.port_channels(0, true) {
            Some(c) if c >= 1 => c as usize,
            _ => return ProcessStatus::Error,
        };
        let out_ch = match io.port_channels(0, false) {
            Some(c) if c >= 1 => c as usize,
            _ => return ProcessStatus::Error,
        };
        match (io.channel_format(0, true), io.channel_format(0, false)) {
            (Some(SampleFormat::F32), Some(SampleFormat::F32)) => self.run32(io, in_ch, out_ch),
            (Some(SampleFormat::F64), Some(SampleFormat::F64)) => self.run64(io, in_ch, out_ch),
            _ => ProcessStatus::Error, // malformed XOR or mixed widths: refuse, never guess
        }
    }

    // ---- ports ----

    fn audio_port_info(&self, index: u32, is_input: bool) -> Option<AudioPortInfo<'_>> {
        if index != 0 {
            return None;
        }
        let mut info = if is_input {
            AudioPortInfo::main_stereo(10, "Main In")
        } else {
            AudioPortInfo::main_stereo(20, "Main Out")
        };
        info.in_place_pair = Some(if is_input { 20 } else { 10 });
        Some(info)
    }

    // ---- params ----

    fn param_count(&self) -> u32 {
        PARAM_COUNT as u32
    }

    fn param_info(&self, index: u32) -> Option<ParamInfo<'_>> {
        let i = spec_index(index)?;
        let spec = SPECS[i];
        Some(ParamInfo {
            id: i as u32,
            flags: CLAP_PARAM_IS_AUTOMATABLE,
            name: spec.name,
            module: spec.module,
            min_value: spec.min,
            max_value: spec.max,
            default_value: default_params()[i],
        })
    }

    fn param_value(&self, id: u32) -> Option<f64> {
        spec_index(id).map(|i| self.params[i])
    }

    fn set_param_value(&mut self, id: u32, value: f64) {
        let Some(i) = spec_index(id) else {
            return; // unknown id: ignore per CLAP, never guess
        };
        self.params[i] = sanitize(value, i);
        self.apply_param(i);
        // The value change is saved-state-visible: tell the host.
        if let Some(h) = self.host {
            h.state_mark_dirty();
        }
    }

    fn param_text(&self, id: u32, value: f64, out: &mut ParamText<'_>) -> bool {
        if spec_index(id).is_none() {
            return false;
        }
        out.write_f64(value)
    }

    fn param_from_text(&self, _id: u32, text: &CStr) -> Option<f64> {
        // Plain decimal display, plain decimal parse; anything else refused.
        text.to_str().ok()?.parse::<f64>().ok()
    }

    // ---- state ----

    fn save_state(&mut self, out: &mut OutputStream<'_>) -> bool {
        let blob = self.serialize();
        out.write(&blob)
    }

    fn load_state(&mut self, input: &mut InputStream<'_>) -> bool {
        let mut blob = [0u8; STATE_BYTES];
        if !input.read_exact(&mut blob) {
            return false;
        }
        self.apply_state(&blob);
        true
    }

    // ---- latency: zero, declared: minimum-phase biquads with no
    // block-spanning delay; the graph's only memory is the filter state. ----

    fn latency(&self) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hathor_clap::sys::audio_buffer::clap_audio_buffer;
    use hathor_clap::sys::process::clap_process;

    /// Drive the REAL `Plugin::process` adapter through a fabricated,
    /// well-formed `clap_process` — the same pattern proven in the analyzer
    /// (task-4/5): the mock plays the host's bookkeeping burden.
    fn drive(
        p: &mut EqPlugin,
        in_ptrs: &mut [*mut f32],
        out_ptrs: &mut [*mut f32],
        frames: u32,
        rate: f64,
    ) -> ProcessStatus {
        let in_buf = clap_audio_buffer {
            data32: in_ptrs.as_mut_ptr(),
            data64: core::ptr::null_mut(),
            channel_count: in_ptrs.len() as u32,
            latency: 0,
            constant_mask: 0,
        };
        let mut out_buf = clap_audio_buffer {
            data32: out_ptrs.as_mut_ptr(),
            data64: core::ptr::null_mut(),
            channel_count: out_ptrs.len() as u32,
            latency: 0,
            constant_mask: 0,
        };
        let proc = clap_process {
            steady_time: 0,
            frames_count: frames,
            transport: core::ptr::null(),
            audio_inputs: &raw const in_buf,
            audio_outputs: &raw mut out_buf,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: core::ptr::null(),
            out_events: core::ptr::null(),
        };
        // SAFETY: every field is well-formed for the duration of this call:
        // the row arrays sit in the callers' live buffers, the channel
        // counts match the array lengths, `frames` matches the slices the
        // rows back (the test's own invariant, asserted before we get here).
        let mut view = unsafe { ProcessView::from_sys(&proc, rate) };
        p.process(&mut view)
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }
        fn f32s(&mut self, n: usize) -> Vec<f32> {
            (0..n)
                .map(|_| (self.next() as f32 / u32::MAX as f32) * 2.0 - 1.0)
                .collect()
        }
    }

    fn sine(freq: f64, rate: f64, n: usize, offset_frames: usize) -> Vec<f32> {
        (0..n)
            .map(|k| {
                let t = (offset_frames + k) as f64;
                (core::f64::consts::TAU * freq * t / rate).sin() as f32 * 0.5
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        let acc: f64 = x.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        (acc / x.len().max(1) as f64).sqrt()
    }

    fn eq_active(rate: f64) -> EqPlugin {
        let mut p = EqPlugin::instantiate();
        assert!(p.activate(rate, 16, 64));
        p
    }

    /// 64-bit twin of [`drive`]: `data64` rows only, `data32` null — a
    /// well-formed double-precision host per the wrapper's XOR doctrine.
    fn drive64(
        p: &mut EqPlugin,
        in_ptrs: &mut [*mut f64],
        out_ptrs: &mut [*mut f64],
        frames: u32,
        rate: f64,
    ) -> ProcessStatus {
        let in_buf = clap_audio_buffer {
            data32: core::ptr::null_mut(),
            data64: in_ptrs.as_mut_ptr(),
            channel_count: in_ptrs.len() as u32,
            latency: 0,
            constant_mask: 0,
        };
        let mut out_buf = clap_audio_buffer {
            data32: core::ptr::null_mut(),
            data64: out_ptrs.as_mut_ptr(),
            channel_count: out_ptrs.len() as u32,
            latency: 0,
            constant_mask: 0,
        };
        let proc = clap_process {
            steady_time: 0,
            frames_count: frames,
            transport: core::ptr::null(),
            audio_inputs: &raw const in_buf,
            audio_outputs: &raw mut out_buf,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: core::ptr::null(),
            out_events: core::ptr::null(),
        };
        // SAFETY: same discipline as `drive`, at double width: the row
        // arrays live in the caller's buffers for the whole call.
        let mut view = unsafe { ProcessView::from_sys(&proc, rate) };
        p.process(&mut view)
    }

    fn sine64(freq: f64, rate: f64, n: usize, offset_frames: usize) -> Vec<f64> {
        (0..n)
            .map(|k| {
                let t = (offset_frames + k) as f64;
                core::f64::consts::TAU * freq * t / rate
            })
            .map(f64::sin)
            .map(|v| v * 0.5)
            .collect()
    }

    fn rms64(x: &[f64]) -> f64 {
        let acc: f64 = x.iter().map(|v| v * v).sum();
        (acc / x.len().max(1) as f64).sqrt()
    }

    #[test]
    fn param_table_is_consistent_and_default_neutral() {
        assert_eq!(PARAM_COUNT, 34);
        assert_eq!(SPECS.len(), PARAM_COUNT);
        assert_eq!(SPECS[0].max, 1.0); // Bypass
        assert_eq!(SPECS[2].name, "Type");
        assert_eq!(SPECS[2].module, "Band 1");
        assert_eq!(SPECS[33].name, "Q");
        assert_eq!(SPECS[33].module, "Band 8");
        assert_eq!((SPECS[2].min, SPECS[2].max), (0.0, 6.0));
        let d = default_params();
        for i in 0..PARAM_COUNT {
            assert!(
                d[i] >= SPECS[i].min && d[i] <= SPECS[i].max,
                "default in range {i}"
            );
        }
        assert_eq!((d[0], d[1]), (0.0, 0.0), "bypassed off, trim at unity");
        assert_eq!(d[2], 0.0, "band 1 starts Off");
        assert_eq!(d[3], 1_000.0, "band 1 freq default");
        assert_eq!(d[5], 1.0, "band 1 Q default");
    }

    #[test]
    fn defaults_are_bit_exact_identity_and_lanes_settle_as_identity() {
        let mut p = eq_active(48_000.0);
        for lane in &p.lanes {
            assert_eq!(lane.coeffs(), Coeffs::IDENTITY);
        }
        assert_eq!(p.master_lin, 1.0, "db_to_lin(0) is exactly 1.0");
        let mut rng = Lcg(1_664_525);
        let mut ia = rng.f32s(64);
        let mut ib = rng.f32s(64);
        let mut oa = vec![-7.0f32; 64];
        let mut ob = vec![-7.0f32; 64];
        for _ in 0..24 {
            let mut inp: [*mut f32; 2] = [ia.as_mut_ptr(), ib.as_mut_ptr()];
            let mut outp: [*mut f32; 2] = [oa.as_mut_ptr(), ob.as_mut_ptr()];
            let st = drive(&mut p, &mut inp, &mut outp, 64, 48_000.0);
            assert!(matches!(st, ProcessStatus::Continue));
        }
        assert_eq!(
            oa.iter().map(|v| v.to_ne_bytes()).collect::<Vec<_>>(),
            ia.iter().map(|v| v.to_ne_bytes()).collect::<Vec<_>>(),
            "neutral chain bit-exact on L"
        );
        assert_eq!(
            ob.iter().map(|v| v.to_ne_bytes()).collect::<Vec<_>>(),
            ib.iter().map(|v| v.to_ne_bytes()).collect::<Vec<_>>(),
            "neutral chain bit-exact on R (independent state, same outcome)"
        );
    }

    #[test]
    fn lp_band_attenuates_a_hot_tone_and_passes_a_low_one() {
        let mut p = eq_active(48_000.0);
        // Band 1: LowPass, 1 kHz, Q 0.707 (set BEFORE nothing else — the
        // params path is the only door).
        p.set_param_value(PARAM_BAND_BASE, 1.0); // type LowPass
        p.set_param_value(PARAM_BAND_BASE + 1, 1_000.0); // freq
        p.set_param_value(PARAM_BAND_BASE + 3, 0.707);
        // Block-exact bins (multiples of 48000/64 = 750 Hz): the RMS of a
        // 64-sample window is phase-independent only when it holds whole
        // cycles — 5250 Hz is 7.0 cycles, 750 Hz is 1.0. A "5 kHz" test here
        // would measure the window, not the filter (measured: 3.5 dB lie).
        let mut hot = sine(5_250.0, 48_000.0, 64, 0);
        let mut z = vec![0.0f32; 64];
        let mut zout = vec![0.0f32; 64];
        // settle the filter response over a few identical blocks
        let mut out = vec![0.0f32; 64];
        for _ in 0..64 {
            let mut inp: [*mut f32; 2] = [hot.as_mut_ptr(), z.as_mut_ptr()];
            let mut outp: [*mut f32; 2] = [out.as_mut_ptr(), zout.as_mut_ptr()];
            drive(&mut p, &mut inp, &mut outp, 64, 48_000.0);
        }
        let att = 20.0 * (rms(&out) / rms(&hot)).log10();
        assert!(att <= -20.0, "LP@1k on a 5250 Hz tone: measured {att} dB");
        let mut low = sine(750.0, 48_000.0, 64, 0);
        for _ in 0..64 {
            let mut inp: [*mut f32; 2] = [low.as_mut_ptr(), z.as_mut_ptr()];
            let mut outp: [*mut f32; 2] = [out.as_mut_ptr(), zout.as_mut_ptr()];
            drive(&mut p, &mut inp, &mut outp, 64, 48_000.0);
        }
        let pass = 20.0 * (rms(&out) / rms(&low)).log10();
        assert!(pass >= -5.0, "LP@1k on a 750 Hz tone: measured {pass} dB");
    }

    #[test]
    fn peak_at_zero_gain_is_identity_within_tolerance() {
        let mut p = eq_active(48_000.0);
        p.set_param_value(PARAM_BAND_BASE, 3.0); // Peak
        p.set_param_value(PARAM_BAND_BASE + 2, 0.0); // 0 dB
                                                     // RBJ peaking at 0 dB reduces to identity algebraically; f32
                                                     // narrowing leaves only ulp-sized residue.
        for _ in 0..8 {
            let mut ia = [0.25f32; 64];
            let mut z = vec![0.0f32; 64];
            let mut zout = vec![0.0f32; 64];
            let mut out = vec![0.0f32; 64];
            let mut inp: [*mut f32; 2] = [ia.as_mut_ptr(), z.as_mut_ptr()];
            let mut outp: [*mut f32; 2] = [out.as_mut_ptr(), zout.as_mut_ptr()];
            drive(&mut p, &mut inp, &mut outp, 64, 48_000.0);
        }
        // RBJ peaking at 0 dB reduces to the identity TRANSFER FUNCTION:
        // after normalization B(z) ≡ A(z), which makes the DF2T recursion
        // return the input exactly. The coefficient set is NOT [1,0,0,0,0]
        // (the poles/b zeros cancel only in the ratio) — assert the honest
        // condition.
        let c = p.lanes[0].coeffs();
        assert!((c.b0 - 1.0).abs() < 1e-6, "b0 = {}", c.b0);
        assert!((c.b1 - c.a1).abs() < 1e-6, "B == A at z^-1");
        assert!((c.b2 - c.a2).abs() < 1e-6, "B == A at z^-2");
    }

    #[test]
    fn bypass_freezes_state_and_unbypass_resumes_the_ramp() {
        let mut p = eq_active(48_000.0);
        p.set_param_value(PARAM_BAND_BASE, 1.0); // LP 1k
        p.set_param_value(PARAM_BAND_BASE + 1, 1_000.0);
        let hot = sine(5_250.0, 48_000.0, 64 * 40, 0);
        let z = vec![0.0f32; 64];
        let run = |p: &mut EqPlugin| {
            let mut out = vec![0.0f32; 64];
            let mut acc = Vec::new();
            for k in 0..40 {
                let mut slice = hot[k * 64..(k + 1) * 64].to_vec();
                let mut zin = z.clone();
                let mut zout = z.clone();
                let mut inp: [*mut f32; 2] = [slice.as_mut_ptr(), zin.as_mut_ptr()];
                let mut outp: [*mut f32; 2] = [out.as_mut_ptr(), zout.as_mut_ptr()];
                drive(p, &mut inp, &mut outp, 64, 48_000.0);
                acc.extend_from_slice(&out);
            }
            acc
        };
        let shaped = run(&mut p);
        assert!(
            20.0 * (rms(&shaped[1280..]) / rms(&hot[1280..])).log10() <= -20.0,
            "LP should have bit the tone"
        );
        p.set_param_value(PARAM_BYPASS, 1.0);
        let frozen = run(&mut p);
        for (got, want) in frozen.iter().zip(hot.iter()) {
            assert_eq!(got.to_ne_bytes(), want.to_ne_bytes(), "bypass bit-exact");
        }
        p.set_param_value(PARAM_BYPASS, 0.0);
        let again = run(&mut p);
        assert!(
            20.0 * (rms(&again[1280..]) / rms(&hot[1280..])).log10() <= -20.0,
            "un-bypass restored the bite"
        );
    }

    #[test]
    fn hostile_params_are_clamped_and_never_poison_the_graph() {
        let mut p = eq_active(48_000.0);
        let poison = [f64::NAN, f64::INFINITY, 1e300, -1e300, 7.4, -3.0, 0.0, 6.0];
        for round in 0..2 {
            for i in 0..PARAM_COUNT {
                p.set_param_value(i as u32, poison[(i + round) % poison.len()]);
            }
        }
        for (i, spec) in SPECS.iter().enumerate() {
            let v = p.param_value(i as u32).unwrap_or(f64::NAN);
            assert!(
                v.is_finite() && v >= spec.min && v <= spec.max,
                "param {i} = {v}"
            );
        }
        let mut ia = vec![0.1f32; 64];
        let mut ib = vec![-0.1f32; 64];
        for _ in 0..8 {
            let mut outa = vec![0.0f32; 64];
            let mut outb = vec![0.0f32; 64];
            let mut inp: [*mut f32; 2] = [ia.as_mut_ptr(), ib.as_mut_ptr()];
            let mut outp: [*mut f32; 2] = [outa.as_mut_ptr(), outb.as_mut_ptr()];
            let st = drive(&mut p, &mut inp, &mut outp, 64, 48_000.0);
            assert!(matches!(st, ProcessStatus::Continue));
            assert!(
                outa.iter().all(|v| v.is_finite()) && outb.iter().all(|v| v.is_finite()),
                "no NaN survived the clamp"
            );
        }
    }

    #[test]
    fn activate_refusals_and_process_outside_active() {
        let mut p = EqPlugin::instantiate();
        assert!(!p.activate(4_000.0, 16, 64), "below 8k refused");
        assert!(!p.activate(200_000.0, 16, 64), "above 192k refused");
        assert!(!p.activate(48_000.0, 16, 0), "zero max refused");
        assert!(!p.activate(48_000.0, 16, 1 << 21), "absurd max refused");
        let mut ia = vec![0.5f32; 64];
        let mut oa = vec![0.0f32; 64];
        {
            let mut inp: [*mut f32; 1] = [ia.as_mut_ptr()];
            let mut outp: [*mut f32; 1] = [oa.as_mut_ptr()];
            assert!(
                matches!(
                    drive(&mut p, &mut inp, &mut outp, 64, 48_000.0),
                    ProcessStatus::Error
                ),
                "process before activate"
            );
        }
        assert!(p.activate(44_100.0, 16, 64));
        p.deactivate();
        let mut inp: [*mut f32; 1] = [ia.as_mut_ptr()];
        let mut outp: [*mut f32; 1] = [oa.as_mut_ptr()];
        assert!(
            matches!(
                drive(&mut p, &mut inp, &mut outp, 64, 44_100.0),
                ProcessStatus::Error
            ),
            "process after deactivate"
        );
    }

    #[test]
    fn oversized_block_refuses_before_touching_anything() {
        let mut p = eq_active(48_000.0);
        let mut ia = vec![0.25f32; 128];
        let mut oa = vec![-9.0f32; 128];
        let keep = oa.clone();
        let mut inp: [*mut f32; 1] = [ia.as_mut_ptr()];
        let mut outp: [*mut f32; 1] = [oa.as_mut_ptr()];
        assert!(matches!(
            drive(&mut p, &mut inp, &mut outp, 128, 48_000.0),
            ProcessStatus::Error
        ));
        assert_eq!(oa, keep, "refused block left the output rows untouched");
    }

    #[test]
    fn state_round_trips_and_hostile_blobs_come_out_legal() {
        let mut p = eq_active(48_000.0);
        p.set_param_value(PARAM_MASTER_GAIN, -2.5);
        p.set_param_value(PARAM_BAND_BASE, 3.0); // Peak
        p.set_param_value(PARAM_BAND_BASE + 1, 3_500.0);
        p.set_param_value(PARAM_BAND_BASE + 2, 6.0);
        p.set_param_value(PARAM_BAND_BASE + 3, 0.8);
        p.set_param_value(PARAM_BAND_BASE + 4 * 5, 1.0); // band 6 LP
        p.set_param_value(PARAM_BAND_BASE + 4 * 5 + 1, 220.0);
        let blob = p.serialize();
        assert_eq!(blob.len(), STATE_BYTES);
        // Drive p through its full transient so its lanes settle.
        let mut ia = vec![0.2f32; 64];
        let mut ib = vec![-0.2f32; 64];
        let mut run = |q: &mut EqPlugin, acc: &mut Vec<f32>| {
            for _ in 0..32 {
                let mut oa = vec![0.0f32; 64];
                let mut ob = vec![0.0f32; 64];
                let mut inp: [*mut f32; 2] = [ia.as_mut_ptr(), ib.as_mut_ptr()];
                let mut outp: [*mut f32; 2] = [oa.as_mut_ptr(), ob.as_mut_ptr()];
                drive(q, &mut inp, &mut outp, 64, 48_000.0);
                acc.extend_from_slice(&oa);
                acc.extend_from_slice(&ob);
            }
        };
        let mut acc_a = Vec::new();
        run(&mut p, &mut acc_a);
        let mut q = eq_active(48_000.0);
        q.apply_state(&blob);
        let mut acc_b = Vec::new();
        run(&mut q, &mut acc_b);
        assert_eq!(acc_a.len(), acc_b.len());
        for (a, b) in acc_a.iter().zip(acc_b.iter()) {
            assert_eq!(
                a.to_ne_bytes(),
                b.to_ne_bytes(),
                "restored state reproduces audio bit-exact after settling"
            );
        }
        // A blob of pure 0xFF decodes to -NaN-ish garbage; every value must
        // come back inside its spec.
        let mut q2 = eq_active(48_000.0);
        let evil = [0xFFu8; STATE_BYTES];
        q2.apply_state(&evil);
        for (i, spec) in SPECS.iter().enumerate() {
            let v = q2.param_value(i as u32).unwrap_or(f64::NAN);
            assert!(
                v.is_finite() && v >= spec.min && v <= spec.max,
                "hostile blob: param {i} = {v}"
            );
        }
    }

    #[test]
    fn mono_in_stereo_out_keeps_the_silence_doctrine() {
        let mut p = eq_active(48_000.0);
        let mut ia = vec![0.3f32; 64];
        let mut oa = vec![-1.0f32; 64];
        let mut ob = vec![-1.0f32; 64];
        let mut inp: [*mut f32; 1] = [ia.as_mut_ptr()];
        let mut outp: [*mut f32; 2] = [oa.as_mut_ptr(), ob.as_mut_ptr()];
        assert!(matches!(
            drive(&mut p, &mut inp, &mut outp, 64, 48_000.0),
            ProcessStatus::Continue
        ));
        for k in 0..64 {
            assert_eq!(
                oa[k].to_ne_bytes(),
                ia[k].to_ne_bytes(),
                "L passed bit-exact"
            );
            assert_eq!(
                ob[k], 0.0,
                "mono in, right out is silence, not invented audio"
            );
        }
    }

    #[test]
    fn ramp_lands_exactly_on_the_designed_target() {
        let mut p = eq_active(48_000.0);
        p.set_param_value(PARAM_BAND_BASE, 1.0); // LP
        p.set_param_value(PARAM_BAND_BASE + 1, 500.0);
        assert!(p.lanes[0].is_ramping(), "a live edit must ramp, never jump");
        let mut ia = vec![0.0f32; 64];
        let mut oa = vec![0.0f32; 64];
        for _ in 0..3 {
            let mut inp: [*mut f32; 1] = [ia.as_mut_ptr()];
            let mut outp: [*mut f32; 1] = [oa.as_mut_ptr()];
            drive(&mut p, &mut inp, &mut outp, 64, 48_000.0);
        }
        assert!(!p.lanes[0].is_ramping(), "192 frames > 128: ramp finished");
        assert_eq!(
            p.lanes[0].coeffs(),
            p.band_target(0),
            "closes on the exact target, not the sum of deltas"
        );
    }

    #[test]
    fn band_state_view_tracks_the_params() {
        let mut p = EqPlugin::instantiate();
        p.set_param_value(PARAM_BAND_BASE + 4 * 3, 6.0); // band 4 HighShelf
        p.set_param_value(PARAM_BAND_BASE + 4 * 3 + 1, 4_000.0);
        let st = p.band_state(3).expect("band 4");
        assert_eq!(st.type_ix, 6);
        assert_eq!(st.freq_hz, 4_000.0);
        assert_eq!(st.gain_db, 0.0);
        assert_eq!(st.q, 1.0);
        assert!(p.band_state(8).is_none());
        assert_eq!(TYPE_NAMES[st.type_ix], "HighShelf");
    }

    #[test]
    fn f64_rows_flow_through_the_scratch() {
        let mut p = eq_active(48_000.0);
        // Neutral chain: f32-representable doubles stay bit-exact through
        // f64 -> f32 -> identity lanes -> f64 — the casts lose nothing on
        // exactly representable values.
        let mut ia = [0.25f64; 64];
        let mut ib = [-0.125f64; 64];
        let mut oa = [-7.0f64; 64];
        let mut ob = [-7.0f64; 64];
        {
            let mut inp: [*mut f64; 2] = [ia.as_mut_ptr(), ib.as_mut_ptr()];
            let mut outp: [*mut f64; 2] = [oa.as_mut_ptr(), ob.as_mut_ptr()];
            assert_eq!(
                drive64(&mut p, &mut inp, &mut outp, 64, 48_000.0),
                ProcessStatus::Continue
            );
        }
        assert_eq!(&oa[..], &ia[..], "neutral 64-bit chain bit-exact on L");
        assert_eq!(&ob[..], &ib[..], "...and on R");
        // A live band at double width: same lane math, reached through the
        // scratch. Same block-exact tone and threshold as the f32 test.
        p.set_param_value(PARAM_BAND_BASE, 1.0); // LowPass
        p.set_param_value(PARAM_BAND_BASE + 1, 1_000.0);
        p.set_param_value(PARAM_BAND_BASE + 3, 0.707);
        let mut hot = sine64(5_250.0, 48_000.0, 64, 0);
        let mut z = [0.0f64; 64];
        let mut zout = [0.0f64; 64];
        let mut out = [0.0f64; 64];
        for _ in 0..64 {
            let mut inp: [*mut f64; 2] = [hot.as_mut_ptr(), z.as_mut_ptr()];
            let mut outp: [*mut f64; 2] = [out.as_mut_ptr(), zout.as_mut_ptr()];
            drive64(&mut p, &mut inp, &mut outp, 64, 48_000.0);
        }
        let att = 20.0 * (rms64(&out) / rms64(&hot)).log10();
        assert!(
            att <= -20.0,
            "64-bit LP@1k on a 5250 Hz tone: measured {att} dB"
        );
        // Bypass freezes at 64-bit too: the host's bytes, untouched.
        p.set_param_value(0, 1.0);
        let mut t = sine64(5_250.0, 48_000.0, 64, 64);
        let mut bo = [1.0f64; 64];
        let mut bz = [1.0f64; 64];
        {
            let mut inp: [*mut f64; 2] = [t.as_mut_ptr(), z.as_mut_ptr()];
            let mut outp: [*mut f64; 2] = [bo.as_mut_ptr(), bz.as_mut_ptr()];
            assert_eq!(
                drive64(&mut p, &mut inp, &mut outp, 64, 48_000.0),
                ProcessStatus::Continue
            );
        }
        assert_eq!(&bo[..], &t[..], "bypass at 64-bit: host bytes, untouched");
        // Mono in at double width: the right row is silence, never leftovers.
        let mut p2 = eq_active(48_000.0);
        let mut m = [0.25f64; 64];
        let mut mo0 = [0.0f64; 64];
        let mut mo1 = [9.0f64; 64];
        {
            let mut inp: [*mut f64; 1] = [m.as_mut_ptr()];
            let mut outp: [*mut f64; 2] = [mo0.as_mut_ptr(), mo1.as_mut_ptr()];
            drive64(&mut p2, &mut inp, &mut outp, 64, 48_000.0);
        }
        assert_eq!(&mo0[..], &m[..]);
        assert!(
            mo1.iter().all(|v| *v == 0.0),
            "mono in, right out silence at either width — no invented audio"
        );
    }

    #[test]
    fn neutral_and_bypass_at_64_bit_never_touch_the_scratch() {
        // Ratified invariant (task-11): values f32 cannot represent must
        // ride the neutral and the bypassed 64-bit paths byte-for-byte. If
        // either branch ever detoured through the f32 scratch, THIS is the
        // test that fails — the 0.25-shaped test above cannot catch the
        // detour by construction.
        let hostile: Vec<f64> = (0..64)
            .map(|i| match i % 4 {
                0 => 1.0 / 3.0,
                1 => core::f64::consts::SQRT_2,
                2 => f64::from_bits(1), // smallest subnormal
                _ => -1e-300,
            })
            .collect();
        let rounded: Vec<f64> = hostile.iter().map(|&v| v as f32 as f64).collect();
        assert_ne!(
            hostile, rounded,
            "the fixture really is unrepresentable in f32"
        );

        let bytes = |v: &[f64]| v.iter().map(|x| x.to_ne_bytes()).collect::<Vec<_>>();
        let mut p = eq_active(48_000.0);
        assert!(
            p.neutral_fast,
            "a fresh neutral instance carries the fast flag"
        );
        let mut in0 = hostile.clone();
        let mut in1 = hostile.clone();
        let mut o0 = vec![0.0f64; 64];
        let mut o1 = vec![0.0f64; 64];
        {
            let mut ip: [*mut f64; 2] = [in0.as_mut_ptr(), in1.as_mut_ptr()];
            let mut op: [*mut f64; 2] = [o0.as_mut_ptr(), o1.as_mut_ptr()];
            assert_eq!(
                drive64(&mut p, &mut ip, &mut op, 64, 48_000.0),
                ProcessStatus::Continue
            );
        }
        assert_eq!(
            bytes(&o0),
            bytes(&hostile),
            "neutral f64: byte-exact, scratch NOT on the way"
        );
        assert_eq!(bytes(&o1), bytes(&hostile));

        // Bypass after an engaged stretch: the same bytes while bypassed.
        p.set_param_value(PARAM_BAND_BASE, 1.0); // LowPass engages
        p.set_param_value(PARAM_BAND_BASE + 1, 1_000.0);
        assert!(!p.neutral_fast, "engaging clears the flag before any copy");
        p.set_param_value(0, 1.0); // bypass
        {
            let mut ip: [*mut f64; 2] = [in0.as_mut_ptr(), in1.as_mut_ptr()];
            let mut op: [*mut f64; 2] = [o0.as_mut_ptr(), o1.as_mut_ptr()];
            drive64(&mut p, &mut ip, &mut op, 64, 48_000.0);
        }
        assert_eq!(bytes(&o0), bytes(&hostile), "bypassed f64: same invariant");

        // Honest hot path when un-bypassed: a hostile-scaled tone — the
        // scratch detour does round its samples (the declared cost of f64),
        // but the filter must BITE >= 20 dB: proof the copy branch above is
        // not the filter pretending to pass.
        p.set_param_value(0, 0.0);
        let mut tone: Vec<f64> = sine64(5_250.0, 48_000.0, 64, 0)
            .iter()
            .map(|v| v / 3.0)
            .collect();
        let mut tout = vec![0.0f64; 64];
        let mut z = vec![0.0f64; 64];
        let mut zout = vec![0.0f64; 64];
        for _ in 0..64 {
            let mut ip: [*mut f64; 2] = [tone.as_mut_ptr(), z.as_mut_ptr()];
            let mut op: [*mut f64; 2] = [tout.as_mut_ptr(), zout.as_mut_ptr()];
            drive64(&mut p, &mut ip, &mut op, 64, 48_000.0);
        }
        let att = 20.0 * (rms64(&tout) / rms64(&tone)).log10();
        assert!(
            att <= -20.0,
            "hot f64 LP@1k on a hostile-scaled 5250 Hz tone: {att} dB"
        );
    }

    #[test]
    fn mixed_or_broken_buffer_widths_are_refused() {
        let mut p = eq_active(48_000.0);
        // f64 in, f32 out: a mixed-width pair is a spec violation. Refuse
        // BEFORE touching either array.
        let mut ia = [0.25f64; 64];
        let mut oa = [3.0f32; 64];
        let mut i64p: [*mut f64; 1] = [ia.as_mut_ptr()];
        let mut o32p: [*mut f32; 1] = [oa.as_mut_ptr()];
        let in_buf = clap_audio_buffer {
            data32: core::ptr::null_mut(),
            data64: i64p.as_mut_ptr(),
            channel_count: 1,
            latency: 0,
            constant_mask: 0,
        };
        let mut out_buf = clap_audio_buffer {
            data32: o32p.as_mut_ptr(),
            data64: core::ptr::null_mut(),
            channel_count: 1,
            latency: 0,
            constant_mask: 0,
        };
        let proc = clap_process {
            steady_time: 0,
            frames_count: 64,
            transport: core::ptr::null(),
            audio_inputs: &raw const in_buf,
            audio_outputs: &raw mut out_buf,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: core::ptr::null(),
            out_events: core::ptr::null(),
        };
        // SAFETY: our own well-formed buffers, alive for the whole call —
        // the test plays the (malformed) host honestly.
        let mut view = unsafe { ProcessView::from_sys(&proc, 48_000.0) };
        assert_eq!(p.process(&mut view), ProcessStatus::Error);
        assert_eq!(&oa[..], &[3.0f32; 64][..], "a refused block writes nothing");
        // XOR violated (both row arrays non-null): same refusal.
        let mut dummy32: [*mut f32; 1] = [oa.as_mut_ptr()];
        let in_buf2 = clap_audio_buffer {
            data32: dummy32.as_mut_ptr(),
            data64: i64p.as_mut_ptr(),
            channel_count: 1,
            latency: 0,
            constant_mask: 0,
        };
        let proc2 = clap_process {
            audio_inputs: &raw const in_buf2,
            ..proc
        };
        // SAFETY: same live buffers.
        let mut view2 = unsafe { ProcessView::from_sys(&proc2, 48_000.0) };
        assert_eq!(p.process(&mut view2), ProcessStatus::Error);
    }
}
