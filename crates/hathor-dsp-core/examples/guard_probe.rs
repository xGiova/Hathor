//! Probe (not a test): does the guard panic + `catch_unwind` survive the release
//! optimizer? Run:
//!   cargo run --release -p hathor-dsp-core --example guard_probe -- vec
//!   cargo run --release -p hathor-dsp-core --example guard_probe -- push
//! Prints the four facts the failing unit test cannot report from inside itself.

use hathor_dsp_core::alloc_guard::{
    allocs_last_section, in_audio_section, reset_counters, set_panic_on_audio_alloc,
};
use hathor_dsp_core::{AudioSection, CheckedAllocator};

#[global_allocator]
static PROBE: CheckedAllocator = CheckedAllocator::new();

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "vec".to_string());
    // Warm the stdout handle while the thread is idle: the lazy stderr/stdout
    // init allocates, and an allocation here would be a false positive.
    println!("[probe] mode={mode}");
    set_panic_on_audio_alloc(true);
    reset_counters();
    let r = std::panic::catch_unwind(|| {
        let g = AudioSection::enter().expect("enter");
        match mode.as_str() {
            "push" => {
                // Deliberate vec_init_then_push: this arm exists to prove the
                // push path is unwind-CATCHABLE where the vec![..] (Box) arm is
                // not — the lint fix would delete the subject of the probe.
                #[allow(clippy::vec_init_then_push)]
                {
                    let mut v: Vec<u8> = Vec::new();
                    v.push(7);
                    drop(v);
                }
            }
            _ => {
                let _v: Vec<u8> = std::hint::black_box(vec![7]);
            }
        }
        let n = g.allocs_here();
        drop(g);
        println!("[probe] closure reached the end, allocs_here={n}");
    });
    println!("[probe] catch_unwind returned err(panicked)={}", r.is_err());
    println!("[probe] in_audio_section={}", in_audio_section());
    println!("[probe] allocs_last_section={}", allocs_last_section());
    println!("[probe] done");
}
