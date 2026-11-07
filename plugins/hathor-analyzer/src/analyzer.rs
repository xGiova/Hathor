//! The analyzer plugin: [`AnalyzerPlugin`] implements the wrapper's safe
//! [`Plugin`](hathor_clap::Plugin) trait over [`FftBank`].
//!
//! ## Raw pointers here: none
//!
//! This module is pure safe Rust. Callback buffers arrive through
//! [`ProcessView`](hathor_clap::ProcessView), whose slices are reborrowed from
//! host-owned arrays (the wrapper owns that argument; see its module doc). The
//! bank lives only between `activate` and `deactivate`; the mono scratch is
//! sized at `activate` — `process` touches no allocation, by construction, and
//! the integration tests assert it under the workspace `AudioSection`.
//!
//! ## The rebuild decision (labeled)
//!
//! [`FftBank::new`] is the bank's only allocation site, so the plugin rebuilds
//! it **exactly in `activate`** (main-thread, outside any deadline). A param
//! changed while active is stored, remembered, and applied at the next
//! activate-cycle; a live swap would need double-buffering and buys nothing in
//! v0 (a host automating smoothing constants mid-take is not a scenario the
//! M1 roadmap needs). Decision reported; change it only with measurement.
//!
//! ## State doctrine
//!
//! 72 bytes: the 9 params, little-endian f64. Smoothing/hold/ring contents are
//! transient by CONVENTION §8.2 — reloaded projects rebuild them by listening,
//! exactly like SPAN does after a reload.

use core::ffi::CStr;

use hathor_clap::sys::ext::params::CLAP_PARAM_IS_AUTOMATABLE;
use hathor_clap::sys::plugin::clap_plugin_descriptor;
use hathor_clap::{
    clap_cstr, make_descriptor, AudioPortInfo, FeatureList, Host, InputStream, LogSeverity,
    OutputStream, ParamInfo, ParamText, Plugin, ProcessStatus, ProcessView, SampleFormat,
};

use hathor_dsp_fft::bank::{BankParams, FftBank, TierTuning, DEFAULT_BINS};

/// Param ids, in `param_info` order (hosts show this list verbatim).
pub const PARAM_FLOOR_DB: u32 = 0;
/// Average time per tier (ms), tier order 256/1024/4096/16384.
pub const PARAM_AVG_TIERS: [u32; 4] = [1, 2, 3, 4];
/// Peak-hold decay per tier (dB/s), same tier order.
pub const PARAM_DECAY_TIERS: [u32; 4] = [5, 6, 7, 8];
/// Total param count.
pub const PARAM_COUNT: usize = 9;

/// Range/name metadata for one param (also our clamp, so the bank's setup
/// asserts can never be reached through a hostile value).
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

/// The normative v0 param table. The `Smoothing` module groups the 8 tier
/// tunings; `floor` belongs to `Display` (it only affects `render_db`, never
/// `process`).
pub const SPECS: [ParamSpec; PARAM_COUNT] = [
    ParamSpec {
        name: "Floor",
        module: "Display",
        min: -144.0,
        max: -20.0,
    },
    ParamSpec {
        name: "Avg 256",
        module: "Smoothing",
        min: 0.0,
        max: 2000.0,
    },
    ParamSpec {
        name: "Avg 1024",
        module: "Smoothing",
        min: 0.0,
        max: 2000.0,
    },
    ParamSpec {
        name: "Avg 4096",
        module: "Smoothing",
        min: 0.0,
        max: 2000.0,
    },
    ParamSpec {
        name: "Avg 16384",
        module: "Smoothing",
        min: 0.0,
        max: 2000.0,
    },
    ParamSpec {
        name: "Decay 256",
        module: "Smoothing",
        min: 0.0,
        max: 96.0,
    },
    ParamSpec {
        name: "Decay 1024",
        module: "Smoothing",
        min: 0.0,
        max: 96.0,
    },
    ParamSpec {
        name: "Decay 4096",
        module: "Smoothing",
        min: 0.0,
        max: 96.0,
    },
    ParamSpec {
        name: "Decay 16384",
        module: "Smoothing",
        min: 0.0,
        max: 96.0,
    },
];

/// Defaults mirror `BankParams::new(48_000)` exactly — asserted by test, so a
/// bank-profile change cannot silently diverge from the plugin's face.
fn default_params() -> [f64; PARAM_COUNT] {
    let bp = BankParams::new(48_000.0);
    let mut p = [0.0; PARAM_COUNT];
    p[0] = bp.floor_db as f64;
    for t in 0..4 {
        p[1 + t] = bp.tuning[t].avg_time_s as f64 * 1000.0;
        p[5 + t] = bp.tuning[t].decay_db_per_s as f64;
    }
    p
}

/// The analyzer instance. One per host `create_plugin`; the wrapper owns the
/// box, this type owns no pointer at all.
pub struct AnalyzerPlugin {
    host: Option<Host>,
    params: [f64; PARAM_COUNT],
    /// Sized at `activate` to the host's max block; `process` never grows it.
    mono: Vec<f32>,
    /// Some between successful `activate` and `deactivate`.
    bank: Option<FftBank>,
    active: bool,
}

impl AnalyzerPlugin {
    fn spec_index(id: u32) -> Option<usize> {
        usize::try_from(id).ok().filter(|i| *i < PARAM_COUNT)
    }

    /// The current params as the bank wants them at `rate`.
    fn bank_params(&self, rate: f32) -> BankParams {
        let tuning = core::array::from_fn(|t| TierTuning {
            avg_time_s: (self.params[1 + t] / 1000.0) as f32,
            decay_db_per_s: self.params[5 + t] as f32,
        });
        BankParams {
            sample_rate: rate,
            bins: DEFAULT_BINS,
            floor_db: self.params[0] as f32,
            tuning,
        }
    }

    /// 72-byte little-endian blob of the 9 params (the whole saved state).
    fn serialize(&self) -> [u8; 72] {
        let mut b = [0u8; 72];
        for i in 0..PARAM_COUNT {
            b[i * 8..i * 8 + 8].copy_from_slice(&self.params[i].to_le_bytes());
        }
        b
    }

    fn deserialize(&mut self, b: &[u8; 72]) {
        for i in 0..PARAM_COUNT {
            let mut g = [0u8; 8];
            g.copy_from_slice(&b[i * 8..i * 8 + 8]);
            self.params[i] = sanitize(f64::from_le_bytes(g), i);
        }
    }

    // ---- display views, design-pending (M1 UI not yet specified) ----
    // Nothing reads them in v0; they exist so the future display layer can use
    // the SAME bank instance the audio thread feeds, without a second one.

    /// Linear power per shared log bin (audio-thread output of the bank).
    pub fn levels(&self) -> &[f32] {
        self.bank.as_ref().map_or(&[], FftBank::levels)
    }

    /// Convert `levels()` to dB on the display thread (a `log10` pass the
    /// bank deliberately keeps out of `process`).
    pub fn render_db(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.render_db();
        }
    }

    /// The dB buffer filled by [`Self::render_db`].
    pub fn db(&self) -> &[f32] {
        self.bank.as_ref().map_or(&[], FftBank::db)
    }

    /// Center frequency of shared log bin `i` (display helper, `powf` cost).
    pub fn log_bin_freq(&self, i: usize) -> f32 {
        self.bank.as_ref().map_or(0.0, |b| b.log_bin_freq(i))
    }

    /// Current param values, in id order (diagnostics/tests).
    pub fn params_snapshot(&self) -> [f64; PARAM_COUNT] {
        self.params
    }

    /// The DSP core the host-facing [`Plugin::process`] adapter calls after its
    /// pass-through: guards, downmix into the `activate`-sized scratch, feed
    /// the bank. Allocation-free, plain slices only. The tests drive the full
    /// adapter (this plus the pass-through) through a fabricated
    /// `ProcessView` — `from_sys` is public since task-5 — so there is exactly
    /// one audio path under test, the host one.
    fn downmix_and_analyze(&mut self, l: &[f32], r: Option<&[f32]>) -> ProcessStatus {
        if !self.active {
            return ProcessStatus::Error;
        }
        let frames = l.len();
        if frames == 0 {
            return ProcessStatus::Continue;
        }
        if frames > self.mono.len() {
            // Host exceeded the max block promised at `activate` (spec
            // violation; hosts expected NOT VERIFIED): fail honestly rather
            // than write past the scratch.
            return ProcessStatus::Error;
        }
        if r.is_some_and(|x| x.len() != frames) {
            return ProcessStatus::Error;
        }
        match r {
            Some(rr) => {
                for (m, (&x, &y)) in self.mono[..frames].iter_mut().zip(l.iter().zip(rr)) {
                    *m = 0.5 * (x + y);
                }
            }
            None => self.mono[..frames].copy_from_slice(l),
        }
        self.analyze_mono(frames)
    }

    /// 64-bit sibling of [`Self::downmix_and_analyze`]: the stereo add happens
    /// in f64 — host precision kept as long as it can be — and the single
    /// narrowing cast lands in the scratch. The bank is f32 by design (the
    /// task-4 decision); 24-bit audio is the real precision floor anyway, so
    /// this cast loses nothing below the noise of the format.
    fn downmix64_and_analyze(&mut self, l: &[f64], r: Option<&[f64]>) -> ProcessStatus {
        if !self.active {
            return ProcessStatus::Error;
        }
        let frames = l.len();
        if frames == 0 {
            return ProcessStatus::Continue;
        }
        if frames > self.mono.len() {
            return ProcessStatus::Error;
        }
        if r.is_some_and(|x| x.len() != frames) {
            return ProcessStatus::Error;
        }
        match r {
            Some(rr) => {
                for (m, (&x, &y)) in self.mono[..frames].iter_mut().zip(l.iter().zip(rr)) {
                    *m = (0.5 * (x + y)) as f32;
                }
            }
            None => {
                for (m, &x) in self.mono[..frames].iter_mut().zip(l) {
                    *m = x as f32;
                }
            }
        }
        self.analyze_mono(frames)
    }

    /// The shared tail: feed the bank exactly the scratch prefix, allocation-
    /// free. Both width paths arrive here — one analysis core, two wires.
    fn analyze_mono(&mut self, frames: usize) -> ProcessStatus {
        let Some(bank) = &mut self.bank else {
            return ProcessStatus::Error;
        };
        bank.process(&self.mono[..frames]);
        ProcessStatus::Continue
    }

    /// The f32 world: unchanged from task-4's shipping path.
    fn run32(&mut self, io: &mut ProcessView<'_>, in_ch: usize, out_ch: usize) -> ProcessStatus {
        // Input rows first: `&'a` slices that outlive further borrows of `io`.
        let Some(l) = io.input_channel(0, 0) else {
            return ProcessStatus::Error;
        };
        let r = if in_ch >= 2 {
            io.input_channel(0, 1)
        } else {
            None
        };
        // In-place pair (we advertise it): the output rows ARE the input rows
        // — touching them would alter the sound we promise to leave alone, so
        // pass-through becomes the no-op the host already performed for us.
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
                    None => o1.fill(0.0),
                }
            }
        }
        self.downmix_and_analyze(l, r)
    }

    /// The f64 world: the same pass-through at double width, bit-exact both
    /// ways (a `copy_from_slice` of doubles moves the host's own bytes).
    fn run64(&mut self, io: &mut ProcessView<'_>, in_ch: usize, out_ch: usize) -> ProcessStatus {
        let Some(l) = io.input_channel_f64(0, 0) else {
            return ProcessStatus::Error;
        };
        let r = if in_ch >= 2 {
            io.input_channel_f64(0, 1)
        } else {
            None
        };
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
        self.downmix64_and_analyze(l, r)
    }
}

/// Clamp into the spec range; a non-finite value from a misbehaving host
/// resets that param to its default (never NaN into the bank's setup asserts).
fn sanitize(value: f64, index: usize) -> f64 {
    let spec = SPECS[index];
    if value.is_finite() {
        value.clamp(spec.min, spec.max)
    } else {
        default_params()[index]
    }
}

/// The plugin identity, PINNED FOREVER where it matters: `id` is persisted by
/// hosts into saved projects, so renaming "dev.hathor.analyzer" breaks every
/// session that carries it — the byte-for-byte test in `tests/lifecycle.rs`
/// is the tripwire that makes such a rename loud, never silent.
///
/// URL fields: the GitHub repo is the only truthful address today; the README
/// IS the manual for now, so `manual_url`/`support_url` reuse it rather than
/// deep-linking docs that do not exist yet.
///
/// Features (clap-sys `plugin_features` constants, the spec's documented
/// strings): audio effect + analyzer. Host-side handling of the list is
/// expected-per-spec, NOT VERIFIED until the owner's Reaper scan.
static DESCRIPTOR: clap_plugin_descriptor = make_descriptor(
    clap_cstr!("dev.hathor.analyzer"),
    clap_cstr!("Hathor Analyzer"),
    clap_cstr!("Hathor"),
    clap_cstr!("https://github.com/xGiova/Hathor"),
    clap_cstr!("https://github.com/xGiova/Hathor"),
    clap_cstr!("https://github.com/xGiova/Hathor"),
    // Bump with the milestone tags (repo tag v0.1.0); the workspace crate
    // version stays a 0.0.0 placeholder by lead decision — this string is the
    // host-visible plugin version, not cargo's.
    clap_cstr!("0.1.0"),
    clap_cstr!("Multi-resolution spectrum analyzer (headless v0; GUI pending design)."),
    Some(FeatureList::new(&[
        hathor_clap::sys::plugin_features::CLAP_PLUGIN_FEATURE_AUDIO_EFFECT.as_ptr(),
        hathor_clap::sys::plugin_features::CLAP_PLUGIN_FEATURE_ANALYZER.as_ptr(),
        core::ptr::null(),
    ])),
);

impl Plugin for AnalyzerPlugin {
    const DESCRIPTOR: &'static clap_plugin_descriptor = &DESCRIPTOR;

    fn instantiate() -> Self {
        Self {
            host: None,
            params: default_params(),
            mono: Vec::new(),
            bank: None,
            active: false,
        }
    }

    fn init(&mut self, host: Host) -> bool {
        let logged = host.log(LogSeverity::Info, clap_cstr!("hathor-analyzer init"));
        self.host = Some(host);
        // A host without `clap.log` is still a valid host: log failure is not
        // an init failure.
        let _ = logged;
        true
    }

    fn activate(&mut self, sample_rate: f64, _min_frames: u32, max_frames: u32) -> bool {
        // The bank's declared operating range — refuse honestly rather than
        // let `FftBank::new` panic inside a host callback.
        if !(44_100.0..=192_000.0).contains(&sample_rate) {
            return false;
        }
        // v0 assumes the bank's axis is defined for the advertised 20 Hz–24 kHz
        // range and a sane max block; a 0-frame or absurd max is a hostile
        // answer, refuse it too.
        if max_frames == 0 || max_frames > 1 << 20 {
            return false;
        }
        let params = self.bank_params(sample_rate as f32);
        self.bank = Some(FftBank::new(&params));
        // Setup allocation, declared (the bank itself is a far larger one):
        self.mono = vec![0.0; max_frames as usize];
        self.active = true;
        true
    }

    fn deactivate(&mut self) {
        self.bank = None;
        self.mono = Vec::new();
        self.active = false;
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    /// Thin adapter: pull the CLAP rows, run the pass-through (row by row —
    /// the view hands out one mutable row at a time), then the shared
    /// [`Self::analyze_mono`] core. This is THE audio path: the spectrum
    /// tests fabricate a `clap_process` and drive exactly this.
    ///
    /// Both wire widths are served, chosen from each port's pointer pair
    /// (`channel_format`): f32 rows run the shipping path; f64 rows pass
    /// through bit-exact and downmix in f64 before one cast. A port that
    /// breaks the XOR, or in/out ports of different widths, is refused
    /// (Error): the plugin never guesses what a malformed host meant.
    fn process(&mut self, io: &mut ProcessView<'_>) -> ProcessStatus {
        if !self.active {
            return ProcessStatus::Error;
        }
        if io.frames() > self.mono.len() {
            // Host exceeded the max block promised at `activate` (spec
            // violation; hosts expected NOT VERIFIED): refuse BEFORE writing
            // anything, so a failed block is at least an untouched block.
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
        // One main stereo port each way, declared in-place pairs: v0 has 2ch
        // for both directions and pass-through supports sharing buffers.
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
        let i = Self::spec_index(index)?;
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
        Self::spec_index(id).map(|i| self.params[i])
    }

    fn set_param_value(&mut self, id: u32, value: f64) {
        let Some(i) = Self::spec_index(id) else {
            return; // unknown id: ignore per CLAP, never guess
        };
        self.params[i] = sanitize(value, i);
        // Takes effect at the next activate (documented decision above). Tell
        // the host the project changed: this is a saved-state-visible edit.
        if let Some(h) = self.host {
            h.state_mark_dirty();
        }
    }

    fn param_text(&self, id: u32, value: f64, out: &mut ParamText<'_>) -> bool {
        if Self::spec_index(id).is_none() {
            return false;
        }
        out.write_f64(value)
    }

    fn param_from_text(&self, _id: u32, text: &CStr) -> Option<f64> {
        // The display format is plain decimal (see `ParamText::write_f64`), so
        // its parse is the honest inverse; anything else is refused.
        text.to_str().ok()?.parse::<f64>().ok()
    }

    // ---- state ----

    fn save_state(&mut self, out: &mut OutputStream<'_>) -> bool {
        let blob = self.serialize();
        out.write(&blob)
    }

    fn load_state(&mut self, input: &mut InputStream<'_>) -> bool {
        let mut blob = [0u8; 72];
        if !input.read_exact(&mut blob) {
            return false;
        }
        self.deserialize(&blob);
        true
    }

    // ---- latency: zero, declared (the bank shows the last full frame; a
    // block of delay is inherent to any STFT display, not audio path latency —
    // the analyzer path never delays the audio, pass-through is same-block). ----

    fn latency(&self) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hathor_clap::sys::audio_buffer::clap_audio_buffer;
    use hathor_clap::sys::process::clap_process;

    /// Drive the REAL `Plugin::process` adapter: fabricate a well-formed
    /// `clap_process` (the host's guarantees, emulated) and run the view over
    /// it — exactly the object a host hands the wrapper during a callback.
    pub(super) fn drive(
        p: &mut AnalyzerPlugin,
        in_ptrs: &mut [*mut f32],
        out_ptrs: &mut [*mut f32],
        frames: u32,
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
        // the row arrays sit in the callers' live buffers, the channel counts
        // match the array lengths, `frames` matches the slices the rows back.
        let mut view = unsafe { ProcessView::from_sys(&proc, 48_000.0) };
        p.process(&mut view)
    }

    #[test]
    fn defaults_mirror_the_bank_profile_exactly() {
        let bp = BankParams::new(48_000.0);
        let p = default_params();
        assert_eq!(p[0], bp.floor_db as f64);
        for t in 0..4 {
            assert_eq!(p[1 + t], bp.tuning[t].avg_time_s as f64 * 1000.0);
            assert_eq!(p[5 + t], bp.tuning[t].decay_db_per_s as f64);
        }
    }

    #[test]
    fn default_params_rebuild_the_default_bank_bit_exact() {
        // The mapping round trip must be lossless, or the plugin's default
        // face is not the bank's default face.
        let plugin = AnalyzerPlugin::instantiate();
        assert_eq!(plugin.bank_params(48_000.0), BankParams::new(48_000.0));
    }

    #[test]
    fn hostile_values_are_clamped_or_reset_never_nan() {
        let mut p = AnalyzerPlugin::instantiate();
        p.set_param_value(1, f64::MAX);
        assert_eq!(p.params_snapshot()[1], SPECS[1].max);
        p.set_param_value(1, f64::NAN);
        assert_eq!(p.params_snapshot()[1], default_params()[1]);
        p.set_param_value(1, -5.0);
        assert_eq!(p.params_snapshot()[1], SPECS[1].min);
        p.set_param_value(99, 0.5); // unknown id: ignored
        assert_eq!(p.param_value(99), None);
    }

    #[test]
    fn state_codec_round_trips_and_clamps() {
        let mut p = AnalyzerPlugin::instantiate();
        p.set_param_value(0, -96.0);
        p.set_param_value(7, 42.5);
        let blob = p.serialize();
        let mut q = AnalyzerPlugin::instantiate();
        q.deserialize(&blob);
        assert_eq!(p.params_snapshot(), q.params_snapshot());
        // garbage in: every field clamped, no panic (72 bytes of ones decode
        // to large negative f64s except the last — the clamp is the contract).
        q.deserialize(&[0x01; 72]);
        for (i, &v) in q.params_snapshot().iter().enumerate() {
            assert!(v >= SPECS[i].min && v <= SPECS[i].max, "param {i} escaped");
        }
    }

    #[test]
    fn activate_refuses_unsupported_rates_and_frame_sizes() {
        let mut p = AnalyzerPlugin::instantiate();
        assert!(!p.activate(8_000.0, 8, 64), "below the bank range");
        assert!(!p.activate(200_000.0, 8, 64), "above the bank range");
        assert!(!p.activate(48_000.0, 0, 0), "zero max block");
        assert!(p.activate(48_000.0, 16, 4096));
        p.deactivate();
    }

    #[test]
    fn param_specs_and_ids_are_consistent() {
        assert_eq!(SPECS.len(), PARAM_COUNT);
        for (i, s) in SPECS.iter().enumerate() {
            assert!(s.min < s.max, "spec {i} has an empty range");
            let d = default_params()[i];
            assert!(d >= s.min && d <= s.max, "default {i} outside its range");
        }
        assert_eq!(PARAM_AVG_TIERS, [1, 2, 3, 4]);
        assert_eq!(PARAM_DECAY_TIERS, [5, 6, 7, 8]);
        let p = AnalyzerPlugin::instantiate();
        assert_eq!(p.param_count() as usize, PARAM_COUNT);
        assert!(p.param_info(PARAM_COUNT as u32).is_none());
        assert_eq!(p.log_bin_freq(0), 0.0, "no bank yet");
        assert!(p.levels().is_empty() && p.db().is_empty());
    }

    #[test]
    fn mono_in_copies_to_left_zeroes_right_and_still_drives_the_bank() {
        let mut p = AnalyzerPlugin::instantiate();
        for id in 1..5 {
            p.set_param_value(id, 0.0);
        }
        for id in 5..9 {
            p.set_param_value(id, 0.0);
        }
        assert!(p.activate(48_000.0, 16, 64));
        // 984.375 Hz in the LEFT channel only: if the downmix wrongly picked
        // silence (or the right channel), the bank stays flat.
        let tone = |base: usize| -> [f32; 64] {
            core::array::from_fn(|i| {
                (std::f64::consts::TAU * 984.375 * (base + i) as f64 / 48_000.0).sin() as f32 * 0.5
            })
        };
        for b in 0..24 {
            let mut l = tone(b * 64);
            let keep = l;
            let mut o0 = [7.0f32; 64];
            let mut o1 = [7.0f32; 64];
            let mut in_ptrs = [l.as_mut_ptr()];
            let mut out_ptrs = [o0.as_mut_ptr(), o1.as_mut_ptr()];
            assert!(matches!(
                drive(&mut p, &mut in_ptrs, &mut out_ptrs, 64),
                ProcessStatus::Continue
            ));
            assert_eq!(o0, keep, "mono in: left passes through");
            assert!(
                o1.iter().all(|&x| x == 0.0),
                "stereo out, mono in: right goes silent"
            );
        }
        assert!(
            p.levels().iter().any(|&x| x > 0.0),
            "the lone channel is the downmix, not silence"
        );
    }

    #[test]
    fn oversized_block_is_refused_before_any_write() {
        let mut p = AnalyzerPlugin::instantiate();
        assert!(p.activate(48_000.0, 16, 64));
        let mut big = [0.5f32; 65];
        let mut o0 = [7.0f32; 65];
        let mut in_ptrs = [big.as_mut_ptr()];
        let mut out_ptrs = [o0.as_mut_ptr()];
        // 65 frames > the 64-frame scratch promised at `activate`.
        assert!(matches!(
            drive(&mut p, &mut in_ptrs, &mut out_ptrs, 65),
            ProcessStatus::Error
        ));
        assert!(
            o0.iter().all(|&x| x == 7.0),
            "the failed block touched nothing"
        );
    }
}
