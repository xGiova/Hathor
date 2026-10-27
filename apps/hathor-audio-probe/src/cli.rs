//! Command line of the probe: `std`-only parsing, no `clap`.
//!
//! ## Why hand-written
//!
//! The M0 contract forbids pulling a new crate without a wiring decision from the
//! lead, and the crate cache we build against holds exactly cpal, rtrb and the
//! library crates. A five-subcommand CLI is ~150 lines of parsing; a dependency
//! for it would be the wrong trade. Grammar is deliberately strict: an unknown
//! flag is an error, never a shrug, because this binary gets run once in front of
//! a human and a silently ignored `--frame 64` would produce a number nobody can
//! trust.
//!
//! ## Legacy spellings
//!
//! [ROADMAP.md](../../../ROADMAP.md) §8 names the probe surface `--list`,
//! `--offline`, `--stream`, `--bench`; the M0 build session asked for subcommands.
//! Both spellings mean the same thing, and `cli` is where that aliasing is decided
//! (so it is unit-tested, not folklore).

use core::fmt;

/// Smallest block the probe will pump: below this the per-block overhead
/// (`Instant`, guard enter/exit) dominates the number being measured.
pub const FRAMES_MIN: u32 = 8;
/// Largest block: above this the planar staging buffer stops being L1-resident and
/// the measurement changes meaning.
pub const FRAMES_MAX: u32 = 8192;
/// Sample rates the probe will even ask a device for.
pub const RATE_MIN: u32 = 8_000;
/// Upper bound of the probe's rate range (double the highest rate we plan for).
pub const RATE_MAX: u32 = 192_000;
/// Shortest live run: below this the WASAPI engine has not settled and the tail
/// statistics are noise.
pub const SECONDS_MIN: f64 = 0.5;
/// Longest live run (M0 criterion 4 needs 60 s; 600 s is the safety ceiling).
pub const SECONDS_MAX: f64 = 600.0;
/// The M0 acceptance loop is 20,000 cycles; the flag may go higher, never to 0.
pub const CYCLES_MIN: usize = 1;
/// Offline ceiling: 2,000,000 blocks is ~30 minutes of pump loop.
pub const CYCLES_MAX: usize = 2_000_000;
/// The spec's floor for a trustworthy median: at least 512 timed samples.
pub const ITERATIONS_MIN: usize = 512;
/// Bench ceiling (a median of 1M samples is a different project).
pub const ITERATIONS_MAX: usize = 262_144;
/// Smallest real-FFT the pump will run (the packing needs N ≥ 4).
pub const FFT_SIZE_MIN: usize = 4;
/// Largest real-FFT the pump will run in a block loop.
pub const FFT_SIZE_MAX: usize = 16_384;

/// What the process was asked to do. `PartialEq` only: [`Tone`] carries an `f32`,
/// so an `Eq` impl would be a lie about float reflexivity.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Print the usage text.
    Help,
    /// Print the version line.
    Version,
    /// Enumerate hosts/devices/configurations. Read-only: no device is opened.
    Devices,
    /// Open a live output stream. Refuses to touch hardware without consent.
    Run(RunOptions),
    /// Pump the same realtime task offline: no device, no driver.
    Offline(OfflineOptions),
    /// Offline cost measurement (the G1 numbers).
    Bench(BenchOptions),
}

/// The output signal the probe feeds into the chain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tone {
    /// Table sine at `hz` (the table is built at setup: no `sin` in the callback).
    Sine(f32),
    /// Digital silence: the DSP still runs, nothing is audible.
    Silence,
}

impl Default for Tone {
    fn default() -> Self {
        Tone::Sine(440.0)
    }
}

/// Options of `run` (the live G1 hardware test).
#[derive(Debug, Clone, PartialEq)]
pub struct RunOptions {
    /// Sample rate to ask the device for.
    pub rate: u32,
    /// DSP block shape, and the driver buffer size we request.
    pub frames: u32,
    /// How long to keep the stream running.
    pub seconds: f64,
    /// Signal into the chain.
    pub tone: Tone,
    /// Substring filter on the device name.
    pub device: Option<String>,
    /// The consent flag. `false` means the hardware is never opened.
    pub consent: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            rate: 48_000,
            frames: 64,
            seconds: 5.0,
            tone: Tone::default(),
            device: None,
            consent: false,
        }
    }
}

/// Options of `offline` (the same task, no device).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineOptions {
    /// Sample rate the task is shaped for.
    pub rate: u32,
    /// Block shape.
    pub frames: u32,
    /// Number of blocks to pump.
    pub cycles: usize,
    /// Real-FFT length.
    pub fft_size: usize,
}

impl Default for OfflineOptions {
    fn default() -> Self {
        Self {
            rate: 48_000,
            frames: 64,
            cycles: 20_000,
            fft_size: 1_024,
        }
    }
}

/// Options of `bench`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchOptions {
    /// Timed samples per row (the median is taken over this many).
    pub iterations: usize,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self { iterations: 512 }
    }
}

/// A rejected command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError(pub String);

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CliError {}

/// The usage text. Static: printing it must never depend on what we parsed.
pub const USAGE: &str = "\
hathor-audio-probe — the M0 realtime probe (contract + numbers)

USAGE:
    hathor-audio-probe <command> [options]

COMMANDS:
    devices                 list hosts, devices and supported output configs
                            (read-only, safe anywhere)
    run                     LIVE hardware test: opens the default output device,
                            pumps chain[lowpass, peak, highpass] + one real FFT
                            per block inside the allocation guard
                            REQUIRES --i-have-user-consent
    offline                 the same task with no device: N blocks, guard-verified
    bench                   offline cost measurement (biquad scalar vs AVX2, FFT,
                            transposes, the full pump block) — the G1 numbers
    help, --help            this text
    version                 version and build labels

OPTIONS:
    --frames <N>            block shape and requested driver buffer [8..=8192]
                            (default 64)
    --rate <HZ>             sample rate to request [8000..=192000] (default 48000)
    --seconds <S>           live run duration [0.5..=600] (default 5)
    --cycles <N>            offline block count [1..=2000000] (default 20000)
    --fft-size <N>          real FFT length, power of two [4..=16384] (default 1024)
    --tone <HZ|silence>     probe signal into the chain (default 440)
    --iterations <N>        timed samples per bench row [512..=262144] (default 512)
    --device <NAME>         substring match on the device name (run only)
    --i-have-user-consent   the only flag that lets `run` open hardware

LEGACY SPELLINGS (ROADMAP §8): --list = devices, --stream = run,
                              --offline = offline, --bench = bench
";

/// Parses an argument vector (without `argv[0]`).
///
/// # Errors
///
/// [`CliError`] for an unknown command, an unknown flag, a missing or out-of-range
/// value. The message names the flag, because the human reading it is at a console
/// with the driver already plugged in.
pub fn parse(argv: &[String]) -> Result<Command, CliError> {
    let Some(first) = argv.first() else {
        return Ok(Command::Help);
    };
    let mut it = argv.iter().skip(1);
    match first.as_str() {
        "help" | "--help" | "-h" => Ok(Command::Help),
        "version" | "--version" | "-V" => Ok(Command::Version),
        "devices" | "--list" => {
            reject_trailing(&mut it)?;
            Ok(Command::Devices)
        }
        "run" | "--stream" => parse_run(&mut it),
        "offline" | "--offline" => parse_offline(&mut it),
        "bench" | "--bench" => parse_bench(&mut it),
        other => Err(CliError(format!(
            "unknown command {other:?}; expected one of devices|run|offline|bench|help"
        ))),
    }
}

/// The flag name, rejecting the `--flag=value` form *before* any value is consumed:
/// half-parsing an equals form would turn a typo into a positional argument and a
/// measured number nobody asked for.
fn flag_name(arg: &'_ str) -> Result<&'_ str, CliError> {
    match arg.split_once('=') {
        Some((name, _)) => Err(CliError(format!(
            "{name}: use `--flag value` (space separated); `--flag=value` is not accepted"
        ))),
        None => Ok(arg),
    }
}

fn parse_run<'a, I>(it: &mut I) -> Result<Command, CliError>
where
    I: Iterator<Item = &'a String>,
{
    let mut o = RunOptions::default();
    while let Some(arg) = it.next() {
        let name = flag_name(arg)?;
        match name {
            "run" | "--stream" => {}
            "--frames" => o.frames = num(next_value(it, name)?, FRAMES_MIN, FRAMES_MAX, name)?,
            "--rate" => o.rate = num(next_value(it, name)?, RATE_MIN, RATE_MAX, name)?,
            "--seconds" => {
                o.seconds = float(next_value(it, name)?, SECONDS_MIN, SECONDS_MAX, name)?
            }
            "--tone" => o.tone = tone(next_value(it, name)?)?,
            "--device" => o.device = Some(next_value(it, name)?.to_string()),
            "--i-have-user-consent" => o.consent = true,
            other => return Err(unknown_flag(other, "run")),
        }
    }
    Ok(Command::Run(o))
}

fn parse_offline<'a, I>(it: &mut I) -> Result<Command, CliError>
where
    I: Iterator<Item = &'a String>,
{
    let mut o = OfflineOptions::default();
    while let Some(arg) = it.next() {
        let name = flag_name(arg)?;
        match name {
            "offline" | "--offline" => {}
            "--frames" => o.frames = num(next_value(it, name)?, FRAMES_MIN, FRAMES_MAX, name)?,
            "--rate" => o.rate = num(next_value(it, name)?, RATE_MIN, RATE_MAX, name)?,
            "--cycles" => {
                o.cycles = num(next_value(it, name)?, CYCLES_MIN as u32, CYCLES_MAX as u32, name)?
                    as usize;
            }
            "--fft-size" => {
                let n = num(
                    next_value(it, name)?,
                    FFT_SIZE_MIN as u32,
                    FFT_SIZE_MAX as u32,
                    name,
                )? as usize;
                if !n.is_power_of_two() {
                    return Err(CliError(format!(
                        "{name} must be a power of two, got {n} (RealFftPlan packs N/2 complex bins)"
                    )));
                }
                o.fft_size = n;
            }
            other => return Err(unknown_flag(other, "offline")),
        }
    }
    Ok(Command::Offline(o))
}

fn parse_bench<'a, I>(it: &mut I) -> Result<Command, CliError>
where
    I: Iterator<Item = &'a String>,
{
    let mut o = BenchOptions::default();
    while let Some(arg) = it.next() {
        let name = flag_name(arg)?;
        match name {
            "bench" | "--bench" => {}
            "--iterations" => {
                o.iterations = num(
                    next_value(it, name)?,
                    ITERATIONS_MIN as u32,
                    ITERATIONS_MAX as u32,
                    name,
                )? as usize;
            }
            other => return Err(unknown_flag(other, "bench")),
        }
    }
    Ok(Command::Bench(o))
}

fn next_value<'a, I>(it: &mut I, name: &str) -> Result<&'a str, CliError>
where
    I: Iterator<Item = &'a String>,
{
    it.next().map(String::as_str).ok_or_else(|| {
        CliError(format!("{name} expects a value (see `hathor-audio-probe help`)"))
    })
}

fn reject_trailing<'a, I>(it: &mut I) -> Result<(), CliError>
where
    I: Iterator<Item = &'a String>,
{
    match it.next() {
        Some(extra) => Err(CliError(format!(
            "devices takes no options, found {extra:?}"
        ))),
        None => Ok(()),
    }
}

fn unknown_flag(name: &str, command: &str) -> CliError {
    CliError(format!(
        "unknown flag {name:?} for `{command}`; see `hathor-audio-probe help`"
    ))
}

fn num(raw: &str, min: u32, max: u32, name: &str) -> Result<u32, CliError> {
    let v: u32 = raw.parse().map_err(|_| {
        CliError(format!(
            "{name} expects an integer, got {raw:?} (range {min}..={max})"
        ))
    })?;
    if v < min || v > max {
        return Err(CliError(format!(
            "{name} out of range: {v} not in {min}..={max}"
        )));
    }
    Ok(v)
}

fn float(raw: &str, min: f64, max: f64, name: &str) -> Result<f64, CliError> {
    let v: f64 = raw.parse().map_err(|_| {
        CliError(format!("{name} expects a number, got {raw:?} (range {min}..={max})"))
    })?;
    if !v.is_finite() || v < min || v > max {
        return Err(CliError(format!(
            "{name} out of range: {v} not in {min}..={max}"
        )));
    }
    Ok(v)
}

fn tone(raw: &str) -> Result<Tone, CliError> {
    if raw.eq_ignore_ascii_case("silence") {
        return Ok(Tone::Silence);
    }
    let hz: f32 = raw.parse().map_err(|_| {
        CliError(format!(
            "--tone expects Hz or `silence`, got {raw:?}"
        ))
    })?;
    if !hz.is_finite() || hz <= 0.0 || hz >= 20_000.0 {
        return Err(CliError(format!(
            "--tone out of range: {hz} Hz must be in (0, 20000)"
        )));
    }
    Ok(Tone::Sine(hz))
}

/// Where the parsed command came from, for the report header.
pub fn argv() -> Vec<String> {
    std::env::args().skip(1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bare_invocation_is_help_not_a_crash() {
        assert_eq!(parse(&args(&[])).ok(), Some(Command::Help));
    }

    #[test]
    fn devices_is_read_only_and_takes_nothing() {
        assert_eq!(parse(&args(&["devices"])).ok(), Some(Command::Devices));
        assert!(
            parse(&args(&["devices", "--frames", "64"])).is_err(),
            "a device listing that silently ignored --frames would lie about its scope"
        );
    }

    #[test]
    fn run_defaults_to_64_frames_at_48k_without_consent() {
        let Command::Run(o) = parse(&args(&["run"])).expect("parsed") else {
            panic!("expected the run command");
        };
        assert_eq!(o.frames, 64, "the M0 gate is a 64-frame block");
        assert_eq!(o.rate, 48_000, "the M0 gate is 48 kHz");
        assert!(!o.consent, "hardware needs an explicit opt-in");
        assert_eq!(o.tone, Tone::Sine(440.0));
    }

    #[test]
    fn run_accepts_every_documented_flag() {
        let c = parse(&args(&[
            "run",
            "--frames",
            "128",
            "--rate",
            "96000",
            "--seconds",
            "60",
            "--tone",
            "silence",
            "--device",
            "Focusrite",
            "--i-have-user-consent",
        ]))
        .expect("parsed");
        let want = RunOptions {
            frames: 128,
            rate: 96_000,
            seconds: 60.0,
            tone: Tone::Silence,
            device: Some("Focusrite".to_string()),
            consent: true,
        };
        assert_eq!(c, Command::Run(want));
    }

    #[test]
    fn legacy_spellings_map_to_the_same_commands() {
        assert_eq!(parse(&args(&["--list"])).ok(), Some(Command::Devices));
        assert!(matches!(
            parse(&args(&["--stream"])).ok(),
            Some(Command::Run(_))
        ));
        assert!(matches!(
            parse(&args(&["--offline"])).ok(),
            Some(Command::Offline(_))
        ));
        assert!(matches!(
            parse(&args(&["--bench"])).ok(),
            Some(Command::Bench(_))
        ));
    }

    #[test]
    fn unknown_command_and_unknown_flag_are_errors() {
        let e = parse(&args(&["devcies"])).err().expect("typo rejected");
        assert!(e.0.contains("unknown command"), "{e}");
        let e = parse(&args(&["bench", "--frames", "64"])).err().expect("wrong scope");
        assert!(e.0.contains("unknown flag"), "{e}");
    }

    #[test]
    fn missing_value_names_the_flag() {
        let e = parse(&args(&["run", "--frames"])).err().expect("dangling flag");
        assert!(e.0.contains("--frames"), "{e}");
    }

    #[test]
    fn equals_form_is_rejected_not_half_parsed() {
        // `--frames=64` would otherwise be read as a positional and accepted:
        // strictness here is what keeps a typo from becoming a measured number.
        let e = parse(&args(&["offline", "--frames=64"])).err().expect("equals form");
        assert!(e.0.contains("space separated"), "{e}");
    }

    #[test]
    fn out_of_range_values_are_rejected_with_the_range_in_the_message() {
        for (argv, needle) in [
            (args(&["run", "--frames", "0"]), "--frames out of range"),
            (args(&["run", "--frames", "99999"]), "--frames out of range"),
            (args(&["run", "--rate", "441000"]), "--rate out of range"),
            (args(&["run", "--seconds", "0.1"]), "--seconds out of range"),
            (args(&["run", "--tone", "0"]), "--tone out of range"),
            (args(&["offline", "--cycles", "0"]), "--cycles out of range"),
            (
                args(&["offline", "--fft-size", "1000"]),
                "power of two",
            ),
            (
                args(&["bench", "--iterations", "64"]),
                "--iterations out of range",
            ),
        ] {
            let owned: Vec<&str> = argv.iter().map(String::as_str).collect();
            let e = parse(&owned).err().unwrap_or_else(|| panic!("reject {owned:?}"));
            assert!(e.0.contains(needle), "{owned:?} gave {e}");
        }
    }

    #[test]
    fn non_numeric_values_are_rejected() {
        let e = parse(&args(&["run", "--frames", "sixtyfour"]))
            .err()
            .expect("not a number");
        assert!(e.0.contains("integer"), "{e}");
    }

    #[test]
    fn offline_defaults_are_the_m0_acceptance_loop() {
        let Command::Offline(o) = parse(&args(&["offline"])).expect("parsed") else {
            panic!("expected offline");
        };
        assert_eq!(o.cycles, 20_000, "ROADMAP M0 criterion 2 is 20,000 cycles");
        assert_eq!(o.frames, 64);
        assert_eq!(o.rate, 48_000);
        assert_eq!(o.fft_size, 1_024);
    }

    #[test]
    fn bench_median_floor_is_512_samples() {
        let Command::Bench(o) = parse(&args(&["bench"])).expect("parsed") else {
            panic!("expected bench");
        };
        assert_eq!(o.iterations, ITERATIONS_MIN);
    }

    #[test]
    fn version_and_help_are_addresses_not_values() {
        assert_eq!(parse(&args(&["version"])).ok(), Some(Command::Version));
        assert_eq!(parse(&args(&["-h"])).ok(), Some(Command::Help));
    }
}
