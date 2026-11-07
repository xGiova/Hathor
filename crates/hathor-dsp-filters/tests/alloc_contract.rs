//! Zero-allocation STRICT contract for the rotator/crossover Nodes.
//!
//! House pattern (identical to `hathor-dsp-fft/tests/alloc_contract.rs`): install
//! [`CheckedAllocator`] as the test-binary global allocator, serialize every test
//! that touches the process-global counters, enter an [`AudioSection`] around the
//! run-phase work, and assert `allocs_here() == 0`. Setup allocations are legal —
//! they are checked to be *visible* to the guard instead, proving the counter is
//! real and not a fiction.

use hathor_dsp_core::alloc_guard::{reset_counters, set_panic_on_audio_alloc};
use hathor_dsp_core::{AudioSection, CheckedAllocator, GraphHints, Node, ProcessCtx};
use hathor_dsp_filters::crossover::{CrossoverFamily, CrossoverPair};
use hathor_dsp_filters::phase_rotator::{params, PhaseRotator};
use std::hint::black_box;

#[global_allocator]
static GUARDED: CheckedAllocator = CheckedAllocator::new();

// The counter state is process-global: parallel tests would overwrite each
// other's publishes. Poison-safe `into_inner`: a panicking test already cleaned
// the globals in Drop, so we do not propagate a harmless failure.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn ctx(sr: u32, frames: usize) -> ProcessCtx {
    ProcessCtx::bare(sr, frames)
}

fn hints(ch: usize) -> GraphHints {
    GraphHints {
        max_channels: ch,
        max_frames: 64,
    }
}

#[test]
fn phase_rotator_process_does_not_allocate_inside_an_audio_section() {
    let _lock = serial();
    let mut rot = PhaseRotator::new(80.0, 8_000.0, 2).expect("ctor");
    let c = ctx(48_000, 64);
    rot.setup(&c, &hints(2)).expect("setup");

    let mut buf = hathor_dsp_core::ChannelBuffer::new(2, 64);
    let section = AudioSection::enter().expect("enter AudioSection");
    for k in 0..8u32 {
        // A parameter change *inside* the guarded section: the dirty path
        // (design + ramp_to onto existing lanes) is the one that must stay clean.
        if k % 3 == 0 {
            rot.set_param(params::ROTATION, black_box(k as f32 / 8.0));
        }
        let mut block = buf.block();
        for ch in block.channels_mut() {
            for (i, x) in ch.iter_mut().enumerate() {
                *x = ((i as f32) * 0.01).sin();
            }
        }
        rot.process(black_box(&c), black_box(&mut block))
            .expect("process");
    }
    let allocs = section.allocs_here();
    assert_eq!(allocs, 0, "rotator process allocated {allocs} times");
}

#[test]
fn phase_rotator_construction_allocations_are_visible_to_the_guard() {
    // Positive control: the guard is not a fiction. Measure mode (panic off)
    // because setup legitimately allocates — we are checking the counter sees
    // it, not that the section tolerates it. Same pattern as fft's contract.
    let _lock = serial();
    set_panic_on_audio_alloc(false);
    reset_counters();
    let counted = {
        let section = AudioSection::enter().expect("enter AudioSection");
        let mut rot = black_box(PhaseRotator::new(80.0, 8_000.0, 2).expect("ctor"));
        rot.setup(black_box(&ctx(48_000, 64)), black_box(&hints(2)))
            .expect("setup");
        let n = section.allocs_here();
        black_box(&rot);
        n
    };
    set_panic_on_audio_alloc(true);
    assert!(counted > 0, "setup allocated nothing? guard is a fiction");
}

#[test]
fn crossover_pair_process_does_not_allocate_inside_an_audio_section() {
    let _lock = serial();
    let mut pair = CrossoverPair::new(CrossoverFamily::Lr4, 1_000.0).expect("ctor");
    let c = ctx(48_000, 64);
    pair.setup(&c, &hints(2)).expect("setup");

    let mut in_lp = hathor_dsp_core::ChannelBuffer::new(2, 64);
    let mut in_hp = hathor_dsp_core::ChannelBuffer::new(2, 64);
    let section = AudioSection::enter().expect("enter AudioSection");
    for k in 0..8u32 {
        // Corner change inside the guard: the dirty path (one design + ramp onto
        // existing lanes, both halves) must be allocation-free.
        if k % 3 == 0 {
            pair.set_freq(black_box(500.0 + k as f32 * 90.0));
        }
        {
            let mut block = in_lp.block();
            for ch in block.channels_mut() {
                for (i, x) in ch.iter_mut().enumerate() {
                    *x = ((i as f32) * 0.02).cos();
                }
            }
            pair.lp_mut()
                .process(black_box(&c), black_box(&mut block))
                .expect("lp process");
        }
        {
            let mut block = in_hp.block();
            for ch in block.channels_mut() {
                for (i, x) in ch.iter_mut().enumerate() {
                    *x = ((i as f32) * 0.02).cos();
                }
            }
            pair.hp_mut()
                .process(black_box(&c), black_box(&mut block))
                .expect("hp process");
        }
    }
    let allocs = section.allocs_here();
    assert_eq!(allocs, 0, "crossover process allocated {allocs} times");
}

#[test]
fn crossover_setup_allocations_are_visible_to_the_guard() {
    // Positive control for the crossover lanes (same measure-mode pattern).
    let _lock = serial();
    set_panic_on_audio_alloc(false);
    reset_counters();
    let counted = {
        let section = AudioSection::enter().expect("enter AudioSection");
        let mut pair = black_box(CrossoverPair::new(CrossoverFamily::Bw2, 800.0).expect("ctor"));
        pair.setup(black_box(&ctx(48_000, 64)), black_box(&hints(2)))
            .expect("setup");
        let n = section.allocs_here();
        black_box(&pair);
        n
    };
    set_panic_on_audio_alloc(true);
    assert!(
        counted > 0,
        "crossover setup allocated nothing? guard is fiction"
    );
}
