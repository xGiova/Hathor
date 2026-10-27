//! `hathor-audio-probe` — the M0 audio probe: a CLI that answers, with numbers from
//! this machine, whether a Rust graph can be pumped through a real Windows audio
//! device inside its budget without allocating once.
//!
//! Three subcommands, in the order a person should reach them:
//!
//! * `devices` — read-only enumeration of hosts, output devices and their config
//!   ranges. Safe anywhere, and the first thing to run: it shows what the probe will
//!   and will not be able to ask for.
//! * `bench` — the offline cost table (scalar biquad, AVX2 8-lane with and without the
//!   transpose, element-wise, the FFTs, the whole pump chunk) as measured wallclock.
//!   Safe anywhere.
//! * `offline` — 20,000 cycles of the live pump with the allocation guard in **strict**
//!   mode: the rehearsal that makes a single live run defensible. Safe anywhere.
//! * `run` — the live hardware test. **Refuses to touch a device** unless
//!   `--i-have-user-consent` is present, because it takes the machine's audio output
//!   and makes sound. Without the flag it prints `G1 run pending — lead session` and
//!   the exact plan it would have followed.
//!
//! Every number this binary prints is measured, and every measurement says what it
//! measured: block cost is **wallclock** from `std::time::Instant` pairs around the
//! chunk, outside-to-outside — not CPU cycles. The bench closes with the build profile
//! and the date it ran, so a table cannot outlive the machine it came from.
//!
//! ## Layer, in one paragraph
//!
//! This is an L5 application on top of L1/L2 crates: `hathor-dsp-core` for buffers and
//! the allocation guard, `hathor-dsp-filters` for the biquads, `hathor-dsp-fft` for the
//! transforms, `hathor-host-graph` for the chain and its scratch door, `cpal` for the
//! device and `rtrb` for the callback→control ring. It owns no DSP: if a kernel is
//! wrong here, the fix belongs in the crate. The only thing this binary invents is the
//! measurement.

use std::process::ExitCode;
use std::sync::Mutex;

use cpal::traits::DeviceTrait;
use hathor_dsp_core::alloc_guard::{self, CheckedAllocator};

use crate::probe_error::ProbeError;

mod bench;
mod cli;
mod counters;
mod cost_summary;
mod device_plan;
mod fft_window;
mod interleave;
mod listing;
mod live_stream;
mod offline_run;
mod probe_error;
mod pump;
mod run_report;
mod stamp;
mod timing_ring;
mod tone;

/// CONVENTION §2.5: every binary in this workspace installs the checked allocator, so
/// "no allocation in the audio callback" is an instrument and not an intention.
#[global_allocator]
static GUARD: CheckedAllocator = CheckedAllocator::new();

/// Serialises the tests that touch the process-global guard state. This is a single
/// binary, so all its tests share one allocator and one mode flag; without this, a
/// `strict` rehearsal and a `measure` pump test could race and make a zero meaningless.
/// Repo pattern: `LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)`.
#[cfg(test)]
pub(crate) fn guard_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn main() -> ExitCode {
    match dispatch(&cli::argv()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `main` is the only place allowed to turn an error into a status: the
            // library code returns `Result`, never calls `exit`.
            eprintln!("error [{}]: {e}", e.tag());
            ExitCode::FAILURE
        }
    }
}

/// Runs one command. Everything that can fail returns; nothing here decides policy.
fn dispatch(argv: &[String]) -> Result<(), ProbeError> {
    match cli::parse(argv)? {
        cli::Command::Help => {
            print!("{}", cli::USAGE);
            Ok(())
        }
        cli::Command::Version => {
            println!(
                "hathor-audio-probe {} (M0 probe, {} build)",
                env!("CARGO_PKG_VERSION"),
                stamp::profile()
            );
            Ok(())
        }
        cli::Command::Devices => {
            let hosts = listing::collect()?;
            let wants = device_plan::Wants {
                rate: cli::RunOptions::default().rate,
                frames: cli::RunOptions::default().frames,
            };
            for line in listing::render(&hosts, wants.rate, wants.frames) {
                println!("{line}");
            }
            Ok(())
        }
        cli::Command::Run(o) => {
            if !o.consent {
                // The gate. Loud, informative, and it changes nothing on the machine.
                println!("G1 run pending — lead session");
                println!();
                println!("`run` opens an audio output device and makes sound, so it will not");
                println!("start without --i-have-user-consent. This is a live hardware test");
                println!("that belongs to a session with a human at the machine; the device is");
                println!("exclusive in practice and the artefact is the console output.");
                println!();
                println!("What --i-have-user-consent would do, in order:");
                for line in live_stream::consent_steps(&o) {
                    println!("{line}");
                }
                return Ok(());
            }
            let host = cpal::default_host();
            let device = live_stream::select_device(&host, o.device.as_deref())?;
            let name = device
                .name()
                .map_err(|e| ProbeError::Device(format!("device name: {e}")))?;
            let configs = live_stream::configs_of(&device)?;
            let plan = live_stream::decide(
                &configs,
                &device_plan::Wants {
                    rate: o.rate,
                    frames: o.frames,
                },
            )?;
            // Printed before the stream exists, so the choice can still be cancelled.
            for line in live_stream::plan_report(&name, &plan, &o) {
                println!("{line}");
            }
            println!();
            let r = live_stream::execute(&o, plan, device, name)?;
            for line in r.lines() {
                println!("{line}");
            }
            if r.passed() {
                return Ok(());
            }
            Err(ProbeError::Gate(format!(
                "{}",
                live_stream::verdict_summary(&r.checks())
            )))
        }
        cli::Command::Offline(o) => {
            let r = offline_run::execute(&o)?;
            for line in r.lines() {
                println!("{line}");
            }
            if r.passed() {
                return Ok(());
            }
            Err(ProbeError::Gate(format!(
                "offline rehearsal failed: {}",
                live_stream::verdict_summary(&r.checks())
            )))
        }
        cli::Command::Bench(o) => bench::execute(&o),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal is the whole point of the gate: no device, no sound, and the reason
    /// printed. Tested by calling `dispatch`, which is the only path `main` has.
    #[test]
    fn run_without_consent_refuses_and_names_the_lead_session() {
        let argv: Vec<String> = ["hathor-audio-probe", "run"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        // `dispatch` prints; the assertion is that it returns Ok (a refusal is not an
        // error) without ever reaching cpal. If it had, this test would need a device.
        assert!(
            dispatch(&argv).is_ok(),
            "a refusal must exit 0: the command worked, it was simply not authorised"
        );
    }

    #[test]
    fn help_and_version_are_parseable_and_never_touch_a_device() {
        for flag in ["--help", "-h", "help", "--version"] {
            let argv = vec!["hathor-audio-probe".to_string(), flag.to_string()];
            assert!(dispatch(&argv).is_ok(), "{flag}");
        }
    }

    #[test]
    fn a_bad_command_line_is_a_usage_error_not_a_panic() {
        let argv = vec![
            "hathor-audio-probe".to_string(),
            "run".to_string(),
            "--frames".to_string(),
            "0".to_string(),
        ];
        let e = dispatch(&argv).err().expect("refused");
        assert_eq!(e.tag(), "usage", "{e}");
    }

    #[test]
    fn the_guard_is_installed_and_reads_zero_when_nothing_ran() {
        let _s = guard_serial();
        alloc_guard::set_panic_on_audio_alloc(false);
        alloc_guard::reset_counters();
        assert_eq!(alloc_guard::allocs_total(), 0, "the binary's own instrument");
        assert!(!alloc_guard::in_audio_section());
    }
}
