//! ANCHOR measurement (ignored by default; run explicitly):
//! `cargo test -p hathor-eq --release --offline -- --ignored --nocapture`
//!
//! Cost of the WHOLE plugin audio path — wrapper vtable, in/out view
//! handling, the copy, the 8-band biquad cascade, the master trim — per
//! 64-frame block, at BOTH 48 kHz (budget 1333.33 µs) and 96 kHz (budget
//! 666.67 µs). Two shapes per rate: the shipping default (all bands Off —
//! the "silent cost" every host pays for the instance) and the worst case
//! (all 8 bands active). M2 acceptance is "<1% of a core at 96 kHz": the
//! MEASURED lines below are the evidence, never a promise made here.
//!
//! Bands are opened through the REAL host path — a `clap.params` flush of
//! param-value events, not a Rust shortcut: what gets measured is exactly
//! what a host drives. (There is no `set` on the params vtable: hosts set
//! params by events. This bench obeys.)
//!
//! CONTESTED-NUMBERS NOTICE: taken while four parallel builds were running
//! on this machine (the 2026-11-07 fan-out). Functional gates are unaffected
//! by contention; these µs figures are labeled and the lead re-measures on a
//! quiet machine at the final gate.

use std::ffi::c_void;
use std::hint::black_box;
use std::time::Instant;

use hathor_clap::sys::audio_buffer::clap_audio_buffer;
use hathor_clap::sys::events::{
    clap_event_header, clap_event_param_value, clap_input_events, CLAP_CORE_EVENT_SPACE_ID,
    CLAP_EVENT_PARAM_VALUE,
};
use hathor_clap::sys::ext::params::{clap_plugin_params, CLAP_EXT_PARAMS};
use hathor_clap::sys::plugin::clap_plugin;
use hathor_clap::sys::process::{clap_process, CLAP_PROCESS_CONTINUE};
use hathor_clap::{create_instance, Plugin};
use hathor_eq::EqPlugin;

const ROUNDS: usize = 4096;

fn mk_event(id: u32, value: f64) -> clap_event_param_value {
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

/// An owned, stack-friendly event list: the vtable callbacks recover `Self`
/// through `ctx`, exactly like the mock host's statics do (here nothing
/// outlives the measured case, and the callee only reads during the call).
struct Evs {
    evs: [clap_event_param_value; 32],
}

unsafe extern "C" fn evs_size(l: *const clap_input_events) -> u32 {
    // SAFETY: `ctx` of every list we build points at the `Evs` that owns it,
    // alive for the duration of the call (this is the bench's own invariant).
    let evs = unsafe { &*(*l).ctx.cast::<Evs>() };
    evs.evs.len() as u32
}

unsafe extern "C" fn evs_get(l: *const clap_input_events, index: u32) -> *const clap_event_header {
    // SAFETY: as above.
    let evs = unsafe { &*(*l).ctx.cast::<Evs>() };
    evs.evs
        .get(index as usize)
        .map_or(core::ptr::null(), |e| core::ptr::from_ref(e).cast())
}

/// One host-shaped `process` call on `plugin` with 2×64 buffers. All raw
/// pointers die with this frame: legal for the duration of the call, exactly
/// what a real host's struct does.
fn call_process(
    plugin: *const clap_plugin,
    process: unsafe extern "C" fn(*const clap_plugin, *const clap_process) -> i32,
    frames: u32,
    in0: &mut [f32; 64],
    in1: &mut [f32; 64],
    out0: &mut [f32; 64],
    out1: &mut [f32; 64],
) -> i32 {
    let mut inp = [in0.as_mut_ptr(), in1.as_mut_ptr()];
    let mut outp = [out0.as_mut_ptr(), out1.as_mut_ptr()];
    let in_buf = clap_audio_buffer {
        data32: inp.as_mut_ptr(),
        data64: core::ptr::null_mut(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let mut out_buf = clap_audio_buffer {
        data32: outp.as_mut_ptr(),
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
    // SAFETY: raw pointers into this frame only; the callee (the wrapper)
    // uses them strictly for the duration of the call.
    unsafe { process(plugin, core::ptr::from_ref(&proc)) }
}

/// The 64-bit twin of [`call_process`]: `data64` rows only. Same borrowing
/// discipline, same lifetime: everything here dies with the call.
fn call_process64(
    plugin: *const clap_plugin,
    process: unsafe extern "C" fn(*const clap_plugin, *const clap_process) -> i32,
    frames: u32,
    in0: &mut [f64; 64],
    in1: &mut [f64; 64],
    out0: &mut [f64; 64],
    out1: &mut [f64; 64],
) -> i32 {
    let mut inp = [in0.as_mut_ptr(), in1.as_mut_ptr()];
    let mut outp = [out0.as_mut_ptr(), out1.as_mut_ptr()];
    let in_buf = clap_audio_buffer {
        data32: core::ptr::null_mut(),
        data64: inp.as_mut_ptr(),
        channel_count: 2,
        latency: 0,
        constant_mask: 0,
    };
    let mut out_buf = clap_audio_buffer {
        data32: core::ptr::null_mut(),
        data64: outp.as_mut_ptr(),
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
    // SAFETY: raw pointers into this frame only; the wrapper uses them
    // strictly for the duration of the call.
    unsafe { process(plugin, core::ptr::from_ref(&proc)) }
}

/// Open all 8 bands through the params extension's `flush`, as a host does.
fn wake_the_bands(plugin: *const clap_plugin) {
    let instance = unsafe {
        // SAFETY: pointer is checked non-null by the caller before we get
        // here; it was created by `create_instance` in `run_case`.
        plugin.as_ref().expect("non-null instance")
    };
    let ext = unsafe {
        // SAFETY: raw C-ABI call on our own live instance with a static
        // extension id string; the returned table is static.
        instance.get_extension.expect("wired")(plugin, CLAP_EXT_PARAMS.as_ptr())
    };
    assert!(!ext.is_null(), "params extension must answer");
    let params: *const clap_plugin_params = ext.cast();
    let mut k = 0usize;
    let mut evs = [mk_event(0, 0.0); 32];
    for b in 0..8u32 {
        let base = 2 + b * 4;
        // A different kind per band (LP..Shelf cycle) + real values: every
        // lane becomes a live recursion, none a disguised identity.
        evs[k] = mk_event(base, (b % 6 + 1) as f64);
        evs[k + 1] = mk_event(base + 1, 110.0 * (b as f64 + 1.0));
        evs[k + 2] = mk_event(base + 2, 3.0 - b as f64);
        evs[k + 3] = mk_event(base + 3, 0.9);
        k += 4;
    }
    let owned = Evs { evs };
    let list = clap_input_events {
        ctx: core::ptr::from_ref(&owned).cast_mut().cast::<c_void>(),
        size: Some(evs_size),
        get: Some(evs_get),
    };
    let flush = unsafe {
        // SAFETY: `params` non-null (checked), our own static extension
        // table; `list` and `owned` live until this function returns.
        (*params).flush.expect("wired")
    };
    unsafe {
        // SAFETY: valid plugin, valid lists per the invariant above; a null
        // output list is the allowed "nothing to push back" shape.
        flush(plugin, core::ptr::from_ref(&list), core::ptr::null())
    };
    // The events are consumed; `owned` dies here — the wrapper never keeps
    // the pointers past the call (audited in task-3's flush trampoline).
}

/// Create an instance through the audited wrapper path and run one measured
/// case: `rate` Hz, bands all active if `full_bands`, f64 rows if `wide`.
fn run_case(rate: f64, full_bands: bool, wide: bool) {
    let budget_us = 64.0f64 / rate * 1e6;
    let plugin: *const clap_plugin = unsafe {
        // SAFETY: our own descriptor id string; a null host is allowed by
        // the wrapper (init is simply never called); the returned pointer
        // is destroyed exactly once below.
        create_instance::<EqPlugin>(core::ptr::null(), <EqPlugin as Plugin>::DESCRIPTOR.id)
    };
    assert!(!plugin.is_null());
    let instance = unsafe {
        // SAFETY: pointer just checked non-null, created by us.
        plugin.as_ref().expect("non-null")
    };
    let activate = instance.activate.expect("wired vtable");
    let process = instance.process.expect("wired vtable");
    assert!(
        unsafe {
            // SAFETY: `plugin` is this instance; plain arguments.
            activate(plugin, rate, 64, 64)
        },
        "activate on the measured instance"
    );
    if full_bands {
        wake_the_bands(plugin);
    }

    let mut in0 = [0.5f32; 64];
    let mut in1 = [-0.25f32; 64];
    let mut out0 = [0f32; 64];
    let mut out1 = [0f32; 64];
    let mut d0 = [0.5f64; 64];
    let mut d1 = [-0.25f64; 64];
    let mut e0 = [0f64; 64];
    let mut e1 = [0f64; 64];

    macro_rules! block {
        ($frames:expr) => {
            if wide {
                call_process64(plugin, process, $frames, &mut d0, &mut d1, &mut e0, &mut e1)
            } else {
                call_process(
                    plugin, process, $frames, &mut in0, &mut in1, &mut out0, &mut out1,
                )
            }
        };
    }

    // Warm-up outside the clock: caches, branch predictors, and the 128-frame
    // coefficient ramp the full-bands case just started.
    for _ in 0..512 {
        let st = block!(64);
        assert_eq!(st, CLAP_PROCESS_CONTINUE);
    }

    let t = Instant::now();
    for _ in 0..ROUNDS {
        let st = block!(64);
        assert_eq!(st, CLAP_PROCESS_CONTINUE);
        if wide {
            black_box(&e0);
        } else {
            black_box(&out0);
        }
    }
    let full_us = t.elapsed().as_secs_f64() * 1e6 / ROUNDS as f64;

    // Zero-frame control: same call path, DSP skipped — the wrapper+view
    // floor, so the DSP share is (full − floor).
    let t = Instant::now();
    for _ in 0..ROUNDS {
        let st = block!(0);
        assert_eq!(st, CLAP_PROCESS_CONTINUE);
        if wide {
            black_box(&e0);
        } else {
            black_box(&out0);
        }
    }
    let floor_us = t.elapsed().as_secs_f64() * 1e6 / ROUNDS as f64;

    let shape = if full_bands {
        "8 bands ACTIVE"
    } else {
        "all bands off"
    };
    let width = if wide { "f64" } else { "f32" };
    println!(
        "MEASURED eq path [{shape} {width}] @{rate:.0} Hz: {full_us:.2} µs/block @64f — {:.2}% of one core (budget {budget_us:.1} µs; wrapper+view floor {floor_us:.2} µs)",
        full_us / budget_us * 100.0
    );

    let deactivate = instance.deactivate.expect("wired");
    // SAFETY: instance we created; deactivate then destroy, each once.
    unsafe { deactivate(plugin) };
    let destroy = instance.destroy.expect("wired");
    unsafe { destroy(plugin) };
}

#[test]
#[ignore = "timing probe: run explicitly with --release -- --ignored --nocapture"]
fn eq_path_cost_per_block() {
    run_case(48_000.0, false, false);
    run_case(48_000.0, true, false);
    run_case(96_000.0, false, false);
    run_case(96_000.0, true, false);
    // The 64-bit host (task-11): convert + filter + convert back. The
    // neutral case exercises the ratified invariant branch (direct f64
    // copy, scratch untouched); the active cases pay the declared detour.
    run_case(48_000.0, false, true);
    run_case(48_000.0, true, true);
    run_case(96_000.0, false, true);
    run_case(96_000.0, true, true);
}
