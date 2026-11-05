//! ANCHOR measurement (ignored by default; run explicitly):
//! `cargo test -p hathor-analyzer --release --offline -- --ignored`
//!
//! Cost of the WHOLE plugin audio path — wrapper vtable, in/out view
//! handling, bit-exact pass-through, stereo downmix, bank `process` — per
//! 64-frame block at 48 kHz, measured here (the bank-only reference was 0.94%
//! of one core, measured 2026-11-05).

use std::hint::black_box;
use std::time::Instant;

use hathor_analyzer::AnalyzerPlugin;
use hathor_clap::sys::audio_buffer::clap_audio_buffer;
use hathor_clap::sys::plugin::clap_plugin;
use hathor_clap::sys::process::{clap_process, CLAP_PROCESS_CONTINUE};
use hathor_clap::{create_instance, Plugin};

const ROUNDS: usize = 4096;

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

#[test]
#[ignore = "timing probe: run explicitly with --release -- --ignored"]
fn whole_plugin_path_cost_per_block() {
    // A null host is fine for the measurement: the audio path never touches
    // it (param dirty-marking is not in this loop). `create_instance` is
    // audited wrapper code; we destroy the instance at the end.
    let plugin: *const clap_plugin = unsafe {
        // SAFETY: our own descriptor id string; a null host is allowed by the
        // wrapper (init is simply never called); the returned pointer is
        // destroyed exactly once below.
        create_instance::<AnalyzerPlugin>(
            core::ptr::null(),
            <AnalyzerPlugin as Plugin>::DESCRIPTOR.id,
        )
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
            activate(plugin, 48_000.0, 64, 64)
        },
        "activate on the measured instance"
    );

    let mut in0 = [0.5f32; 64];
    let mut in1 = [-0.25f32; 64];
    let mut out0 = [0f32; 64];
    let mut out1 = [0f32; 64];

    // Warm-up outside the clock: caches plus branch predictors.
    for _ in 0..512 {
        let st = call_process(
            plugin, process, 64, &mut in0, &mut in1, &mut out0, &mut out1,
        );
        assert_eq!(st, CLAP_PROCESS_CONTINUE);
    }

    // Measured loop: full block through the same vtable a host calls.
    let t = Instant::now();
    for _ in 0..ROUNDS {
        let st = call_process(
            plugin, process, 64, &mut in0, &mut in1, &mut out0, &mut out1,
        );
        assert_eq!(st, CLAP_PROCESS_CONTINUE);
        black_box(&out0);
    }
    let full = t.elapsed();

    // Zero-frame control: same call path, DSP skipped — the floor cost of
    // the wrapper + view setup alone.
    let t = Instant::now();
    for _ in 0..ROUNDS {
        let st = call_process(plugin, process, 0, &mut in0, &mut in1, &mut out0, &mut out1);
        assert_eq!(st, CLAP_PROCESS_CONTINUE);
        black_box(&out0);
    }
    let floor = t.elapsed();

    let budget_us = 64.0f64 / 48_000.0 * 1e6; // 1333.33 µs at 48 kHz
    let full_us = full.as_secs_f64() * 1e6 / ROUNDS as f64;
    let floor_us = floor.as_secs_f64() * 1e6 / ROUNDS as f64;
    println!(
        "MEASURED whole plugin path (vtable + pass-through + downmix + bank): {full_us:.2} µs/block @64 frames — {:.2}% of one core (budget {budget_us:.1} µs)",
        full_us / budget_us * 100.0
    );
    println!(
        "MEASURED wrapper+view floor (0-frame control): {floor_us:.2} µs/block ({:.3}%)",
        floor_us / budget_us * 100.0
    );
    println!("(reference, bank-only on this machine, 2026-11-05: 0.94% of one core)");

    let deactivate = instance.deactivate.expect("wired");
    // SAFETY: instance we created; deactivate then destroy, each once.
    unsafe { deactivate(plugin) };
    let destroy = instance.destroy.expect("wired");
    unsafe { destroy(plugin) };
}
