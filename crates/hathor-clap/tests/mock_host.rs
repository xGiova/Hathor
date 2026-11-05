//! Mock-host lifecycle: the official proxy for "this plugin loads in a host".
//!
//! ## What this proves, and what it does NOT
//!
//! This test plays the host role through the *real C ABI*: function pointers,
//! raw `#[repr(C)]` structs, string-comparing extension lookups — the exact
//! sequence a DAW runs: init → get_factory → create → add_extensions →
//! activate(48000, 64) → process (sine in, output touched and non-NaN) → flush
//! with queued param events → save_state → perturb → reload → deactivate →
//! destroy → deinit. The process loop additionally runs under the workspace
//! `AudioSection` (STRICT): zero allocations inside the wrapper's process path.
//!
//! **It is a MOCK.** The closest thing to a DAW load that fits in a test — real
//! loading in Reaper is an OWNER step scheduled after task-4. Nothing here may
//! be quoted as "verified in real hosts"; the wrapper's host-behavior
//! assumptions stay labeled *expected, NOT VERIFIED* in their own modules.
//!
//! ## Consumer under test
//!
//! A minimal dedicated `Plugin` impl: fixed-gain stereo, three params,
//! stack-only state I/O. Per the lead's ruling this is deliberately NOT a
//! `Chain`/host-graph composition — the wrapper must stay generic over the safe
//! trait, and task-4 supplies the real plugin. The gain consumer is
//! allocation-free so the zero-alloc gate measures the *wrapper*, not DSP.

use std::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, MutexGuard, OnceLock};

use hathor_clap::sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, clap_output_events,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
};
use hathor_clap::sys::ext::audio_ports::{
    clap_audio_port_info, clap_host_audio_ports, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN,
    CLAP_EXT_AUDIO_PORTS, CLAP_PORT_STEREO,
};
use hathor_clap::sys::ext::latency::{clap_host_latency, clap_plugin_latency, CLAP_EXT_LATENCY};
use hathor_clap::sys::ext::log::{clap_host_log, CLAP_EXT_LOG};
use hathor_clap::sys::ext::params::{
    clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS,
};
use hathor_clap::sys::ext::render::{
    clap_plugin_render, CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME,
};
use hathor_clap::sys::ext::state::{clap_host_state, clap_plugin_state, CLAP_EXT_STATE};
use hathor_clap::sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use hathor_clap::sys::host::clap_host;
use hathor_clap::sys::plugin::{clap_plugin, clap_plugin_descriptor};
use hathor_clap::sys::process::{clap_process, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR};
use hathor_clap::sys::stream::{clap_istream, clap_ostream};
use hathor_clap::sys::version::{clap_version_is_compatible, CLAP_VERSION};

use hathor_clap::{
    clap_cstr, create_instance, read_padded_name, AudioPortInfo, Host, InputStream, LogSeverity,
    OutputStream, ParamInfo, ParamText, Plugin, ProcessStatus, ProcessView,
};

use hathor_dsp_core::{AudioSection, CheckedAllocator};

// House auditor: the same global allocator the core/host-graph tests install,
// so "0 allocations" means the same thing everywhere.
#[global_allocator]
static ALLOC: CheckedAllocator = CheckedAllocator::new();

// ---- host bookkeeping (one lock; the lifecycle test drives it from one thread) ----

#[derive(Default)]
struct HostState {
    log_lines: Vec<(i32, String)>,
    latencies_announced: u32,
    dirty_marks: u32,
    rescans: u32,
    restart_requests: u32,
    saved_state: Vec<u8>,
    load_cursor: usize,
}

fn serial() -> MutexGuard<'static, HostState> {
    static LOCK: OnceLock<Mutex<HostState>> = OnceLock::new();
    match LOCK.get_or_init(|| Mutex::new(HostState::default())).lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// ---- the mock host's callbacks (raw C, on purpose) ----

unsafe extern "C" fn host_log(_host: *const clap_host, severity: i32, message: *const c_char) {
    // SAFETY: test-controlled: the wrapper built `message` from a Rust `CStr`
    // that lives for the call; `_host` is our own static below.
    let msg = unsafe { CStr::from_ptr(message) };
    serial()
        .log_lines
        .push((severity, msg.to_string_lossy().into_owned()));
}

unsafe extern "C" fn host_latency_changed(_host: *const clap_host) {
    serial().latencies_announced += 1;
}

unsafe extern "C" fn host_state_mark_dirty(_host: *const clap_host) {
    serial().dirty_marks += 1;
}

unsafe extern "C" fn host_rescan(_host: *const clap_host, _flags: u32) {
    serial().rescans += 1;
}

unsafe extern "C" fn host_params_clear(_host: *const clap_host, _param_id: u32, _flags: u32) {}

unsafe extern "C" fn host_request_flush(_host: *const clap_host) {
    let _ = _host;
    // the wrapper never asks for a flush in this lifecycle; kept for vtable
    // completeness and wired to the state below.
    serial().rescans += 1;
}

unsafe extern "C" fn host_request_restart(_host: *const clap_host) {
    serial().restart_requests += 1;
}

unsafe extern "C" fn host_get_extension(
    _host: *const clap_host,
    id: *const c_char,
) -> *const c_void {
    // SAFETY: test-controlled: `id` comes from the wrapper's `clap_cstr!` ids
    // or our own calls below.
    let asked = unsafe { CStr::from_ptr(id) };
    if asked == CLAP_EXT_LOG {
        (&raw const MOCK_LOG).cast()
    } else if asked == CLAP_EXT_LATENCY {
        (&raw const MOCK_LATENCY).cast()
    } else if asked == CLAP_EXT_STATE {
        (&raw const MOCK_STATE).cast()
    } else if asked == CLAP_EXT_AUDIO_PORTS {
        (&raw const MOCK_AUDIO_PORTS).cast()
    } else if asked == CLAP_EXT_PARAMS {
        (&raw const MOCK_PARAMS).cast()
    } else {
        core::ptr::null()
    }
}

static MOCK_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};
static MOCK_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};
static MOCK_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_state_mark_dirty),
};
static MOCK_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: None,
    rescan: Some(host_rescan),
};
static MOCK_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_request_flush),
};

static MOCK_HOST: clap_host = clap_host {
    clap_version: CLAP_VERSION,
    host_data: core::ptr::null_mut(),
    name: clap_cstr!("Mock Host").as_ptr(),
    vendor: clap_cstr!("Nobody").as_ptr(),
    url: clap_cstr!("").as_ptr(),
    version: clap_cstr!("0.0").as_ptr(),
    get_extension: Some(host_get_extension),
    request_restart: Some(host_request_restart),
    request_process: None,
    request_callback: None,
};

// ---- state streams: bytes into `saved_state`, back out on load ----

unsafe extern "C" fn stream_write(
    _stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    let n = size as usize;
    // SAFETY: the wrapper passes one owned slice of `n` bytes that lives for the
    // call; the stream struct itself is our static below.
    let bytes = unsafe { core::slice::from_raw_parts(buffer.cast::<u8>(), n) };
    let mut st = serial();
    if st.saved_state.len() + n > 4096 {
        return -1; // mock refuses absurd blobs
    }
    st.saved_state.extend_from_slice(bytes);
    n as i64
}

unsafe extern "C" fn stream_read(
    _stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    let n = size as usize;
    let mut st = serial();
    let remaining = st.saved_state.len() - st.load_cursor;
    let take = n.min(remaining);
    // SAFETY: `buffer` is the wrapper's stack array of at least `take` writable
    // bytes for the call; we only fill what the host's stream still has.
    let dst = unsafe { core::slice::from_raw_parts_mut(buffer.cast::<u8>(), take) };
    dst.copy_from_slice(&st.saved_state[st.load_cursor..st.load_cursor + take]);
    st.load_cursor += take;
    take as i64
}

static MOCK_OSTREAM: clap_ostream = clap_ostream {
    ctx: core::ptr::null_mut(),
    write: Some(stream_write),
};
static MOCK_ISTREAM: clap_istream = clap_istream {
    ctx: core::ptr::null_mut(),
    read: Some(stream_read),
};

// ---- flush event tables ----

fn mk_event(id: u32, value: f64, time: u32) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: core::mem::size_of::<clap_event_param_value>() as u32,
            time,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: id,
        cookie: core::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

static mut FLUSH_MAIN: Option<[clap_event_param_value; 2]> = None;
static mut PERTURB: Option<[clap_event_param_value; 1]> = None;

fn fill_flush_events() {
    let main = &raw mut FLUSH_MAIN;
    let perturb = &raw mut PERTURB;
    // SAFETY: called once at the top of the single lifecycle test, before any
    // list pointer exists; the vtables below only ever read afterwards.
    unsafe {
        *main = Some([mk_event(0, 0.25, 16), mk_event(1, 7000.0, 0)]);
        *perturb = Some([mk_event(0, 0.9, 0)]);
    }
}

unsafe extern "C" fn main_size(_l: *const clap_input_events) -> u32 {
    2
}

unsafe extern "C" fn main_get(l: *const clap_input_events, index: u32) -> *const clap_event_header {
    if l == core::ptr::from_ref(&MAIN_LIST) {
        // SAFETY: test storage filled before the list is ever handed over.
        unsafe {
            match (*core::ptr::addr_of!(FLUSH_MAIN))
                .as_ref()
                .and_then(|t| t.get(index as usize))
            {
                Some(ev) => core::ptr::from_ref(ev).cast(),
                None => core::ptr::null(),
            }
        }
    } else {
        core::ptr::null()
    }
}

unsafe extern "C" fn perturb_size(_l: *const clap_input_events) -> u32 {
    1
}

unsafe extern "C" fn perturb_get(
    l: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    if l == core::ptr::from_ref(&PERTURB_LIST) {
        // SAFETY: as above.
        unsafe {
            match (*core::ptr::addr_of!(PERTURB))
                .as_ref()
                .and_then(|t| t.get(index as usize))
            {
                Some(ev) => core::ptr::from_ref(ev).cast(),
                None => core::ptr::null(),
            }
        }
    } else {
        core::ptr::null()
    }
}

static MAIN_LIST: clap_input_events = clap_input_events {
    ctx: core::ptr::null_mut(),
    size: Some(main_size),
    get: Some(main_get),
};

static PERTURB_LIST: clap_input_events = clap_input_events {
    ctx: core::ptr::null_mut(),
    size: Some(perturb_size),
    get: Some(perturb_get),
};

static NULL_OUT_LIST: clap_output_events = clap_output_events {
    ctx: core::ptr::null_mut(),
    try_push: None,
};

// ---- the consumer plugin: fixed-gain stereo, 3 params, stack-only state ----

const PARAM_GAIN: u32 = 0;
const PARAM_FREQ: u32 = 1;
const PARAM_MODE: u32 = 2;

static DESCRIPTOR: clap_plugin_descriptor = hathor_clap::make_descriptor(
    clap_cstr!("com.hathor.mock-gain"),
    clap_cstr!("Mock Gain"),
    clap_cstr!("Hathor"),
    clap_cstr!("https://hathor.local/mock-gain"),
    clap_cstr!(""),
    clap_cstr!(""),
    clap_cstr!("0.1.0"),
    clap_cstr!("mock-host proxy consumer, allocation-free by construction"),
);

struct GainPlugin {
    host: Option<Host>,
    gain: f64,
    freq: f64,
    mode: f64,
    active: bool,
}

impl Plugin for GainPlugin {
    const DESCRIPTOR: &'static clap_plugin_descriptor = &DESCRIPTOR;

    fn instantiate() -> Self {
        Self {
            host: None,
            gain: 1.0,
            freq: 1000.0,
            mode: 0.0,
            active: false,
        }
    }

    fn init(&mut self, host: Host) -> bool {
        // First host touch: prove the log forward lives inside the lifecycle the
        // way task-4 will use it.
        let logged = host.log(LogSeverity::Info, clap_cstr!("mock-gain init"));
        self.host = Some(host);
        logged && self.param_count() == 3
    }

    fn activate(&mut self, _sample_rate: f64, _min: u32, _max: u32) -> bool {
        self.active = true;
        true
    }

    fn deactivate(&mut self) {
        self.active = false;
    }

    fn process(&mut self, io: &mut ProcessView<'_>) -> ProcessStatus {
        if !self.active {
            return ProcessStatus::Error;
        }
        let gain = self.gain as f32;
        let frames = io.frames();
        let channels = io.port_channels(0, false).unwrap_or(0) as usize;
        let aliased = io.out_aliases_in();
        for ch in 0..channels {
            // Take the input borrow *before* the output borrow: the view keeps
            // them independent (`&'a` vs `&mut self`) on purpose.
            let src = io.input_channel(0, ch);
            let Some(mut block) = io.output_block(0, ch) else {
                return ProcessStatus::Error;
            };
            let dst = block.channel(0);
            if frames == 0 {
                continue;
            }
            if aliased {
                // in-place pair: the input bytes ARE the output; scale in place.
                for v in dst.iter_mut() {
                    *v *= gain;
                }
            } else {
                match src {
                    Some(s) => {
                        dst.copy_from_slice(s);
                        for v in dst.iter_mut() {
                            *v *= gain;
                        }
                    }
                    None => dst.fill(0.0),
                }
            }
        }
        ProcessStatus::Continue
    }

    // ---- ports: one main stereo in + out, declared in-place pairs ----

    fn audio_port_count(&self, _is_input: bool) -> u32 {
        1
    }

    fn audio_port_info(&self, index: u32, is_input: bool) -> Option<AudioPortInfo<'_>> {
        if index != 0 {
            return None;
        }
        let name = if is_input { "Main In" } else { "Main Out" };
        let mut info = AudioPortInfo::main_stereo(if is_input { 10 } else { 20 }, name);
        info.in_place_pair = Some(if is_input { 20 } else { 10 });
        Some(info)
    }

    // ---- params ----

    fn param_count(&self) -> u32 {
        3
    }

    fn param_info(&self, index: u32) -> Option<ParamInfo<'_>> {
        match index {
            0 => Some(ParamInfo::automatable(PARAM_GAIN, "Gain", 0.0, 1.0)),
            1 => Some(ParamInfo {
                id: PARAM_FREQ,
                flags: 0,
                name: "Freq",
                module: "Tone",
                min_value: 20.0,
                max_value: 20_000.0,
                default_value: 1000.0,
            }),
            2 => Some(ParamInfo {
                id: PARAM_MODE,
                flags: 0,
                name: "Mode",
                module: "",
                min_value: 0.0,
                max_value: 2.0,
                default_value: 0.0,
            }),
            _ => None,
        }
    }

    fn param_value(&self, id: u32) -> Option<f64> {
        match id {
            PARAM_GAIN => Some(self.gain),
            PARAM_FREQ => Some(self.freq),
            PARAM_MODE => Some(self.mode),
            _ => None,
        }
    }

    fn set_param_value(&mut self, id: u32, value: f64) {
        match id {
            PARAM_GAIN => self.gain = value,
            PARAM_FREQ => self.freq = value,
            PARAM_MODE => self.mode = value,
            _ => {}
        }
        if id == PARAM_GAIN {
            if let Some(h) = self.host {
                // exercise the host-forwarding surface from inside the consumer
                h.latency_changed();
                h.state_mark_dirty();
            }
        }
    }

    fn param_text(&self, id: u32, value: f64, out: &mut ParamText<'_>) -> bool {
        match id {
            PARAM_GAIN | PARAM_FREQ => out.write_f64(value),
            PARAM_MODE => out.write_str(if value < 0.5 {
                "clean"
            } else if value < 1.5 {
                "soft"
            } else {
                "hard"
            }),
            _ => false,
        }
    }

    fn param_from_text(&self, _id: u32, text: &CStr) -> Option<f64> {
        // numeric display round-trips; no other syntax is claimed
        text.to_str().ok()?.parse::<f64>().ok()
    }

    fn save_state(&mut self, out: &mut OutputStream<'_>) -> bool {
        let mut buf = [0u8; 20];
        buf[0..8].copy_from_slice(&self.gain.to_le_bytes());
        buf[8..16].copy_from_slice(&self.freq.to_le_bytes());
        buf[16..20].copy_from_slice(&(self.mode as u32).to_le_bytes());
        out.write(&buf)
    }

    fn load_state(&mut self, input: &mut InputStream<'_>) -> bool {
        let mut buf = [0u8; 20];
        if !input.read_exact(&mut buf) {
            return false;
        }
        self.gain = f64::from_le_bytes(buf[0..8].try_into().expect("len 8"));
        self.freq = f64::from_le_bytes(buf[8..16].try_into().expect("len 8"));
        self.mode = u32::from_le_bytes(buf[16..20].try_into().expect("len 4")) as f64;
        true
    }

    fn on_main_thread(&mut self) {
        if let Some(h) = self.host {
            h.request_restart();
        }
    }
}

// ---- the entry registration, exercised through the macro exactly as task-4 will ----

hathor_clap::clap_entry!(generated, GainPlugin);

// ---- helpers so the lifecycle reads the way a host drives it ----

fn expect<T: Copy>(slot: &Option<T>) -> T {
    *slot
        .as_ref()
        .expect("mock host expected a vtable slot that is null")
}

fn ext_of(plugin: *const clap_plugin, id: &'static CStr) -> *const c_void {
    // SAFETY: raw vtable call; `id` is a static `&CStr` from clap-sys, and the
    // plugin pointer is the one we created.
    unsafe {
        plugin
            .as_ref()
            .and_then(|p| p.get_extension)
            .map_or(core::ptr::null(), |f| f(plugin, id.as_ptr()))
    }
}

fn process_with(
    plugin: *const clap_plugin,
    frames: u32,
    in0: &mut [f32; 64],
    in1: &mut [f32; 64],
    out0: &mut [f32; 64],
    out1: &mut [f32; 64],
) -> i32 {
    let mut in_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
    let mut out_ptrs = [out0.as_mut_ptr(), out1.as_mut_ptr()];
    let in_buf = clap_audio_buffer {
        data32: in_ptrs.as_mut_ptr(),
        data64: core::ptr::null_mut(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let mut out_buf = clap_audio_buffer {
        data32: out_ptrs.as_mut_ptr(),
        data64: core::ptr::null_mut(),
        channel_count: 2,
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
    // SAFETY: raw process vtable call with a host-shaped struct whose arrays we
    // own for the call (everything here is a stack local of this fn).
    unsafe {
        plugin
            .as_ref()
            .and_then(|p| p.process)
            .map_or(CLAP_PROCESS_ERROR, |f| {
                f(plugin, core::ptr::from_ref(&proc))
            })
    }
}

use hathor_clap::sys::audio_buffer::clap_audio_buffer;

/// deterministic quarter-offset sine pair — no rng, no allocation
fn sine_pair(i: usize) -> (f32, f32) {
    let x = i as f64 * 0.02;
    (
        x.sin() as f32,
        (x + std::f64::consts::FRAC_PI_2).sin() as f32,
    )
}

// ---- THE proxy test: whole lifecycle through the C ABI ----

#[test]
fn mock_host_full_lifecycle_and_alloc_contract() {
    fill_flush_events();
    let entry = &generated::clap_entry;

    // 1. entry version handshake, like every real host does first.
    assert!(
        clap_version_is_compatible(entry.clap_version),
        "the entry advertises a CLAP version the host (us) understands"
    );
    // 1b. the by-value proof field by field: the exported static IS the whole
    // vtable — version stamped, every slot wired to a generated wrapper fn.
    assert_eq!(entry.clap_version, hathor_clap::sys::version::CLAP_VERSION);
    assert!(
        entry.init.is_some() && entry.deinit.is_some() && entry.get_factory.is_some(),
        "no hole in the generated table"
    );

    // 2. init / double-init refusal (documented wrapper policy).
    let init = expect(&entry.init);
    let ok = unsafe { init(core::ptr::null()) };
    assert!(ok, "init succeeds");
    let again = unsafe { init(core::ptr::null()) };
    assert!(
        !again,
        "a second init without deinit is refused (policy above)"
    );

    // 3. factory: only the one id, only ours.
    let get_factory = expect(&entry.get_factory);
    let bogus = unsafe { get_factory(clap_cstr!("clap.not-a-factory").as_ptr()) };
    assert!(bogus.is_null(), "unknown factory id answers null");
    let factory_raw = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) };
    assert!(
        !factory_raw.is_null(),
        "the real factory id answers a pointer"
    );
    let factory: *const clap_plugin_factory = factory_raw.cast();

    // 4. factory contents.
    let count = unsafe { expect(&(*factory).get_plugin_count)(factory) };
    assert_eq!(count, 1, "this factory carries exactly one plugin");
    let desc: *const clap_plugin_descriptor =
        unsafe { expect(&(*factory).get_plugin_descriptor)(factory, 0) };
    assert!(!desc.is_null());
    let out_of_range = unsafe { expect(&(*factory).get_plugin_descriptor)(factory, 1) };
    assert!(out_of_range.is_null(), "index 1 does not exist");
    // SAFETY: the descriptor is a static; its fields are `clap_cstr!` pointers.
    let d = unsafe { &*desc };
    assert_eq!(d.clap_version, CLAP_VERSION);
    assert_eq!(
        unsafe { CStr::from_ptr(d.id) },
        clap_cstr!("com.hathor.mock-gain")
    );
    assert_eq!(unsafe { CStr::from_ptr(d.name) }, clap_cstr!("Mock Gain"));
    assert!(d.features.is_null(), "empty feature list published as null");

    // 5. wrong id must NOT create.
    let create = unsafe { expect(&(*factory).create_plugin) };
    let wrong = unsafe {
        create(
            factory,
            &raw const MOCK_HOST,
            clap_cstr!("com.example.nope").as_ptr(),
        )
    };
    assert!(wrong.is_null(), "a foreign id is refused at create time");

    // 6. create with the right id.
    let plugin: *const clap_plugin = unsafe { create(factory, &raw const MOCK_HOST, d.id) };
    assert!(!plugin.is_null(), "create succeeds for our own id");
    // SAFETY: `plugin` is the pointer we just created; plain field reads.
    unsafe {
        let p = &*plugin;
        assert!(
            !p.plugin_data.is_null(),
            "plugin_data identifies the instance"
        );
        assert_eq!(
            CStr::from_ptr(expect(&p.desc.as_ref()).id),
            CStr::from_ptr(DESCRIPTOR.id),
            "the instance vtable points at our descriptor"
        );
    }

    // 7. plugin init (host gets its first touches).
    let inited = unsafe { expect(&(*plugin).init)(plugin) };
    assert!(inited, "plugin.init reaches the consumer");
    assert_eq!(
        serial().log_lines.len(),
        1,
        "the consumer logged exactly once at init"
    );
    assert_eq!(serial().log_lines[0].0, 1, "severity INFO reached the host");

    // 8. add_extensions: host asks for each extension by id.
    let ports: *const clap_plugin_audio_ports = ext_of(plugin, CLAP_EXT_AUDIO_PORTS).cast();
    let params: *const clap_plugin_params = ext_of(plugin, CLAP_EXT_PARAMS).cast();
    let state: *const clap_plugin_state = ext_of(plugin, CLAP_EXT_STATE).cast();
    let latency: *const clap_plugin_latency = ext_of(plugin, CLAP_EXT_LATENCY).cast();
    let render: *const clap_plugin_render = ext_of(plugin, CLAP_EXT_RENDER).cast();
    let noise = ext_of(plugin, clap_cstr!("clap.gui"));
    assert!(
        !ports.is_null()
            && !params.is_null()
            && !state.is_null()
            && !latency.is_null()
            && !render.is_null(),
        "every wired extension answers a pointer"
    );
    assert!(noise.is_null(), "an extension we do not speak answers null");

    // audio_ports count/get through the raw vtable.
    let (n_in, n_out) = (unsafe { expect(&(*ports).count)(plugin, true) }, unsafe {
        expect(&(*ports).count)(plugin, false)
    });
    assert_eq!((n_in, n_out), (1, 1), "one main port each way");
    let mut pin = clap_audio_port_info {
        id: 0,
        name: [0; 256],
        flags: 0,
        channel_count: 0,
        port_type: core::ptr::null(),
        in_place_pair: 0,
    };
    let got = unsafe { expect(&(*ports).get)(plugin, 0, true, &raw mut pin) };
    assert!(got, "port 0 in fills the host's struct");
    assert_eq!(pin.id, 10);
    assert_eq!(pin.flags & CLAP_AUDIO_PORT_IS_MAIN, CLAP_AUDIO_PORT_IS_MAIN);
    assert_eq!(pin.channel_count, 2, "stereo, f32");
    assert_eq!(read_padded_name(&pin.name), "Main In");
    assert_eq!(unsafe { CStr::from_ptr(pin.port_type) }, CLAP_PORT_STEREO);
    assert_eq!(pin.in_place_pair, 20);
    let bad_index = unsafe { expect(&(*ports).get)(plugin, 5, true, &raw mut pin) };
    assert!(!bad_index, "out of range index answers false");

    // params count / get_info / get_value / value_to_text / text_to_value.
    assert_eq!(unsafe { expect(&(*params).count)(plugin) }, 3);
    let mut pinfo = clap_param_info {
        id: 0,
        flags: 0,
        cookie: core::ptr::null_mut(),
        name: [0; 256],
        module: [0; 1024],
        min_value: 0.0,
        max_value: 0.0,
        default_value: 0.0,
    };
    assert!(unsafe { expect(&(*params).get_info)(plugin, 1, &raw mut pinfo) });
    assert_eq!(pinfo.id, PARAM_FREQ);
    assert_eq!(read_padded_name(&pinfo.name), "Freq");
    assert_eq!(read_padded_name(&pinfo.module), "Tone");
    assert_eq!(pinfo.max_value, 20_000.0);
    assert!(
        !unsafe { expect(&(*params).get_info)(plugin, 3, &raw mut pinfo) },
        "param index 3 is out of range"
    );
    let mut v = 0.0f64;
    assert!(unsafe { expect(&(*params).get_value)(plugin, PARAM_FREQ, &raw mut v) });
    assert_eq!(v, 1000.0, "default freq as declared");
    assert!(
        !unsafe { expect(&(*params).get_value)(plugin, 999, &raw mut v) },
        "unknown id answers false"
    );
    let mut tbuf = [0i8; 1024];
    let txt_ok = unsafe {
        expect(&(*params).value_to_text)(plugin, PARAM_FREQ, 1234.5, tbuf.as_mut_ptr(), 1024)
    };
    assert!(txt_ok);
    assert_eq!(
        // SAFETY: `value_to_text` terminates inside the buffer we just gave it.
        unsafe { CStr::from_ptr(tbuf.as_ptr()) }
            .to_str()
            .expect("ascii"),
        "1234.50",
        "text formatting crosses the boundary intact"
    );
    let mut parsed = 9.0f64;
    let ok_parse = unsafe {
        expect(&(*params).text_to_value)(
            plugin,
            PARAM_FREQ,
            clap_cstr!("2.75").as_ptr(),
            &raw mut parsed,
        )
    };
    assert!(
        ok_parse && parsed == 2.75,
        "the consumer's numeric parse crosses the boundary"
    );

    // latency + render.
    assert_eq!(
        unsafe { expect(&(*latency).get)(plugin) },
        0,
        "default latency 0"
    );
    assert!(
        !unsafe { expect(&(*render).has_hard_realtime_requirement)(plugin) },
        "the wrapper's honest default: nothing here requires realtime"
    );
    assert!(
        unsafe { expect(&(*render).set)(plugin, CLAP_RENDER_OFFLINE) },
        "offline accepted"
    );
    assert!(unsafe { expect(&(*render).set)(plugin, CLAP_RENDER_REALTIME) });
    assert!(
        !unsafe { expect(&(*render).set)(plugin, 999) },
        "unknown render mode refused, never guessed"
    );

    // 9. activate(48000, 64) — the task's exact numbers.
    let activated = unsafe { expect(&(*plugin).activate)(plugin, 48_000.0, 64, 64) };
    assert!(activated);

    // 10. process: sine in, output touched and non-NaN; ALLOC CONTRACT under
    // the workspace AudioSection (STRICT), zero allocations in the wrapper path.
    assert!(unsafe { expect(&(*plugin).start_processing)(plugin) });

    let mut in0 = [0f32; 64];
    let mut in1 = [0f32; 64];
    let mut out0 = [-7.0f32; 64];
    let mut out1 = [-7.0f32; 64];
    for i in 0..64 {
        let (s, c) = sine_pair(i);
        in0[i] = s;
        in1[i] = c;
    }

    // Warm-up block FIRST, outside the section (lazy paths, none expected).
    let warm = process_with(plugin, 64, &mut in0, &mut in1, &mut out0, &mut out1);
    assert_eq!(warm, CLAP_PROCESS_CONTINUE);
    assert!(
        out0.iter().chain(out1.iter()).all(|x| x.is_finite()),
        "warm-up output is touched, finite, non-NaN"
    );
    assert!(
        out0.iter().any(|x| *x != -7.0),
        "output memory actually moved"
    );

    // The gate: run blocks strictly inside the audio section.
    let section = AudioSection::enter().expect("outermost section");
    let mut last_status = -999;
    for _ in 0..8 {
        last_status = process_with(plugin, 64, &mut in0, &mut in1, &mut out0, &mut out1);
    }
    let inside = section.allocs_here();
    drop(section);
    assert_eq!(last_status, CLAP_PROCESS_CONTINUE);
    assert_eq!(
        inside, 0,
        "zero allocations across 8 process blocks inside the audio section"
    );
    // gain is 1.0 so far: output must equal input bit-exactly
    assert_eq!(&out0[..], &in0[..], "gain 1.0 passthrough, exact");
    assert_eq!(&out1[..], &in1[..]);

    // 11. flush: two queued param values arrive through the default flush path.
    unsafe { expect(&(*params).flush)(plugin, &raw const MAIN_LIST, &raw const NULL_OUT_LIST) };
    assert!(unsafe { expect(&(*params).get_value)(plugin, PARAM_GAIN, &raw mut v) });
    assert_eq!(v, 0.25, "gain event applied in order");
    assert!(unsafe { expect(&(*params).get_value)(plugin, PARAM_FREQ, &raw mut v) });
    assert_eq!(v, 7000.0);
    {
        let st = serial();
        assert_eq!(
            st.latencies_announced, 1,
            "gain set fired host.latency_changed"
        );
        assert_eq!(st.dirty_marks, 1, "gain set fired host.state.mark_dirty");
    }

    // gain 0.25 now: one more block must scale by exactly a quarter, still strict.
    let section2 = AudioSection::enter().expect("second section");
    let mut o2 = [-1.0f32; 64];
    let mut o3 = [-1.0f32; 64];
    let st = process_with(plugin, 64, &mut in0, &mut in1, &mut o2, &mut o3);
    let inside2 = section2.allocs_here();
    drop(section2);
    assert_eq!(st, CLAP_PROCESS_CONTINUE);
    assert_eq!(inside2, 0, "the scaled path allocates nothing either");
    for i in 0..64 {
        assert!(
            (o2[i] - in0[i] * 0.25).abs() < 1e-6,
            "gain 0.25 applied at sample {i}"
        );
        assert!(!o2[i].is_nan() && !o3[i].is_nan());
    }

    // 12. save_state → perturb → reload: bytes cross through the host stream.
    {
        let mut st = serial();
        st.saved_state.clear();
        st.load_cursor = 0;
    }
    assert!(
        unsafe { expect(&(*state).save)(plugin, &raw const MOCK_OSTREAM) },
        "state save succeeded"
    );
    {
        let st = serial();
        assert_eq!(
            st.saved_state.len(),
            20,
            "our 20-byte layout crossed intact"
        );
    }
    // perturb gain via a freq-free event list... gain event, new value:
    unsafe { expect(&(*params).flush)(plugin, &raw const PERTURB_LIST, &raw const NULL_OUT_LIST) };
    assert!(unsafe { expect(&(*params).get_value)(plugin, PARAM_GAIN, &raw mut v) });
    assert_eq!(v, 0.9, "perturbation applied before reload");
    // reload must restore the saved 0.25
    assert!(
        unsafe { expect(&(*state).load)(plugin, &raw const MOCK_ISTREAM) },
        "state load succeeded"
    );
    assert!(unsafe { expect(&(*params).get_value)(plugin, PARAM_GAIN, &raw mut v) });
    assert_eq!(v, 0.25, "reload restored the saved gain");
    assert!(unsafe { expect(&(*params).get_value)(plugin, PARAM_FREQ, &raw mut v) });
    assert_eq!(v, 7000.0, "freq survived save→perturb→load");

    // 13. on_main_thread forwards to the consumer which asks for restart.
    unsafe { expect(&(*plugin).on_main_thread)(plugin) };
    assert_eq!(
        serial().restart_requests,
        1,
        "host.request_restart reached the mock"
    );

    // 14. deactivate / process-after-deactivate / stop / destroy / deinit.
    unsafe { expect(&(*plugin).stop_processing)(plugin) };
    unsafe { expect(&(*plugin).deactivate)(plugin) };
    let after = process_with(plugin, 64, &mut in0, &mut in1, &mut out0, &mut out1);
    assert_eq!(
        after, CLAP_PROCESS_ERROR,
        "the consumer refuses process after deactivate (Error status)"
    );
    unsafe { expect(&(*plugin).reset)(plugin) };
    unsafe { expect(&(*plugin).destroy)(plugin) };
    unsafe { expect(&entry.deinit)() };
    assert!(
        !hathor_clap::is_initialized(),
        "the entry flag is clear after deinit"
    );
}

/// Direct create-path check without touching the entry init flag: a foreign id
/// is refused by `create_instance` itself (safe to run in parallel with the
/// lifecycle test — it never reads the global).
#[test]
fn create_refuses_foreign_ids_without_entry_state() {
    let p = unsafe {
        // SAFETY: `plugin_id` is a `clap_cstr!` static alive for the call; the
        // host pointer is our own static (and this call refuses before creating).
        create_instance::<GainPlugin>(
            &raw const MOCK_HOST,
            clap_cstr!("com.example.nope").as_ptr(),
        )
    };
    assert!(p.is_null(), "wrapper-level refusal of a foreign id");
    let p_null_host = unsafe {
        // SAFETY: null host is an allowed argument; `DESCRIPTOR.id` is the
        // static's own NUL-terminated string.
        create_instance::<GainPlugin>(core::ptr::null(), DESCRIPTOR.id)
    };
    assert!(
        !p_null_host.is_null(),
        "a null host still creates; init would refuse it instead"
    );
    // The instance token the host holds is `&base` inside the box; the box
    // address lives in `plugin_data`. Both must be non-null and the second must
    // actually point at (or before, since `base` is not offset 0) the same box —
    // a cheap sanity: dereference via plugin_data and read desc back.
    let init = unsafe { p_null_host.as_ref().and_then(|q| q.init) };
    let refused = unsafe { init.expect("init slot")(p_null_host) };
    assert!(!refused, "init with a null host is refused cleanly, no UB");
    // SAFETY: p_null_host is an instance we created and nobody else will destroy.
    unsafe {
        if let Some(destroy) = p_null_host.as_ref().and_then(|q| q.destroy) {
            destroy(p_null_host);
        }
    }
}
