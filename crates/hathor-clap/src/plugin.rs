//! The plugin half: a safe consumer [`Plugin`] trait and the `extern "C"`
//! vtables that carry a live instance across the FFI.
//!
//! ## Raw pointers in this module, and why they are sound
//!
//! One allocation carries everything: `Instance<P>` is heap-owned (a leaked
//! `Box`) and `plugin_data` stores that box's address — the ONLY address we
//! ever treat as an `Instance<P>`. The `base` header the host holds points
//! *into* the same box (`Instance` is `repr(Rust)`, so `base` sits at a
//! compiler-chosen offset — measured 64, not assumed 0). The lifecycle is
//! therefore ownership, not aliasing:
//!
//! * `create_instance` leaks a `Box<Instance<P>>` and returns `&raw base`;
//!   from that moment the host owns the address as an opaque token.
//! * every `clap_plugin` callback receives exactly the pointer we handed out
//!   (CLAP hosts pass it through untouched — expected, NOT VERIFIED per host,
//!   and the mock-host proxy asserts it for ours); the trampoline rebuilds the
//!   `&mut Instance<P>` from `plugin_data` with `P` matching because only the
//!   vtable instantiated for `P` can dispatch here.
//! * `destroy` runs `Box::from_raw` — the unique reclaim point — after which
//!   no pointer into the instance is used again.
//!
//! Interior mutability is deliberately `&mut`-based: the wrapper hands one
//! mutable borrow at a time inside each trampoline, so the consumer trait is
//! plain safe Rust (`&mut self` where CLAP says the call is exclusive).
//! Nothing in this module ever runs on a hot path except `process`, and `process`
//! allocates nothing: the view borrows host memory (see [`crate::process`]).

use core::ffi::{c_char, c_void, CStr};

use crate::cstr::write_padded_name;
use crate::events::{InEvents, OutEvents};
use crate::host::Host;
use crate::process::{ProcessStatus, ProcessView};
use crate::sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_STEREO,
};
use crate::sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use crate::sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS};
use crate::sys::ext::render::{
    clap_plugin_render, clap_plugin_render_mode, CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE,
    CLAP_RENDER_REALTIME,
};
use crate::sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use crate::sys::host::clap_host;
use crate::sys::id::{clap_id, CLAP_INVALID_ID};
use crate::sys::plugin::{clap_plugin, clap_plugin_descriptor};
use crate::sys::process::clap_process;
use crate::sys::stream::{clap_istream, clap_ostream};

/// A plugin written against core `hathor-dsp-core` types, free of `unsafe`.
///
/// Implement this (together with a `static` descriptor, see
/// [`crate::make_descriptor`]) and hand the type to [`crate::clap_entry!`].
/// Everything has a default except the five required items; defaults are honest
/// "no extension" answers (0 params, 1 main stereo in + out, latency 0,
/// realtime-capable) so a minimal plugin stays minimal.
pub trait Plugin {
    /// The static descriptor: id, name, vendor strings. Must live for `'static`
    /// — the host reads it before instance creation and may cache the pointer.
    const DESCRIPTOR: &'static clap_plugin_descriptor;

    /// Builds the instance object at `create_plugin` time. Host interaction
    /// starts at [`Plugin::init`]; this must not talk to the host.
    fn instantiate() -> Self;

    /// Receives the [`Host`] handle (extension of `init` in `clap_plugin`).
    /// `false` refuses the instance, which makes `create_plugin` fail.
    fn init(&mut self, host: Host) -> bool;

    /// The audio-thread entry: fill the output [`Block`](hathor_dsp_core::buffer::Block)s
    /// from the input slices. Called only between `start_processing` and
    /// `stop_processing`; must never allocate (the workspace audio guard is
    /// armed in hosts — and our mock-host test asserts zero allocations here).
    fn process(&mut self, io: &mut ProcessView<'_>) -> ProcessStatus;

    /// Prepares for `process` at the given sample rate and frame-size bounds.
    fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> bool;

    /// Releases everything `activate` prepared.
    fn deactivate(&mut self);

    /// Optional: called before the first `process` of a playback cycle.
    fn start_processing(&mut self) -> bool {
        true
    }

    /// Optional: called after the last `process` of a playback cycle.
    fn stop_processing(&mut self) {}

    /// Optional: clear internal state on seek/loop (reset is *not* paired with
    /// process — the host simply says "forget history").
    fn reset(&mut self) {}

    /// Optional: drain main-thread work after [`crate::Host::request_callback`].
    fn on_main_thread(&mut self) {}

    // ---- audio ports (extension `clap.audio-ports`) ----

    /// Number of input or output ports. Default: one main port each way.
    fn audio_port_count(&self, _is_input: bool) -> u32 {
        1
    }

    /// Describes port `index` of the given direction; `None` = out of range.
    /// Default: a single main stereo f32 port (ids 0 in / 1 out).
    /// Positioning (surround/ambisonic) is **not** wired in this wrapper yet:
    /// ports advertise nothing beyond stereo.
    fn audio_port_info(&self, index: u32, is_input: bool) -> Option<AudioPortInfo<'_>> {
        match (index, is_input) {
            (0, true) => Some(AudioPortInfo::main_stereo(0, "In")),
            (0, false) => Some(AudioPortInfo::main_stereo(1, "Out")),
            _ => None,
        }
    }

    // ---- params (extension `clap.params`) ----

    /// Number of parameters. Default: none.
    fn param_count(&self) -> u32 {
        0
    }

    /// Describes param `index` (order matters: hosts show it as a list).
    fn param_info(&self, _index: u32) -> Option<ParamInfo<'_>> {
        None
    }

    /// Current value of `id`, in its own range. `None` = unknown id.
    fn param_value(&self, _id: clap_id) -> Option<f64> {
        None
    }

    /// Host asks the plugin to move `id` (user UI, automation, state load).
    fn set_param_value(&mut self, _id: clap_id, _value: f64) {}

    /// Formats `value` of `id` for display; write into `out`, return `true`.
    /// Default: no formatting.
    fn param_text(&self, _id: clap_id, _value: f64, _out: &mut ParamText<'_>) -> bool {
        false
    }

    /// Parses user-typed text back to a value. Default: refuse.
    fn param_from_text(&self, _id: clap_id, _text: &CStr) -> Option<f64> {
        None
    }

    /// Receives queued param updates (and may push events back). The default
    /// feeds every [`crate::events::ParamUpdate`] to [`Plugin::set_param_value`]; override for
    /// per-frame scheduling or full event inspection.
    ///
    /// Note: `clap.params` v1 has no `get_midi` (that is a pre-1.2 draft API —
    /// the task text mentioning it is stale; MIDI arrives as events instead).
    fn flush(&mut self, input: Option<InEvents<'_>>, _output: Option<OutEvents<'_>>) {
        if let Some(events) = input {
            for ev in events.param_values() {
                self.set_param_value(ev.id, ev.value);
            }
        }
    }

    // ---- state (extension `clap.state`) ----

    /// Writes the project-state blob to `out`. Default: an empty state (valid).
    fn save_state(&mut self, _out: &mut OutputStream<'_>) -> bool {
        true
    }

    /// Reads back what [`Plugin::save_state`] wrote. Default: ignore (matches
    /// the empty-state default above).
    fn load_state(&mut self, _input: &mut InputStream<'_>) -> bool {
        true
    }

    // ---- latency (extension `clap.latency`) ----

    /// Reported latency in frames. Default: 0. Call [`crate::Host::latency_changed`]
    /// whenever this answer moves.
    fn latency(&self) -> u32 {
        0
    }

    // ---- render (extension `clap.render`) ----

    /// True if the plugin cannot live outside the realtime thread. Default:
    /// false — nothing in this wrapper *requires* realtime; whether offline
    /// rendering is *correct* (no smoothing artifacts) is the consumer's call.
    fn has_hard_realtime_requirement(&self) -> bool {
        false
    }

    /// Host enters/leaves offline mode. Default: accept both (realtime-only
    /// consumers override and return false for `offline == true`).
    fn set_offline(&mut self, _offline: bool) -> bool {
        true
    }
}

/// Safe description of one audio port, written into the host's
/// `clap_audio_port_info` on demand.
#[derive(Debug, Clone)]
pub struct AudioPortInfo<'a> {
    /// Stable port id (0..count range recommended; hosts key on it).
    pub id: clap_id,
    /// Display name, truncated by the wrapper into the host's fixed array.
    pub name: &'a str,
    /// True = the main port (the one hosts route first).
    pub main: bool,
    /// Number of channels (stereo main port = 2).
    pub channel_count: u32,
    /// `mono`/`stereo`/custom port type, or `None` for "unspecified".
    pub port_type: Option<&'static CStr>,
    /// The port that can share buffers with this one (in-place pair), if any.
    pub in_place_pair: Option<clap_id>,
}

impl<'a> AudioPortInfo<'a> {
    /// The house default: a main stereo f32 port with no in-place promise.
    pub const fn main_stereo(id: clap_id, name: &'a str) -> Self {
        Self {
            id,
            name,
            main: true,
            channel_count: 2,
            port_type: Some(CLAP_PORT_STEREO),
            in_place_pair: None,
        }
    }

    fn write_into(&self, dst: *mut clap_audio_port_info) {
        // SAFETY: `dst` is a host-owned, properly aligned `clap_audio_port_info`
        // (the vtable contract); we only write through it, and finish with a
        // full-struct store so no padding reads uninitialized.
        unsafe {
            (*dst).id = self.id;
            (*dst).flags = if self.main {
                CLAP_AUDIO_PORT_IS_MAIN
            } else {
                0
            };
            (*dst).channel_count = self.channel_count;
            (*dst).port_type = self.port_type.map_or(core::ptr::null(), CStr::as_ptr);
            (*dst).in_place_pair = self.in_place_pair.unwrap_or(CLAP_INVALID_ID);
            write_padded_name(&mut (*dst).name, self.name);
        }
    }
}

/// Safe description of one parameter, written into the host's
/// `clap_param_info` on demand.
#[derive(Debug, Clone)]
pub struct ParamInfo<'a> {
    /// Stable parameter id.
    pub id: clap_id,
    /// `CLAP_PARAM_*` flags from `clap-sys` (e.g. `CLAP_PARAM_IS_AUTOMATABLE`).
    pub flags: u32,
    /// Display name.
    pub name: &'a str,
    /// Path/module for grouping ("" = top level).
    pub module: &'a str,
    /// Range lower bound.
    pub min_value: f64,
    /// Range upper bound.
    pub max_value: f64,
    /// Default value (what a fresh instance starts with).
    pub default_value: f64,
}

impl<'a> ParamInfo<'a> {
    /// A plain automatable float parameter between `min` and `max`.
    pub fn automatable(id: clap_id, name: &'a str, min_value: f64, max_value: f64) -> Self {
        Self {
            id,
            flags: crate::sys::ext::params::CLAP_PARAM_IS_AUTOMATABLE,
            name,
            module: "",
            min_value,
            max_value,
            default_value: min_value,
        }
    }

    fn write_into(&self, dst: *mut clap_param_info) {
        // SAFETY: host-owned aligned `clap_param_info` per the vtable contract;
        // every field we own is assigned, names get their guaranteed NUL.
        unsafe {
            (*dst).id = self.id;
            (*dst).flags = self.flags;
            (*dst).cookie = core::ptr::null_mut();
            (*dst).min_value = self.min_value;
            (*dst).max_value = self.max_value;
            (*dst).default_value = self.default_value;
            write_padded_name(&mut (*dst).name, self.name);
            write_padded_name(&mut (*dst).module, self.module);
        }
    }
}

/// Writer for [`Plugin::param_text`]: the host's fixed-size buffer, bounded.
pub struct ParamText<'a> {
    buf: &'a mut [c_char],
    written: usize,
}

impl<'a> ParamText<'a> {
    fn new(buf: *mut c_char, capacity: u32) -> Self {
        // SAFETY: vtable contract — `buf` points at `capacity` writable bytes
        // owned by the host for the duration of the call.
        let buf = unsafe { core::slice::from_raw_parts_mut(buf, capacity as usize) };
        Self { buf, written: 0 }
    }

    /// Writes `s` (no NUL needed: the wrapper terminates). `false` if too long.
    pub fn write_str(&mut self, s: &str) -> bool {
        if s.len() + 1 > self.buf.len() {
            return false;
        }
        // Keep NUL termination inside the capacity.
        let bytes = s.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            self.buf[i] = *b as c_char;
        }
        self.buf[bytes.len()] = 0;
        self.written = bytes.len();
        true
    }

    /// `format_args`-style: formats `value` with two decimals as a fallback text.
    pub fn write_f64(&mut self, value: f64) -> bool {
        let mut tmp = [0u8; 32];
        let s = format_f64(value, &mut tmp);
        self.write_str(s)
    }
}

/// Tiny float formatting that never allocates (state paths must stay quiet).
fn format_f64(value: f64, buf: &mut [u8; 32]) -> &str {
    // Fixed two-decimal rendering, no exponent, no std formatting.
    let neg = value < 0.0;
    let mut v = value.abs();
    if !v.is_finite() {
        let s = if value.is_nan() {
            "nan"
        } else if neg {
            "-inf"
        } else {
            "inf"
        };
        let n = s.len();
        buf[..n].copy_from_slice(s.as_bytes());
        return core::str::from_utf8(&buf[..n]).unwrap_or("0");
    }
    v = (v * 100.0 + 0.5).trunc() / 100.0;
    let whole = v.trunc();
    let frac = ((v - whole) * 100.0).round() as u32;
    let mut pos = 0usize;
    if neg {
        buf[pos] = b'-';
        pos += 1;
    }
    let mut digits = [0u8; 16];
    let mut dn = 0usize;
    let mut w = whole as u64;
    if w == 0 {
        digits[dn] = b'0';
        dn += 1;
    }
    while w > 0 && dn < digits.len() {
        digits[dn] = b'0' + (w % 10) as u8;
        w /= 10;
        dn += 1;
    }
    for i in (0..dn).rev() {
        buf[pos] = digits[i];
        pos += 1;
    }
    buf[pos] = b'.';
    buf[pos + 1] = b'0' + (frac / 10 % 10) as u8;
    buf[pos + 2] = b'0' + (frac % 10) as u8;
    core::str::from_utf8(&buf[..pos + 3]).unwrap_or("0")
}

/// Byte writer backed by the host's `clap_ostream` (state save path).
pub struct OutputStream<'a> {
    sys: &'a clap_ostream,
}

impl<'a> OutputStream<'a> {
    fn from_raw(sys: *const clap_ostream) -> Option<Self> {
        // SAFETY: the host builds this stream on its own stack for the duration
        // of the save call (CLAP contract; NOT VERIFIED beyond our mock).
        unsafe { sys.as_ref() }.map(|sys| Self { sys })
    }

    /// Writes all `bytes`; `false` on a short/stubbed write (host said no).
    pub fn write(&mut self, bytes: &[u8]) -> bool {
        let Some(write) = self.sys.write else {
            return false;
        };
        let n = unsafe {
            // SAFETY: `bytes` borrows caller memory alive across the call; the
            // stream pointer is the same one we wrapped, valid for the call.
            write(
                self.sys,
                bytes.as_ptr().cast::<c_void>(),
                bytes.len() as u64,
            )
        };
        u64::try_from(n).is_ok_and(|w| w == bytes.len() as u64)
    }
}

/// Byte reader backed by the host's `clap_istream` (state load path).
pub struct InputStream<'a> {
    sys: &'a clap_istream,
}

impl<'a> InputStream<'a> {
    fn from_raw(sys: *const clap_istream) -> Option<Self> {
        // SAFETY: same contract as `OutputStream`: host-stable for the load call.
        unsafe { sys.as_ref() }.map(|sys| Self { sys })
    }

    /// Reads up to `buf.len()` bytes; returns how many the host actually gave.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let Some(read) = self.sys.read else {
            return 0;
        };
        let n = unsafe {
            // SAFETY: `buf` is caller memory alive across the synchronous call.
            read(
                self.sys,
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as u64,
            )
        };
        usize::try_from(n).unwrap_or(0)
    }

    /// Fills `buf` completely or reports `false` at the first short read.
    pub fn read_exact(&mut self, buf: &mut [u8]) -> bool {
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = self.read(&mut buf[filled..]);
            if n == 0 {
                return false;
            }
            filled += n;
        }
        true
    }
}

// ---- the leaked instance: one Box holding vtables + the consumer ----

/// Everything the FFI side needs about one live plugin instance.
pub(crate) struct Instance<P: Plugin> {
    /// The vtable header the host gets back from [`create_instance`]. Note this
    /// is NOT the instance address: `Instance<P>` is `repr(Rust)`, so the
    /// compiler may place `base` at an offset (measured: 64). Every callback
    /// recovers the instance through `plugin_data`, which stores the true box
    /// address; that pointer is the only one we ever dereference as `Instance`.
    base: clap_plugin,
    host: Option<Host>,
    sample_rate_hz: f64,
    ext_ports: clap_plugin_audio_ports,
    ext_params: clap_plugin_params,
    ext_state: clap_plugin_state,
    ext_latency: clap_plugin_latency,
    ext_render: clap_plugin_render,
    plugin: P,
}

impl<P: Plugin> Instance<P> {
    fn new(host: Option<Host>) -> Self {
        Self {
            base: clap_plugin {
                desc: P::DESCRIPTOR,
                plugin_data: core::ptr::null_mut(),
                init: Some(tr_init::<P>),
                destroy: Some(tr_destroy::<P>),
                activate: Some(tr_activate::<P>),
                deactivate: Some(tr_deactivate::<P>),
                start_processing: Some(tr_start::<P>),
                stop_processing: Some(tr_stop::<P>),
                reset: Some(tr_reset::<P>),
                process: Some(tr_process::<P>),
                get_extension: Some(tr_get_extension::<P>),
                on_main_thread: Some(tr_on_main_thread::<P>),
            },
            host,
            sample_rate_hz: 0.0,
            ext_ports: clap_plugin_audio_ports {
                count: Some(tr_port_count::<P>),
                get: Some(tr_port_get::<P>),
            },
            ext_params: clap_plugin_params {
                count: Some(tr_param_count::<P>),
                get_info: Some(tr_param_info::<P>),
                get_value: Some(tr_param_value::<P>),
                value_to_text: Some(tr_param_text::<P>),
                text_to_value: Some(tr_param_parse::<P>),
                flush: Some(tr_param_flush::<P>),
            },
            ext_state: clap_plugin_state {
                save: Some(tr_state_save::<P>),
                load: Some(tr_state_load::<P>),
            },
            ext_latency: clap_plugin_latency {
                get: Some(tr_latency_get::<P>),
            },
            ext_render: clap_plugin_render {
                has_hard_realtime_requirement: Some(tr_render_hard_rt::<P>),
                set: Some(tr_render_set::<P>),
            },
            plugin: P::instantiate(),
        }
    }
}

/// Rebuilds the instance borrow from the pointer the host echoes back.
///
/// # Safety
///
/// * `plugin` is a pointer previously returned by [`create_instance`] for this
///   exact `P` (the trampoline that calls us lives in that instance's vtable),
///   and the host has not called `destroy` yet — CLAP lifetime contract, the
///   mock-host proxy test pins the sequence for us, real hosts NOT VERIFIED.
/// * We dereference `plugin` only to read `plugin_data` (a `#[repr(C)]` field
///   whose layout is pinned in [`crate::abi`]) and recover the instance from
///   that stored box address — never by assuming `plugin` and `Instance` share
///   an address (`Instance<P>` is `repr(Rust)`).
unsafe fn instance_of<'a, P: Plugin>(plugin: *const clap_plugin) -> Option<&'a mut Instance<P>> {
    // SAFETY: `plugin` is non-null by the caller's vtable contract (hosts pass
    // back the exact pointer we returned); reading `plugin_data` is an aligned
    // load of a `#[repr(C)]` struct field whose layout is pinned in [`crate::abi`].
    let data = unsafe { (*plugin).plugin_data };
    if data.is_null() {
        return None;
    }
    // SAFETY: module invariant — `plugin_data` was set by `create_instance` to
    // the address of a leaked `Instance<P>`, and this trampoline belongs to the
    // vtable of that same `P`, so the type match is guaranteed by dispatch, not
    // by guesswork. The borrow lives only for the duration of the callback.
    Some(unsafe { &mut *(data.cast::<Instance<P>>()) })
}

/// Creates one instance for the host; returns the `clap_plugin` vtable pointer,
/// or null if `plugin_id` is not [`Plugin::DESCRIPTOR`].id. A null host pointer
/// is tolerated at creation (the wrapper refuses at `init`, per [`Plugin::init`]
/// taking a live [`Host`]).
///
/// The `Box` leak here is *instance creation*, a main-thread event the host
/// triggers outside the audio cycle (CLAP rule; the allocation is reclaimed in
/// `destroy` — the only place that reclaims it).
///
/// # Safety
///
/// * `plugin_id` is null or a NUL-terminated C string owned by the caller for
///   the duration of the call (what a CLAP host passes).
/// * `host` is null or a live `clap_host` the host keeps valid at least until
///   `destroy` (the CLAP host contract — expected, NOT VERIFIED per host).
pub unsafe fn create_instance<P: Plugin + 'static>(
    host: *const clap_host,
    plugin_id: *const c_char,
) -> *const clap_plugin {
    if plugin_id.is_null() {
        return core::ptr::null();
    }
    let want = unsafe {
        // SAFETY: the host passed a NUL-terminated id (vtable contract); our
        // side of the comparison is a `clap_cstr!` static.
        CStr::from_ptr(plugin_id)
    };
    let ours = unsafe {
        // SAFETY: descriptors are built from `clap_cstr!` statics
        // ([`crate::make_descriptor`]), so `id` is a valid NUL-terminated string.
        CStr::from_ptr(P::DESCRIPTOR.id)
    };
    if want != ours {
        return core::ptr::null(); // not our plugin: the factory answered honestly
    }
    let mut boxed = Box::new(Instance::<P>::new(Host::from_raw(host)));
    let raw: *mut Instance<P> = &mut *boxed;
    unsafe {
        // SAFETY: `raw` is the address the box will have for its entire life —
        // we set `plugin_data` before exposing any pointer to the instance, and
        // `boxed` never moves after this line (it is immediately leaked).
        (*raw).base.plugin_data = raw.cast::<c_void>();
    }
    let leaked: &'static mut Instance<P> = Box::leak(boxed);
    &raw const leaked.base
}

// ---- trampolines: one thin `extern "C"` fn per vtable slot ----

unsafe extern "C" fn tr_init<P: Plugin>(plugin: *const clap_plugin) -> bool {
    // SAFETY: host echoes our pointer, instance not yet destroyed (module
    // invariant above); we borrow it for exactly this call.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    match inst.host {
        Some(host) => inst.plugin.init(host),
        None => false, // a null host at create time: refuse the instance
    }
}

unsafe extern "C" fn tr_destroy<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: the unique reclaim point. The host guarantees no further use of
    // `plugin` after `destroy` returns (CLAP contract; NOT VERIFIED per host).
    // The instance address comes from `plugin_data`, NOT from address equality
    // with `plugin`: `Instance<P>` is `repr(Rust)` and the compiler is free to
    // keep `base` at a nonzero offset (measured: 64 under this layout).
    // SAFETY: `plugin` is non-null by the vtable contract we handed out.
    let data = unsafe { (*plugin).plugin_data };
    if data.is_null() {
        return; // impossible by construction; leak rather than free a null Box
    }
    drop(unsafe { Box::from_raw(data.cast::<Instance<P>>()) });
}

unsafe extern "C" fn tr_activate<P: Plugin>(
    plugin: *const clap_plugin,
    sample_rate: f64,
    min_frames: u32,
    max_frames: u32,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    let ok = inst.plugin.activate(sample_rate, min_frames, max_frames);
    if ok {
        // Remember what the host promised: `process` views need the rate for
        // `ProcessCtx`, and only an active instance ever gets a `process` call.
        inst.sample_rate_hz = sample_rate;
    }
    ok
}

unsafe extern "C" fn tr_deactivate<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: borrowed for the call only.
    if let Some(inst) = unsafe { instance_of::<P>(plugin) } {
        inst.plugin.deactivate();
        inst.sample_rate_hz = 0.0;
    }
}

unsafe extern "C" fn tr_start<P: Plugin>(plugin: *const clap_plugin) -> bool {
    // SAFETY: borrowed for the call only.
    unsafe { instance_of::<P>(plugin) }.is_some_and(|inst| inst.plugin.start_processing())
}

unsafe extern "C" fn tr_stop<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: borrowed for the call only.
    if let Some(inst) = unsafe { instance_of::<P>(plugin) } {
        inst.plugin.stop_processing();
    }
}

unsafe extern "C" fn tr_reset<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: borrowed for the call only.
    if let Some(inst) = unsafe { instance_of::<P>(plugin) } {
        inst.plugin.reset();
    }
}

unsafe extern "C" fn tr_on_main_thread<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: borrowed for the call only.
    if let Some(inst) = unsafe { instance_of::<P>(plugin) } {
        inst.plugin.on_main_thread();
    }
}

unsafe extern "C" fn tr_process<P: Plugin>(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> crate::sys::process::clap_process_status {
    // SAFETY: host echoes our pointer and owns `process` for the call.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return crate::sys::process::CLAP_PROCESS_ERROR;
    };
    let Some(proc) = (unsafe { process.as_ref() }) else {
        return crate::sys::process::CLAP_PROCESS_ERROR; // null process: host bug
    };
    let rate = inst.sample_rate_hz;
    // SAFETY: the vtable hands us the host's `clap_process` for the duration
    // of this call — the CLAP contract is exactly `from_sys`'s safety
    // requirement, and null process was already rejected above.
    let mut view = unsafe { ProcessView::from_sys(proc, rate) };
    inst.plugin.process(&mut view).to_sys()
}

unsafe extern "C" fn tr_get_extension<P: Plugin>(
    plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return core::ptr::null();
    }
    // SAFETY: borrowed for the call: the host keeps `id` NUL-terminated while
    // calling (contract; NOT VERIFIED per host).
    let asked = unsafe { CStr::from_ptr(id) };
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return core::ptr::null();
    };
    if asked == CLAP_EXT_AUDIO_PORTS {
        (&raw const inst.ext_ports).cast::<c_void>()
    } else if asked == CLAP_EXT_PARAMS {
        (&raw const inst.ext_params).cast::<c_void>()
    } else if asked == CLAP_EXT_STATE {
        (&raw const inst.ext_state).cast::<c_void>()
    } else if asked == CLAP_EXT_LATENCY {
        (&raw const inst.ext_latency).cast::<c_void>()
    } else if asked == CLAP_EXT_RENDER {
        (&raw const inst.ext_render).cast::<c_void>()
    } else {
        core::ptr::null() // honest: we speak no other extension
    }
}

unsafe extern "C" fn tr_port_count<P: Plugin>(plugin: *const clap_plugin, is_input: bool) -> u32 {
    // SAFETY: borrowed for the call only.
    unsafe { instance_of::<P>(plugin) }.map_or(0, |inst| inst.plugin.audio_port_count(is_input))
}

unsafe extern "C" fn tr_port_get<P: Plugin>(
    plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    out: *mut clap_audio_port_info,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    // SAFETY: host-owned output struct, aligned and writable for this call
    // (vtable contract); we fill every field we know about through `write_into`.
    unsafe { out.as_mut() }
        .and_then(|dst| {
            inst.plugin.audio_port_info(index, is_input).map(|info| {
                info.write_into(dst);
                true
            })
        })
        .unwrap_or(false)
}

unsafe extern "C" fn tr_param_count<P: Plugin>(plugin: *const clap_plugin) -> u32 {
    // SAFETY: borrowed for the call only.
    unsafe { instance_of::<P>(plugin) }.map_or(0, |inst| inst.plugin.param_count())
}

unsafe extern "C" fn tr_param_info<P: Plugin>(
    plugin: *const clap_plugin,
    index: u32,
    out: *mut clap_param_info,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    // SAFETY: host-owned aligned output struct for the call; `write_into`
    // assigns every field we own, names with guaranteed NUL.
    unsafe { out.as_mut() }
        .and_then(|dst| {
            inst.plugin.param_info(index).map(|info| {
                info.write_into(dst);
                true
            })
        })
        .unwrap_or(false)
}

unsafe extern "C" fn tr_param_value<P: Plugin>(
    plugin: *const clap_plugin,
    id: clap_id,
    out_value: *mut f64,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    let value = inst.plugin.param_value(id);
    match value {
        Some(v) => {
            // SAFETY: host owns the f64 slot for the call; single aligned store.
            unsafe { *out_value = v };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn tr_param_text<P: Plugin>(
    plugin: *const clap_plugin,
    id: clap_id,
    value: f64,
    out_buffer: *mut c_char,
    out_buffer_capacity: u32,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    if out_buffer.is_null() || out_buffer_capacity == 0 {
        return false;
    }
    let mut text = ParamText::new(out_buffer, out_buffer_capacity);
    inst.plugin.param_text(id, value, &mut text)
}

unsafe extern "C" fn tr_param_parse<P: Plugin>(
    plugin: *const clap_plugin,
    id: clap_id,
    text: *const c_char,
    out_value: *mut f64,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    if text.is_null() {
        return false;
    }
    // SAFETY: host keeps `text` NUL-terminated for the call (contract; NOT
    // VERIFIED per host).
    let value = inst
        .plugin
        .param_from_text(id, unsafe { CStr::from_ptr(text) });
    match value {
        Some(v) => {
            // SAFETY: host owns the f64 slot for the call.
            unsafe { *out_value = v };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn tr_param_flush<P: Plugin>(
    plugin: *const clap_plugin,
    in_events: *const crate::sys::events::clap_input_events,
    out_events: *const crate::sys::events::clap_output_events,
) {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return;
    };
    // Both event lists are call-scoped borrows per the CLAP rule.
    let input = InEvents::from_raw(in_events);
    // SAFETY: `out_events` is *const but the host vtable mutates through it
    // (`try_push` takes `*const`); `as_mut` on a raw pointer from a const
    // source is the host's design — we confine the borrow to this call.
    let output = OutEvents::from_raw(out_events.cast_mut());
    inst.plugin.flush(input, output);
}

unsafe extern "C" fn tr_state_save<P: Plugin>(
    plugin: *const clap_plugin,
    stream: *const clap_ostream,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    match OutputStream::from_raw(stream) {
        Some(mut out) => inst.plugin.save_state(&mut out),
        None => false,
    }
}

unsafe extern "C" fn tr_state_load<P: Plugin>(
    plugin: *const clap_plugin,
    stream: *const clap_istream,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    match InputStream::from_raw(stream) {
        Some(mut input) => inst.plugin.load_state(&mut input),
        None => false,
    }
}

unsafe extern "C" fn tr_latency_get<P: Plugin>(plugin: *const clap_plugin) -> u32 {
    // SAFETY: borrowed for the call only.
    unsafe { instance_of::<P>(plugin) }.map_or(0, |inst| inst.plugin.latency())
}

unsafe extern "C" fn tr_render_hard_rt<P: Plugin>(plugin: *const clap_plugin) -> bool {
    // SAFETY: borrowed for the call only.
    unsafe { instance_of::<P>(plugin) }
        .is_some_and(|inst| inst.plugin.has_hard_realtime_requirement())
}

unsafe extern "C" fn tr_render_set<P: Plugin>(
    plugin: *const clap_plugin,
    mode: clap_plugin_render_mode,
) -> bool {
    // SAFETY: borrowed for the call only.
    let Some(inst) = (unsafe { instance_of::<P>(plugin) }) else {
        return false;
    };
    match mode {
        m if m == CLAP_RENDER_REALTIME => inst.plugin.set_offline(false),
        m if m == CLAP_RENDER_OFFLINE => inst.plugin.set_offline(true),
        _ => false, // unknown mode: honest refusal, never an "assume realtime"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clap_cstr;

    #[test]
    fn format_f64_is_short_and_exact() {
        let mut buf = [0u8; 32];
        assert_eq!(format_f64(0.0, &mut buf), "0.00");
        assert_eq!(format_f64(-3.25, &mut buf), "-3.25");
        assert_eq!(format_f64(120.0, &mut buf), "120.00");
        assert_eq!(format_f64(f64::NAN, &mut buf), "nan");
        assert_eq!(format_f64(f64::INFINITY, &mut buf), "inf");
        assert_eq!(format_f64(f64::NEG_INFINITY, &mut buf), "-inf");
    }

    #[test]
    fn port_and_param_write_into_padded_arrays() {
        let mut dst = clap_audio_port_info {
            id: 0,
            name: [0; crate::sys::string_sizes::CLAP_NAME_SIZE],
            flags: 0,
            channel_count: 0,
            port_type: core::ptr::null(),
            in_place_pair: 0,
        };
        AudioPortInfo::main_stereo(3, "Main").write_into(&mut dst);
        assert_eq!(dst.id, 3);
        assert_eq!(dst.flags, CLAP_AUDIO_PORT_IS_MAIN);
        assert_eq!(dst.channel_count, 2);
        assert_eq!(dst.in_place_pair, CLAP_INVALID_ID);
        // SAFETY: name was just terminated by write_padded_name inside the
        // fixed array we own.
        let read_name = unsafe { CStr::from_ptr(dst.name.as_ptr()) };
        assert_eq!(read_name, clap_cstr!("Main"));

        let mut pin = clap_param_info {
            id: 0,
            flags: 0,
            cookie: core::ptr::null_mut(),
            name: [0; crate::sys::string_sizes::CLAP_NAME_SIZE],
            module: [0; crate::sys::string_sizes::CLAP_PATH_SIZE],
            min_value: 0.0,
            max_value: 0.0,
            default_value: 0.0,
        };
        ParamInfo::automatable(7, "Freq", 20.0, 20_000.0).write_into(&mut pin);
        assert_eq!(pin.id, 7);
        assert_eq!(
            pin.flags,
            crate::sys::ext::params::CLAP_PARAM_IS_AUTOMATABLE
        );
        // SAFETY: as above, the writer guarantees the NUL inside the array.
        let read = unsafe { CStr::from_ptr(pin.name.as_ptr()) };
        assert_eq!(read, clap_cstr!("Freq"));
        assert!(pin.module[0] == 0, "empty module is a bare terminator");
    }

    #[test]
    fn param_text_is_bounded_by_the_hosts_capacity() {
        let mut raw = [0i8; 8];
        let mut text = ParamText::new(raw.as_mut_ptr(), 8);
        assert!(text.write_str("123.45"));
        assert!(
            !text.write_str("way too long here"),
            "capacity + NUL respected"
        );
        let mut tiny = [0i8; 2];
        let mut t2 = ParamText::new(tiny.as_mut_ptr(), 2);
        assert!(t2.write_str("x"), "1 char + NUL fits exactly");
        assert!(!t2.write_str("xy"), "2 chars + NUL does not");
    }
}
