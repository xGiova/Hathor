//! Mock-host lifecycle for Hathor Analyzer — the official proxy for "this
//! plugin loads in a host" (tests the exported entry table, the factory, all
//! four wired extensions, pass-through bit-exactness, state save→perturb→load,
//! and the zero-allocation process loop under the workspace AudioSection).
//!
//! **It is a MOCK.** The closest thing to a DAW load that fits in a test. Real
//! Reaper loading is an OWNER step (build recipe in the report to the lead);
//! nothing here may be quoted as "verified in real hosts".
//!
//! Note this drives `hathor_analyzer::clap_entry` — the very static exported
//! by value at the symbol a host `dlsym`s for. We cannot exercise `dlsym`
//! itself offline-std, so this is the declared proxy.

use std::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, MutexGuard, OnceLock};

use hathor_analyzer::AnalyzerPlugin;
use hathor_clap::clap_cstr;
use hathor_clap::read_padded_name;
use hathor_clap::sys::audio_buffer::clap_audio_buffer;
use hathor_clap::sys::ext::audio_ports::{
    clap_audio_port_info, clap_host_audio_ports, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN,
    CLAP_EXT_AUDIO_PORTS, CLAP_PORT_STEREO,
};
use hathor_clap::sys::ext::latency::{clap_host_latency, clap_plugin_latency, CLAP_EXT_LATENCY};
use hathor_clap::sys::ext::log::{clap_host_log, CLAP_EXT_LOG};
use hathor_clap::sys::ext::params::{
    clap_host_params, clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS,
};
use hathor_clap::sys::ext::state::{clap_host_state, clap_plugin_state, CLAP_EXT_STATE};
use hathor_clap::sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use hathor_clap::sys::host::clap_host;
use hathor_clap::sys::plugin::{clap_plugin, clap_plugin_descriptor};
use hathor_clap::sys::process::{clap_process, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR};
use hathor_clap::sys::stream::{clap_istream, clap_ostream};
use hathor_clap::sys::version::{clap_version_is_compatible, CLAP_VERSION};

use hathor_dsp_core::{AudioSection, CheckedAllocator};

#[global_allocator]
static ALLOC: CheckedAllocator = CheckedAllocator::new();

// ---- host bookkeeping ----

#[derive(Default)]
struct HostState {
    log_lines: Vec<(i32, String)>,
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

// ---- mock host vtables ----

unsafe extern "C" fn host_log(_host: *const clap_host, severity: i32, message: *const c_char) {
    // SAFETY: test-controlled: `message` is a Rust `CStr` alive for the call
    // (built by the wrapper), `_host` is our own static.
    let msg = unsafe { CStr::from_ptr(message) };
    serial()
        .log_lines
        .push((severity, msg.to_string_lossy().into_owned()));
}

unsafe extern "C" fn host_mark_dirty(_host: *const clap_host) {
    serial().dirty_marks += 1;
}

unsafe extern "C" fn host_rescan(_host: *const clap_host, _flags: u32) {
    serial().rescans += 1;
}

unsafe extern "C" fn host_latency_changed(_host: *const clap_host) {}

unsafe extern "C" fn host_get_extension(
    _host: *const clap_host,
    id: *const c_char,
) -> *const c_void {
    // SAFETY: test-controlled: `id` is a static C string from the wrapper.
    let asked = unsafe { CStr::from_ptr(id) };
    if asked == CLAP_EXT_LOG {
        (&raw const MOCK_LOG).cast()
    } else if asked == CLAP_EXT_STATE {
        (&raw const MOCK_STATE).cast()
    } else if asked == CLAP_EXT_AUDIO_PORTS {
        (&raw const MOCK_AUDIO_PORTS).cast()
    } else if asked == CLAP_EXT_PARAMS {
        (&raw const MOCK_PARAMS).cast()
    } else if asked == CLAP_EXT_LATENCY {
        (&raw const MOCK_LATENCY).cast()
    } else {
        core::ptr::null()
    }
}

static MOCK_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};
static MOCK_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_mark_dirty),
};
static MOCK_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};
static MOCK_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: None,
    rescan: Some(host_rescan),
};
static MOCK_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_rescan),
    clear: None,
    request_flush: None,
};

static MOCK_HOST: clap_host = clap_host {
    clap_version: CLAP_VERSION,
    host_data: core::ptr::null_mut(),
    name: clap_cstr!("Mock Host").as_ptr(),
    vendor: clap_cstr!("Nobody").as_ptr(),
    url: clap_cstr!("").as_ptr(),
    version: clap_cstr!("0.0").as_ptr(),
    get_extension: Some(host_get_extension),
    request_restart: None,
    request_process: None,
    request_callback: None,
};

// ---- state streams ----

unsafe extern "C" fn stream_write(
    _stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    let n = size as usize;
    // SAFETY: the wrapper hands us an owned slice of `n` bytes for the call.
    let bytes = unsafe { core::slice::from_raw_parts(buffer.cast::<u8>(), n) };
    let mut st = serial();
    if st.saved_state.len() + n > 4096 {
        return -1;
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
    let take = n.min(st.saved_state.len() - st.load_cursor);
    // SAFETY: `buffer` is the wrapper's stack array; we fill `take` bytes.
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

static mut FLUSH_MAIN: Option<[hathor_clap::sys::events::clap_event_param_value; 2]> = None;
static mut PERTURB: Option<[hathor_clap::sys::events::clap_event_param_value; 1]> = None;

fn mk_event(id: u32, value: f64) -> hathor_clap::sys::events::clap_event_param_value {
    use hathor_clap::sys::events::{
        clap_event_header, clap_event_param_value, CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE,
    };
    clap_event_param_value {
        header: clap_event_header {
            size: core::mem::size_of::<clap_event_param_value>() as u32,
            time: 0,
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

fn fill_flush_events() {
    // SAFETY: called once at the top of the lifecycle test before any list
    // pointer is read; the value written replaces the initial `None`, so no
    // old value leaks.
    unsafe {
        core::ptr::write(
            &raw mut FLUSH_MAIN,
            Some([mk_event(0, -96.0), mk_event(2, 500.0)]),
        );
        core::ptr::write(&raw mut PERTURB, Some([mk_event(0, -40.0)]));
    }
}

unsafe extern "C" fn main_size(_l: *const hathor_clap::sys::events::clap_input_events) -> u32 {
    2
}

unsafe extern "C" fn main_get(
    l: *const hathor_clap::sys::events::clap_input_events,
    index: u32,
) -> *const hathor_clap::sys::events::clap_event_header {
    if l == core::ptr::from_ref(&MAIN_LIST) {
        // SAFETY: test storage filled above, never mutated after publication.
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

unsafe extern "C" fn perturb_size(_l: *const hathor_clap::sys::events::clap_input_events) -> u32 {
    1
}

unsafe extern "C" fn perturb_get(
    l: *const hathor_clap::sys::events::clap_input_events,
    index: u32,
) -> *const hathor_clap::sys::events::clap_event_header {
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

static MAIN_LIST: hathor_clap::sys::events::clap_input_events =
    hathor_clap::sys::events::clap_input_events {
        ctx: core::ptr::null_mut(),
        size: Some(main_size),
        get: Some(main_get),
    };
static PERTURB_LIST: hathor_clap::sys::events::clap_input_events =
    hathor_clap::sys::events::clap_input_events {
        ctx: core::ptr::null_mut(),
        size: Some(perturb_size),
        get: Some(perturb_get),
    };
static NULL_OUT_LIST: hathor_clap::sys::events::clap_output_events =
    hathor_clap::sys::events::clap_output_events {
        ctx: core::ptr::null_mut(),
        try_push: None,
    };

// ---- helpers ----

fn expect<T: Copy>(slot: &Option<T>) -> T {
    *slot
        .as_ref()
        .expect("mock host expected a vtable slot that is null")
}

fn ext_of(plugin: *const clap_plugin, id: &'static CStr) -> *const c_void {
    // SAFETY: raw get_extension call; `id` is a `clap_cstr!` static and
    // `plugin` is an instance we created.
    unsafe {
        plugin
            .as_ref()
            .and_then(|p| p.get_extension)
            .map_or(core::ptr::null(), |f| f(plugin, id.as_ptr()))
    }
}

fn process_block(
    plugin: *const clap_plugin,
    in0: &mut [f32; 64],
    in1: &mut [f32; 64],
    out0: &mut [f32; 64],
    out1: &mut [f32; 64],
    in_place: bool,
) -> i32 {
    let mut in_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
    let mut out_ptrs = if in_place {
        [in0.as_mut_ptr(), in1.as_mut_ptr()]
    } else {
        [out0.as_mut_ptr(), out1.as_mut_ptr()]
    };
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
        frames_count: 64,
        transport: core::ptr::null(),
        audio_inputs: &raw const in_buf,
        audio_outputs: &raw mut out_buf,
        audio_inputs_count: 1,
        audio_outputs_count: 1,
        in_events: core::ptr::null(),
        out_events: core::ptr::null(),
    };
    // SAFETY: host-shaped struct; all arrays are stack locals alive for the
    // call (this fn is the mock's own bookkeeping).
    unsafe {
        plugin
            .as_ref()
            .and_then(|p| p.process)
            .map_or(CLAP_PROCESS_ERROR, |f| {
                f(plugin, core::ptr::from_ref(&proc))
            })
    }
}

fn tone(i: usize) -> (f32, f32) {
    // 984.375 Hz = bin-exact for the 1024/4096/16384 tiers at 48 kHz.
    let x = std::f64::consts::TAU * 984.375f64 * i as f64 / 48_000.0f64;
    (x.sin() as f32 * 0.5, x.sin() as f32 * 0.5)
}

// ---- THE lifecycle ----

#[test]
fn analyzer_lifecycle_through_the_c_abi() {
    fill_flush_events();
    // The exported table: the same struct a host resolves at symbol `clap_entry`.
    let entry = &hathor_analyzer::clap_entry;

    assert!(clap_version_is_compatible(entry.clap_version));

    let init = expect(&entry.init);
    assert!(unsafe { init(core::ptr::null()) }, "init");
    assert!(
        !unsafe { init(core::ptr::null()) },
        "double init refused (wrapper policy)"
    );

    let get_factory = expect(&entry.get_factory);
    assert!(unsafe { get_factory(clap_cstr!("clap.nothing").as_ptr()) }.is_null());
    let factory_raw = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) };
    assert!(!factory_raw.is_null());
    let factory: *const clap_plugin_factory = factory_raw.cast();

    let count = unsafe { expect(&(*factory).get_plugin_count)(factory) };
    assert_eq!(count, 1, "this factory carries exactly the analyzer");
    let desc: *const clap_plugin_descriptor =
        unsafe { expect(&(*factory).get_plugin_descriptor)(factory, 0) };
    assert!(!desc.is_null());
    // SAFETY: static descriptor; fields are `clap_cstr!` pointers.
    let d = unsafe { &*desc };
    assert_eq!(
        unsafe { CStr::from_ptr(d.id) },
        clap_cstr!("dev.hathor.analyzer")
    );
    assert_eq!(
        unsafe { CStr::from_ptr(d.name) },
        clap_cstr!("Hathor Analyzer")
    );
    assert!(
        !d.features.is_null(),
        "the analyzer declares features; the whole table is pinned in the test below"
    );
    assert!(unsafe { expect(&(*factory).get_plugin_descriptor)(factory, 1) }.is_null());

    let create = unsafe { expect(&(*factory).create_plugin) };
    let wrong = unsafe { create(factory, &raw const MOCK_HOST, clap_cstr!("nope").as_ptr()) };
    assert!(wrong.is_null());
    let plugin: *const clap_plugin = unsafe { create(factory, &raw const MOCK_HOST, d.id) };
    assert!(!plugin.is_null());

    assert!(unsafe { expect(&(*plugin).init)(plugin) }, "plugin init");
    assert_eq!(serial().log_lines.len(), 1, "init logged once via host.log");
    assert_eq!(serial().log_lines[0].1, "hathor-analyzer init");

    // Extensions: the four wired ones answer; GUI does NOT exist (headless).
    let ports: *const clap_plugin_audio_ports = ext_of(plugin, CLAP_EXT_AUDIO_PORTS).cast();
    let params: *const clap_plugin_params = ext_of(plugin, CLAP_EXT_PARAMS).cast();
    let state: *const clap_plugin_state = ext_of(plugin, CLAP_EXT_STATE).cast();
    let latency: *const clap_plugin_latency = ext_of(plugin, CLAP_EXT_LATENCY).cast();
    assert!(
        !ports.is_null() && !params.is_null() && !state.is_null() && !latency.is_null(),
        "all four wired extensions answer"
    );
    assert!(
        ext_of(plugin, clap_cstr!("clap.gui")).is_null(),
        "NO clap_gui is registered: headless by design (design-before-code)"
    );

    // Ports: one main stereo each way, in-place pair 10<->20.
    assert_eq!(unsafe { expect(&(*ports).count)(plugin, true) }, 1);
    assert_eq!(unsafe { expect(&(*ports).count)(plugin, false) }, 1);
    let mut pin = clap_audio_port_info {
        id: 0,
        name: [0; 256],
        flags: 0,
        channel_count: 0,
        port_type: core::ptr::null(),
        in_place_pair: 0,
    };
    assert!(unsafe { expect(&(*ports).get)(plugin, 0, true, &raw mut pin) });
    assert_eq!(pin.id, 10);
    assert_eq!(pin.flags & CLAP_AUDIO_PORT_IS_MAIN, CLAP_AUDIO_PORT_IS_MAIN);
    assert_eq!(pin.channel_count, 2);
    assert_eq!(read_padded_name(&pin.name), "Main In");
    assert_eq!(unsafe { CStr::from_ptr(pin.port_type) }, CLAP_PORT_STEREO);
    assert_eq!(pin.in_place_pair, 20);
    assert!(unsafe { expect(&(*ports).get)(plugin, 0, false, &raw mut pin) });
    assert_eq!(pin.id, 20);
    assert_eq!(read_padded_name(&pin.name), "Main Out");
    assert_eq!(pin.in_place_pair, 10, "the pair points back at the input");

    // Params: 9, with the spec table visible to the host.
    assert_eq!(unsafe { expect(&(*params).count)(plugin) }, 9);
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
    assert!(unsafe { expect(&(*params).get_info)(plugin, 0, &raw mut pinfo) });
    assert_eq!(pinfo.id, 0);
    assert_eq!(read_padded_name(&pinfo.name), "Floor");
    assert_eq!(read_padded_name(&pinfo.module), "Display");
    assert_eq!((pinfo.min_value, pinfo.max_value), (-144.0, -20.0));
    assert_eq!(pinfo.default_value, -120.0);
    assert!(unsafe { expect(&(*params).get_info)(plugin, 5, &raw mut pinfo) });
    assert_eq!(read_padded_name(&pinfo.name), "Decay 256");
    assert_eq!(read_padded_name(&pinfo.module), "Smoothing");
    assert!(!unsafe { expect(&(*params).get_info)(plugin, 9, &raw mut pinfo) });
    let mut v = 0.0f64;
    assert!(unsafe { expect(&(*params).get_value)(plugin, 0, &raw mut v) });
    assert_eq!(v, -120.0, "floor default from the bank profile");
    let mut tbuf = [0i8; 256];
    assert!(unsafe { expect(&(*params).value_to_text)(plugin, 0, -96.0, tbuf.as_mut_ptr(), 256) });
    assert_eq!(
        unsafe { CStr::from_ptr(tbuf.as_ptr()) }
            .to_str()
            .expect("ascii"),
        "-96.00"
    );
    let mut parsed = 0.0;
    assert!(unsafe {
        expect(&(*params).text_to_value)(plugin, 0, clap_cstr!("2.5").as_ptr(), &raw mut parsed)
    });
    assert_eq!(parsed, 2.5);

    assert_eq!(
        unsafe { expect(&(*latency).get)(plugin) },
        0,
        "declared latency 0"
    );

    // activate refuses an unsupported rate (no panic — refusal), then succeeds.
    assert!(
        !unsafe { expect(&(*plugin).activate)(plugin, 8_000.0, 8, 64) },
        "8k refused"
    );
    assert!(unsafe { expect(&(*plugin).activate)(plugin, 48_000.0, 64, 64) });
    assert!(unsafe { expect(&(*plugin).start_processing)(plugin) });

    // Pass-through, bit-exact, over tone blocks; zero-allocation gate inside.
    let mut in0 = [0f32; 64];
    let mut in1 = [0f32; 64];
    for (i, (a, b)) in in0.iter_mut().zip(in1.iter_mut()).enumerate() {
        let (s, _) = tone(i);
        *a = s;
        *b = s;
    }
    let mut out0 = [-7.0f32; 64];
    let mut out1 = [-7.0f32; 64];
    let warm = process_block(plugin, &mut in0, &mut in1, &mut out0, &mut out1, false);
    assert_eq!(warm, CLAP_PROCESS_CONTINUE);
    assert_eq!(&out0[..], &in0[..], "pass-through bit-exact (warm-up)");

    let mut in0b = in0;
    let mut in1b = in1;
    let section = AudioSection::enter().expect("outermost section");
    let mut st = CLAP_PROCESS_ERROR;
    for _ in 0..8 {
        st = process_block(plugin, &mut in0b, &mut in1b, &mut out0, &mut out1, false);
    }
    let inside = section.allocs_here();
    drop(section);
    assert_eq!(st, CLAP_PROCESS_CONTINUE);
    assert_eq!(
        inside, 0,
        "zero allocations across 8 blocks in the audio section"
    );
    assert_eq!(
        &out0[..],
        &in0b[..],
        "pass-through bit-exact (post-gate blocks)"
    );
    assert_eq!(&out1[..], &in1b[..]);

    // In-place pair: the host hands the same rows; the plugin must not touch
    // what is already in place (and must not error).
    let mut inp = [0.25f32; 64];
    let mut inq = [-0.125f32; 64];
    let mut dummy = [-7.0f32; 64];
    let mut dummy2 = [-7.0f32; 64];
    let keep_l = inp;
    let keep_r = inq;
    let inplace_st = process_block(plugin, &mut inp, &mut inq, &mut dummy, &mut dummy2, true);
    assert_eq!(inplace_st, CLAP_PROCESS_CONTINUE, "in-place pair handled");
    assert_eq!(inp, keep_l, "in-place buffers untouched by the plugin");
    assert_eq!(inq, keep_r);

    // flush applies events in order through the default path.
    unsafe { expect(&(*params).flush)(plugin, &raw const MAIN_LIST, &raw const NULL_OUT_LIST) };
    assert!(unsafe { expect(&(*params).get_value)(plugin, 0, &raw mut v) });
    assert_eq!(v, -96.0, "floor param event applied");
    assert!(unsafe { expect(&(*params).get_value)(plugin, 2, &raw mut v) });
    assert_eq!(v, 500.0, "Avg 1024 applied");
    assert_eq!(
        serial().dirty_marks,
        2,
        "each saved-state-visible change marked dirty"
    );

    // save → perturb → load round trip over the host stream.
    {
        let mut st = serial();
        st.saved_state.clear();
        st.load_cursor = 0;
    }
    assert!(unsafe { expect(&(*state).save)(plugin, &raw const MOCK_OSTREAM) });
    assert_eq!(serial().saved_state.len(), 72, "9 f64 params = 72 bytes");
    unsafe { expect(&(*params).flush)(plugin, &raw const PERTURB_LIST, &raw const NULL_OUT_LIST) };
    assert!(unsafe { expect(&(*params).get_value)(plugin, 0, &raw mut v) });
    assert_eq!(v, -40.0, "perturbation took hold");
    assert!(unsafe { expect(&(*state).load)(plugin, &raw const MOCK_ISTREAM) });
    assert!(unsafe { expect(&(*params).get_value)(plugin, 0, &raw mut v) });
    assert_eq!(v, -96.0, "load restored the saved floor");
    assert!(unsafe { expect(&(*params).get_value)(plugin, 2, &raw mut v) });
    assert_eq!(v, 500.0, "load restored the saved Avg 1024");

    // Shutdown path. The analyzer never asks for a restart.
    assert_eq!(serial().restart_requests, 0);
    unsafe { expect(&(*plugin).reset)(plugin) };
    unsafe { expect(&(*plugin).stop_processing)(plugin) };
    unsafe { expect(&(*plugin).deactivate)(plugin) };
    let mut i0 = [0f32; 64];
    let mut i1 = [0f32; 64];
    let after = process_block(plugin, &mut i0, &mut i1, &mut out0, &mut out1, false);
    assert_eq!(
        after, CLAP_PROCESS_ERROR,
        "process refused outside activate"
    );
    unsafe { expect(&(*plugin).destroy)(plugin) };
    unsafe { expect(&entry.deinit)() };
    assert!(
        !hathor_clap::is_initialized(),
        "entry flag clear after deinit"
    );
}

/// The descriptor table IS the shipped identity: every string pinned
/// byte-for-byte here, read through the factory exactly as a host scanning
/// the library would. The `id` row is FOREVER-PINNED — hosts persist it into
/// saved projects — so this test is the tripwire that makes a rename loud
/// here instead of silently fatal in somebody's sessions.
///
/// This test deliberately does NOT call `init`/`deinit`: `get_factory` needs
/// no entry flag (wrapper policy, audited in task-3), which keeps this a pure
/// data-table check that can run in parallel with the lifecycle test.
#[test]
fn descriptor_ships_the_pinned_identity_byte_for_byte() {
    let entry = &hathor_analyzer::clap_entry;
    let get_factory = expect(&entry.get_factory);
    let raw = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) };
    assert!(!raw.is_null());
    let factory: *const clap_plugin_factory = raw.cast();
    let desc: *const clap_plugin_descriptor =
        unsafe { expect(&(*factory).get_plugin_descriptor)(factory, 0) };
    assert!(!desc.is_null());
    // SAFETY: static in the plugin, alive for the library's life; the fields
    // are `clap_cstr!` pointers by the wrapper's own contract.
    let d = unsafe { &*desc };
    let s = |p: *const c_char| -> String {
        // SAFETY: every field reached here is a static NUL-terminated string.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    };
    assert_eq!(d.clap_version, CLAP_VERSION, "ABI version stamped first");
    // id: forever. Editing the value below is a migration decision, not an
    // edit — see the definition site in src/analyzer.rs.
    assert_eq!(s(d.id), "dev.hathor.analyzer");
    assert_eq!(s(d.name), "Hathor Analyzer");
    assert_eq!(s(d.vendor), "Hathor");
    assert_eq!(s(d.url), "https://github.com/xGiova/Hathor");
    assert_eq!(s(d.manual_url), "https://github.com/xGiova/Hathor");
    assert_eq!(s(d.support_url), "https://github.com/xGiova/Hathor");
    assert_eq!(s(d.version), "0.1.0");
    assert_eq!(
        s(d.description),
        "Multi-resolution spectrum analyzer (headless v0; GUI pending design)."
    );
    assert!(!d.features.is_null(), "features are declared, not stubbed");
    // SAFETY: the shipped list is a static, NULL-terminated by construction.
    let (f0, f1, term) = unsafe { (s(*d.features), s(*d.features.add(1)), *d.features.add(2)) };
    assert_eq!(f0, "audio-effect");
    assert_eq!(f1, "analyzer");
    assert!(
        term.is_null(),
        "clap.h: the feature list ends with a null row"
    );
}

/// A foreign id never reaches `create_instance`; a null-host instance creates
/// but refuses `init` — without touching the process-wide init flag, so this
/// can run alongside the lifecycle test.
#[test]
fn create_paths_stay_clean() {
    use hathor_clap::create_instance;
    let bad = unsafe {
        // SAFETY: null id argument is an allowed (and refused) call.
        create_instance::<AnalyzerPlugin>(core::ptr::null(), core::ptr::null())
    };
    assert!(bad.is_null());
    // SAFETY: `DESCRIPTOR.id` is our own static C string; null host is allowed.
    let id = <AnalyzerPlugin as hathor_clap::Plugin>::DESCRIPTOR.id;
    let inst = unsafe { create_instance::<AnalyzerPlugin>(core::ptr::null(), id) };
    assert!(!inst.is_null(), "null host still creates");
    let refused = unsafe { expect(&inst.as_ref().expect("inst").init)(inst) };
    assert!(!refused, "init refuses a null host instead of misbehaving");
    // SAFETY: instance we created and nobody else will destroy.
    unsafe {
        if let Some(destroy) = inst.as_ref().and_then(|q| q.destroy) {
            destroy(inst);
        }
    }
}
