//! Mock-host lifecycle for Hathor EQ — the official proxy for "this
//! plugin loads in a host" (tests the exported entry table, the factory, all
//! four wired extensions, neutral-chain bit-exactness, state save→perturb→load,
//! and the zero-allocation process loop under the workspace AudioSection).
//!
//! **It is a MOCK.** The closest thing to a DAW load that fits in a test. Real
//! Reaper loading is an OWNER step (build recipe in the report to the lead);
//! nothing here may be quoted as "verified in real hosts".
//!
//! Note this drives `hathor_eq::clap_entry` — the very static exported
//! by value at the symbol a host `dlsym`s for. We cannot exercise `dlsym`
//! itself offline-std, so this is the declared proxy.

use std::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, MutexGuard, OnceLock};

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
use hathor_eq::EqPlugin;

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
            Some([mk_event(0, 1.0), mk_event(3, 440.0)]),
        );
        core::ptr::write(&raw mut PERTURB, Some([mk_event(0, 0.0)]));
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

// All-8-bands engagement list for the strict hot-path test: the REAL host
// door (params flush), not a Rust shortcut.

static mut HOT: Option<[hathor_clap::sys::events::clap_event_param_value; 32]> = None;

fn fill_hot_events() {
    let mut evs = [mk_event(0, 0.0); 32];
    let mut k = 0usize;
    for b in 0..8u32 {
        let base = 2 + b * 4;
        evs[k] = mk_event(base, (b % 6 + 1) as f64); // a live kind per band
        evs[k + 1] = mk_event(base + 1, 110.0 * (b as f64 + 1.0));
        evs[k + 2] = mk_event(base + 2, 3.0 - b as f64);
        evs[k + 3] = mk_event(base + 3, 0.9);
        k += 4;
    }
    // SAFETY: written once inside `serial()` before the list is read; the
    // value replaces the initial `None`, no old value leaks.
    unsafe { core::ptr::write(&raw mut HOT, Some(evs)) };
}

unsafe extern "C" fn hot_size(_l: *const hathor_clap::sys::events::clap_input_events) -> u32 {
    32
}

unsafe extern "C" fn hot_get(
    l: *const hathor_clap::sys::events::clap_input_events,
    index: u32,
) -> *const hathor_clap::sys::events::clap_event_header {
    if l == core::ptr::from_ref(&HOT_LIST) {
        // SAFETY: test storage filled under `serial()`, never mutated after
        // publication.
        unsafe {
            match (*core::ptr::addr_of!(HOT))
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

static HOT_LIST: hathor_clap::sys::events::clap_input_events =
    hathor_clap::sys::events::clap_input_events {
        ctx: core::ptr::null_mut(),
        size: Some(hot_size),
        get: Some(hot_get),
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

/// The 64-bit twin of [`process_block`]: `data64` rows only, `data32` null.
fn process_block64(
    plugin: *const clap_plugin,
    in0: &mut [f64; 64],
    in1: &mut [f64; 64],
    out0: &mut [f64; 64],
    out1: &mut [f64; 64],
    in_place: bool,
) -> i32 {
    let mut in_ptrs = [in0.as_mut_ptr(), in1.as_mut_ptr()];
    let mut out_ptrs = if in_place {
        [in0.as_mut_ptr(), in1.as_mut_ptr()]
    } else {
        [out0.as_mut_ptr(), out1.as_mut_ptr()]
    };
    let in_buf = clap_audio_buffer {
        data32: core::ptr::null_mut(),
        data64: in_ptrs.as_mut_ptr(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let mut out_buf = clap_audio_buffer {
        data32: core::ptr::null_mut(),
        data64: out_ptrs.as_mut_ptr(),
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

/// Drive the same `Plugin::process` directly on a typed instance through a
/// fabricated view. Identical function to what the vtable reaches — used
/// ONLY to read the neutral-fast flag, which the C door cannot expose.
fn drive_typed(
    p: &mut EqPlugin,
    in0: &mut [f32; 64],
    in1: &mut [f32; 64],
    out0: &mut [f32; 64],
    out1: &mut [f32; 64],
) -> hathor_clap::ProcessStatus {
    use hathor_clap::{Plugin as _, ProcessView};
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
        frames_count: 64,
        transport: core::ptr::null(),
        audio_inputs: &raw const in_buf,
        audio_outputs: &raw mut out_buf,
        audio_inputs_count: 1,
        audio_outputs_count: 1,
        in_events: core::ptr::null(),
        out_events: core::ptr::null(),
    };
    // SAFETY: stack locals alive for the call; well-formed by construction
    // (the same discipline as the unit tests' `drive`).
    let mut view = unsafe { ProcessView::from_sys(&proc, 48_000.0) };
    p.process(&mut view)
}

fn tone(i: usize) -> (f32, f32) {
    // 984.375 Hz = bin-exact for the 1024/4096/16384 tiers at 48 kHz.
    let x = std::f64::consts::TAU * 984.375f64 * i as f64 / 48_000.0f64;
    (x.sin() as f32 * 0.5, x.sin() as f32 * 0.5)
}

// ---- THE lifecycle ----

#[test]
fn eq_lifecycle_through_the_c_abi() {
    fill_flush_events();
    // The exported table: the same struct a host resolves at symbol `clap_entry`.
    let entry = &hathor_eq::clap_entry;

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
    assert_eq!(count, 1, "this factory carries exactly the eq");
    let desc: *const clap_plugin_descriptor =
        unsafe { expect(&(*factory).get_plugin_descriptor)(factory, 0) };
    assert!(!desc.is_null());
    // SAFETY: static descriptor; fields are `clap_cstr!` pointers.
    let d = unsafe { &*desc };
    assert_eq!(unsafe { CStr::from_ptr(d.id) }, clap_cstr!("dev.hathor.eq"));
    assert_eq!(unsafe { CStr::from_ptr(d.name) }, clap_cstr!("Hathor EQ"));
    assert!(
        !d.features.is_null(),
        "the EQ declares features; the whole table is pinned in the test below"
    );
    assert!(unsafe { expect(&(*factory).get_plugin_descriptor)(factory, 1) }.is_null());

    let create = unsafe { expect(&(*factory).create_plugin) };
    let wrong = unsafe { create(factory, &raw const MOCK_HOST, clap_cstr!("nope").as_ptr()) };
    assert!(wrong.is_null());
    let plugin: *const clap_plugin = unsafe { create(factory, &raw const MOCK_HOST, d.id) };
    assert!(!plugin.is_null());

    assert!(unsafe { expect(&(*plugin).init)(plugin) }, "plugin init");
    assert_eq!(serial().log_lines.len(), 1, "init logged once via host.log");
    assert_eq!(serial().log_lines[0].1, "hathor-eq init");

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

    // Params: 34, with the spec table visible to the host.
    assert_eq!(unsafe { expect(&(*params).count)(plugin) }, 34);
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
    assert_eq!(read_padded_name(&pinfo.name), "Bypass");
    assert_eq!(read_padded_name(&pinfo.module), "Global");
    assert_eq!((pinfo.min_value, pinfo.max_value), (0.0, 1.0));
    assert_eq!(pinfo.default_value, 0.0);
    assert!(unsafe { expect(&(*params).get_info)(plugin, 5, &raw mut pinfo) });
    assert_eq!(read_padded_name(&pinfo.name), "Q");
    assert_eq!(read_padded_name(&pinfo.module), "Band 1");
    assert!(!unsafe { expect(&(*params).get_info)(plugin, 34, &raw mut pinfo) });
    let mut v = 0.0f64;
    assert!(unsafe { expect(&(*params).get_value)(plugin, 0, &raw mut v) });
    assert_eq!(
        v, 0.0,
        "bypass defaults off: the neutral chain is the shipping default"
    );
    let mut tbuf = [0i8; 256];
    assert!(unsafe { expect(&(*params).value_to_text)(plugin, 1, -9.5, tbuf.as_mut_ptr(), 256) });
    assert_eq!(
        unsafe { CStr::from_ptr(tbuf.as_ptr()) }
            .to_str()
            .expect("ascii"),
        "-9.50"
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
        !unsafe { expect(&(*plugin).activate)(plugin, 4_000.0, 8, 64) },
        "4k refused (EQ envelope starts at 8 kHz)"
    );
    assert!(unsafe { expect(&(*plugin).activate)(plugin, 48_000.0, 64, 64) });
    assert!(unsafe { expect(&(*plugin).start_processing)(plugin) });

    // Neutral chain, bit-exact, over tone blocks; zero-allocation gate inside.
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
    assert_eq!(&out0[..], &in0[..], "neutral chain bit-exact (warm-up)");

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
        "neutral chain bit-exact (post-gate blocks)"
    );
    assert_eq!(&out1[..], &in1b[..]);

    // In-place pair: the host hands the same rows. With the neutral fast
    // path (task-11) the plugin does not touch the rows at all — "untouched"
    // means literally UNWRITTEN now, and the assert below proves exactly
    // that. Before the fast path it meant bit-identical content rewritten by
    // identity biquads; `hot_bands_pass_the_strict_guard_at_both_widths` is
    // what keeps the WRITING path under the strict guard.
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
    assert_eq!(v, 1.0, "bypass event applied");
    assert!(unsafe { expect(&(*params).get_value)(plugin, 3, &raw mut v) });
    assert_eq!(v, 440.0, "band 1 freq applied");
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
    assert_eq!(serial().saved_state.len(), 272, "34 f64 params = 272 bytes");
    unsafe { expect(&(*params).flush)(plugin, &raw const PERTURB_LIST, &raw const NULL_OUT_LIST) };
    assert!(unsafe { expect(&(*params).get_value)(plugin, 0, &raw mut v) });
    assert_eq!(v, 0.0, "perturbation took hold");
    assert!(unsafe { expect(&(*state).load)(plugin, &raw const MOCK_ISTREAM) });
    assert!(unsafe { expect(&(*params).get_value)(plugin, 0, &raw mut v) });
    assert_eq!(v, 1.0, "load restored the saved bypass");
    assert!(unsafe { expect(&(*params).get_value)(plugin, 3, &raw mut v) });
    assert_eq!(v, 440.0, "load restored the saved band 1 freq");

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
    let entry = &hathor_eq::clap_entry;
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
    // edit — see the definition site in src/eq.rs.
    assert_eq!(s(d.id), "dev.hathor.eq");
    assert_eq!(s(d.name), "Hathor EQ");
    assert_eq!(s(d.vendor), "Hathor");
    assert_eq!(s(d.url), "https://github.com/xGiova/Hathor");
    assert_eq!(s(d.manual_url), "https://github.com/xGiova/Hathor");
    assert_eq!(s(d.support_url), "https://github.com/xGiova/Hathor");
    assert_eq!(s(d.version), "0.1.0");
    assert_eq!(
        s(d.description),
        "8-band parametric EQ (headless v0; GUI pending design)."
    );
    assert!(!d.features.is_null(), "features are declared, not stubbed");
    // SAFETY: the shipped list is a static, NULL-terminated by construction.
    let (f0, f1, term) = unsafe { (s(*d.features), s(*d.features.add(1)), *d.features.add(2)) };
    assert_eq!(f0, "audio-effect");
    assert_eq!(f1, "equalizer");
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
        create_instance::<EqPlugin>(core::ptr::null(), core::ptr::null())
    };
    assert!(bad.is_null());
    // SAFETY: `DESCRIPTOR.id` is our own static C string; null host is allowed.
    let id = <EqPlugin as hathor_clap::Plugin>::DESCRIPTOR.id;
    let inst = unsafe { create_instance::<EqPlugin>(core::ptr::null(), id) };
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

/// The STRICT guard must cover the REAL DSP path, not the copy shortcut:
/// all 8 bands engaged through the host door (params flush), then 8+8
/// guarded blocks — f32 rows and f64 rows — under `AudioSection`. A typed
/// probe asserts the neutral-fast flag stays DOWN while a band is alive and
/// re-arms only after a settle back to neutral: the flag is the proof-of-
/// path the guard needs (task-11 follow-up demanded by the lead — a guard
/// that passes covering LESS is worse than one that fails).
#[test]
fn hot_bands_pass_the_strict_guard_at_both_widths() {
    use hathor_clap::{create_instance, Plugin as _, ProcessStatus};
    let _st = serial(); // the mock host's globals; HOT publication included
    fill_hot_events();
    let id = <EqPlugin as hathor_clap::Plugin>::DESCRIPTOR.id;
    let plugin = unsafe {
        // SAFETY: our own descriptor id; null host is allowed (no logging
        // path runs here); the instance is destroyed exactly once below.
        create_instance::<EqPlugin>(core::ptr::null(), id)
    };
    assert!(!plugin.is_null());
    assert!(
        unsafe { expect(&plugin.as_ref().expect("inst").activate)(plugin, 48_000.0, 64, 64) },
        "activate on the hot instance"
    );
    let params: *const clap_plugin_params = ext_of(plugin, CLAP_EXT_PARAMS).cast();
    assert!(!params.is_null(), "params extension must answer");
    unsafe {
        // SAFETY: valid instance, our own static lists; null output list is
        // the allowed shape.
        expect(&(*params).flush)(plugin, &raw const HOT_LIST, &raw const NULL_OUT_LIST)
    };

    // Settle the 128-frame ramps OUTSIDE the section (warm-up may breathe;
    // the guarded blocks must not allocate).
    let mut a = [0.25f32; 64];
    let mut b = [-0.125f32; 64];
    let mut c = [0.0f32; 64];
    let mut d = [0.0f32; 64];
    for _ in 0..4 {
        assert_eq!(
            process_block(plugin, &mut a, &mut b, &mut c, &mut d, false),
            CLAP_PROCESS_CONTINUE
        );
    }
    let sec = AudioSection::enter().expect("outermost section");
    let mut st = CLAP_PROCESS_ERROR;
    for _ in 0..8 {
        st = process_block(plugin, &mut a, &mut b, &mut c, &mut d, false);
    }
    let f32_allocs = sec.allocs_here();
    drop(sec);
    assert_eq!(st, CLAP_PROCESS_CONTINUE);
    assert_eq!(
        f32_allocs, 0,
        "hot f32 recursions: zero allocations over 8 blocks"
    );

    // The same at double width: convert-in / filter / convert-out through
    // the activate-sized scratch — zero fresh allocations is the claim.
    let mut a64 = [0.25f64; 64];
    let mut b64 = [-0.125f64; 64];
    let mut c64 = [0.0f64; 64];
    let mut d64 = [0.0f64; 64];
    for _ in 0..4 {
        assert_eq!(
            process_block64(plugin, &mut a64, &mut b64, &mut c64, &mut d64, false),
            CLAP_PROCESS_CONTINUE
        );
    }
    let sec = AudioSection::enter().expect("outermost section");
    let mut st = CLAP_PROCESS_ERROR;
    for _ in 0..8 {
        st = process_block64(plugin, &mut a64, &mut b64, &mut c64, &mut d64, false);
    }
    let f64_allocs = sec.allocs_here();
    drop(sec);
    assert_eq!(st, CLAP_PROCESS_CONTINUE);
    assert_eq!(
        f64_allocs, 0,
        "hot f64 scratch detour: zero allocations over 8 blocks"
    );

    let destroy = unsafe {
        // SAFETY: `plugin` is the live instance we created.
        expect(&plugin.as_ref().expect("inst").destroy)
    };
    // SAFETY: instance we created; destroyed exactly once.
    unsafe { destroy(plugin) };

    // Proof-of-path on the typed object (the flag is invisible through the
    // vtable — same `process`, one extra window into its state).
    let mut q = EqPlugin::instantiate();
    assert!(q.activate(48_000.0, 64, 64));
    assert!(
        q.is_neutral_fast(),
        "fresh neutral instance: fast flag armed"
    );
    q.set_param_value(2, 1.0); // band 1 LowPass
    q.set_param_value(3, 1_000.0);
    assert!(
        !q.is_neutral_fast(),
        "engagement must clear the flag immediately"
    );
    let mut ta = [0.25f32; 64];
    let mut tb = [0.25f32; 64];
    let mut tc = [0.0f32; 64];
    let mut td = [0.0f32; 64];
    for _ in 0..6 {
        assert_eq!(
            drive_typed(&mut q, &mut ta, &mut tb, &mut tc, &mut td),
            ProcessStatus::Continue
        );
    }
    assert!(
        !q.is_neutral_fast(),
        "a live band never re-arms the flag: the recursion is what ran"
    );
    // The return trip: off means ramping first, settled copy only later.
    q.set_param_value(2, 0.0); // Type -> Off: ramp back to IDENTITY
    assert!(!q.is_neutral_fast(), "the return trip ramps: still hot");
    for _ in 0..6 {
        assert_eq!(
            drive_typed(&mut q, &mut ta, &mut tb, &mut tc, &mut td),
            ProcessStatus::Continue
        );
    }
    assert!(
        q.is_neutral_fast(),
        "settled neutral re-arms the copy fast path (flag tracks reality)"
    );
}
