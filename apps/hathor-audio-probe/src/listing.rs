//! `devices`: what the machine says it has. Read-only, safe to run anywhere.
//!
//! ## Why this is a subcommand and not a footnote
//!
//! The gate that matters (`run`) refuses to guess a device: it opens the *default*
//! output and prints the config it chose. Before a human lets that happen they need
//! to see what the drivers actually report — the rate ranges, the buffer ranges, the
//! formats — because those numbers are what `run` will be judged against. This
//! command never opens a stream, never writes to a device, and can be run on a
//! machine with no speakers.
//!
//! The `render` half is pure (`Vec<DeviceSummary>` → lines of text) so the shape of
//! the output is unit-tested; the `collect` half is the cpal calls and nothing else.

use cpal::traits::{DeviceTrait, HostTrait};

use crate::device_plan::{BufferLimit, ConfigInfo, StreamFormat};
use crate::probe_error::ProbeError;

/// One host, and whether it answered at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSummary {
    /// Host name as cpal spells it (`WASAPI`, `DirectSound`, …).
    pub name: String,
    /// False when `Host::from_id` failed: the API is there, the runtime is not.
    pub available: bool,
    /// Devices found under this host.
    pub devices: Vec<DeviceSummary>,
    /// Failure text when `available` is false or enumeration errored.
    pub note: Option<String>,
}

/// One device and what it claims about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSummary {
    /// Host it belongs to.
    pub host: String,
    /// Its name, or `name() `error text.
    pub name: String,
    /// True for the machine's default output device.
    pub is_default: bool,
    /// Output-capable (this probe only cares about outputs).
    pub is_output: bool,
    /// The configs it reported, in the device's own order.
    pub configs: Vec<ConfigInfo>,
    /// Set when enumeration errored for this device (it is disconnected, typically).
    pub note: Option<String>,
}

impl DeviceSummary {
    /// The config `plan_output` would be handed for a `--frames 64 --rate 48000` run,
    /// as the one-line answer to "what would happen".
    pub fn preview_plan(&self, rate: u32, frames: u32) -> Option<String> {
        let wants = crate::device_plan::Wants { rate, frames };
        let p = crate::device_plan::plan_output(&self.configs, &wants).ok()?;
        Some(format!(
            "would open: {} ch @ {} Hz ({}), driver buffer {}, DSP chunk {} frames",
            p.channels,
            p.rate,
            p.format.label(),
            match p.buffer {
                crate::device_plan::BufferRequest::Fixed(n) => n.to_string(),
                crate::device_plan::BufferRequest::Default => "engine default".to_string(),
            },
            p.chunk_frames,
        ))
    }
}

/// Formats the summaries the way `devices` prints them.
///
/// `rate`/`frames` are the probe's defaults, used for the per-device plan preview:
/// the point is to show what `run` would do *before* anyone runs `run`.
#[must_use]
pub fn render(hosts: &[HostSummary], rate: u32, frames: u32) -> Vec<String> {
    let mut out = Vec::new();
    if hosts.is_empty() {
        out.push("no audio hosts were found on this machine".to_string());
        return out;
    }
    for h in hosts {
        out.push(format!(
            "host {}: {}",
            h.name,
            if h.available {
                "available"
            } else {
                "NOT available"
            }
        ));
        if let Some(n) = &h.note {
            out.push(format!("  note: {n}"));
        }
        if h.available && h.devices.is_empty() {
            out.push("  (no devices enumerated)".to_string());
        }
        for d in &h.devices {
            let mark = if d.is_default {
                " [DEFAULT OUTPUT]"
            } else {
                ""
            };
            let kind = if d.is_output { "output" } else { "input-only" };
            out.push(format!("  {} ({}){}", d.name, kind, mark));
            if let Some(n) = &d.note {
                out.push(format!("    note: {n}"));
                continue;
            }
            if d.configs.is_empty() {
                out.push("    (reported no output configurations)".to_string());
                continue;
            }
            for (i, c) in d.configs.iter().enumerate() {
                out.push(format!("    [{i}] {}", render_config(c)));
            }
            if let Some(p) = d.preview_plan(rate, frames) {
                out.push(format!("    {p}"));
            }
        }
    }
    out
}

/// One config, in the device's own numbers.
fn render_config(c: &ConfigInfo) -> String {
    let buffer = match c.buffer {
        BufferLimit::Range { .. } if c.buffer.is_free_rein() => {
            "buffer 0..=max (free rein: shared mode)".to_string()
        }
        BufferLimit::Range { min, max } => format!("buffer {min}..={max} frames"),
        BufferLimit::Unknown => "buffer unknown until opened".to_string(),
    };
    let rates = if c.min_rate == c.max_rate {
        format!("{} Hz", c.min_rate)
    } else {
        format!("{}..={} Hz", c.min_rate, c.max_rate)
    };
    format!(
        "{} ch, {}, {}, {}",
        c.channels,
        rates,
        buffer,
        format_label(c.format)
    )
}

fn format_label(f: StreamFormat) -> String {
    match f {
        StreamFormat::Unsupported => "format not usable by this probe".to_string(),
        other => format!("{} native", other.label()),
    }
}

/// Enumerates hosts and their output devices through cpal.
///
/// Every `Result` is turned into a [`HostSummary`]/[`DeviceSummary`] with a `note`
/// rather than propagated: a machine where DirectSound is missing but WASAPI works
/// must still print the devices it has. That is the difference between a probe and a
/// crash.
pub fn collect() -> Result<Vec<HostSummary>, ProbeError> {
    let host_ids = cpal::available_hosts();
    if host_ids.is_empty() {
        return Err(ProbeError::Device(
            "cpal reported no audio hosts at all".to_string(),
        ));
    }
    let default = cpal::default_host().default_output_device();
    let default_name = default.as_ref().and_then(|d| d.name().ok());
    let mut hosts = Vec::new();
    for id in host_ids {
        let host_name = id.name().to_string();
        // `host_from_id` takes the id by value and fails when the backend is not
        // present on this machine — a normal answer on Windows, where the compiled-in
        // list is longer than the installed one.
        match cpal::host_from_id(id) {
            Err(_) => hosts.push(HostSummary {
                name: host_name,
                available: false,
                devices: Vec::new(),
                note: Some("the host is compiled in but could not be created".to_string()),
            }),
            Ok(host) => {
                let devices = match host.output_devices() {
                    Err(e) => {
                        let note = Some(format!("device enumeration failed: {e}"));
                        hosts.push(HostSummary {
                            name: host_name,
                            available: true,
                            devices: Vec::new(),
                            note,
                        });
                        continue;
                    }
                    Ok(devices) => devices
                        .map(|d| {
                            let name = d.name().unwrap_or_else(|e| e.to_string());
                            let configs = d
                                .supported_output_configs()
                                .map(|c| c.map(to_info).collect())
                                .unwrap_or_default();
                            DeviceSummary {
                                is_default: default_name.as_ref() == Some(&name),
                                host: host_name.clone(),
                                name,
                                is_output: true,
                                configs,
                                note: None,
                            }
                        })
                        .collect(),
                };
                hosts.push(HostSummary {
                    name: host_name,
                    available: true,
                    devices,
                    note: None,
                });
            }
        }
    }
    Ok(hosts)
}

/// The one-line cpal→probe translation. A field per statement, so it can be read
/// without a sound card attached.
fn to_info(range: cpal::SupportedStreamConfigRange) -> ConfigInfo {
    ConfigInfo {
        channels: range.channels(),
        min_rate: range.min_sample_rate().0,
        max_rate: range.max_sample_rate().0,
        buffer: match range.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => BufferLimit::Range {
                min: *min,
                max: *max,
            },
            cpal::SupportedBufferSize::Unknown => BufferLimit::Unknown,
        },
        format: match range.sample_format() {
            cpal::SampleFormat::F32 => StreamFormat::F32,
            cpal::SampleFormat::I16 => StreamFormat::I16,
            _ => StreamFormat::Unsupported,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_plan::StreamFormat::F32;

    fn cfg(ch: u16, lo: u32, hi: u32) -> ConfigInfo {
        ConfigInfo {
            channels: ch,
            min_rate: lo,
            max_rate: hi,
            buffer: BufferLimit::Range {
                min: 0,
                max: u32::MAX,
            },
            format: F32,
        }
    }

    fn device(name: &str, default: bool) -> DeviceSummary {
        DeviceSummary {
            host: "WASAPI".to_string(),
            name: name.to_string(),
            is_default: default,
            is_output: true,
            configs: vec![cfg(2, 44_100, 192_000)],
            note: None,
        }
    }

    #[test]
    fn an_empty_machine_says_so_instead_of_printing_nothing() {
        assert_eq!(
            render(&[], 48_000, 64),
            vec!["no audio hosts were found on this machine".to_string()]
        );
    }

    #[test]
    fn a_host_that_did_not_answer_is_labelled_not_skipped() {
        let h = HostSummary {
            name: "DirectSound".to_string(),
            available: false,
            devices: Vec::new(),
            note: Some("no runtime".to_string()),
        };
        let lines = render(&[h], 48_000, 64);
        assert!(
            lines[0].contains("host DirectSound: NOT available"),
            "{lines:?}"
        );
        assert!(lines[1].contains("no runtime"), "{lines:?}");
    }

    #[test]
    fn an_available_host_with_no_devices_says_that() {
        let h = HostSummary {
            name: "WASAPI".to_string(),
            available: true,
            devices: Vec::new(),
            note: None,
        };
        assert!(render(&[h], 48_000, 64)
            .iter()
            .any(|l| l.contains("no devices enumerated")));
    }

    #[test]
    fn a_config_line_carries_channels_rates_buffer_and_format() {
        let lines = render(
            &[HostSummary {
                name: "WASAPI".to_string(),
                available: true,
                devices: vec![device("Speakers", true)],
                note: None,
            }],
            48_000,
            64,
        );
        let all = lines.join("\n");
        assert!(all.contains("[DEFAULT OUTPUT]"), "{all}");
        assert!(all.contains("2 ch"), "{all}");
        assert!(all.contains("44100..=192000 Hz"), "{all}");
        assert!(all.contains("free rein"), "{all}");
        assert!(all.contains("f32 native"), "{all}");
    }

    #[test]
    fn the_preview_line_shows_what_run_would_do() {
        let d = device("Speakers", false);
        let p = d.preview_plan(48_000, 64).expect("a plan");
        assert!(p.contains("would open: 2 ch @ 48000 Hz"), "{p}");
        assert!(p.contains("driver buffer 64"), "{p}");
        assert!(p.contains("DSP chunk 64 frames"), "{p}");
    }

    #[test]
    fn a_device_with_no_configs_prints_a_line_not_a_blank() {
        let mut d = device("Ghost", false);
        d.configs.clear();
        let lines = render(
            &[HostSummary {
                name: "WASAPI".to_string(),
                available: true,
                devices: vec![d],
                note: None,
            }],
            48_000,
            64,
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("reported no output configurations")),
            "{lines:?}"
        );
    }

    #[test]
    fn an_erroring_device_keeps_its_note_and_no_config_table() {
        let mut d = device("Disconnected", false);
        d.note = Some("device unavailable".to_string());
        let lines = render(
            &[HostSummary {
                name: "WASAPI".to_string(),
                available: true,
                devices: vec![d],
                note: None,
            }],
            48_000,
            64,
        );
        assert!(
            lines.iter().any(|l| l.contains("note: device unavailable")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_single_rate_config_is_not_printed_as_a_range() {
        let c = ConfigInfo {
            channels: 2,
            min_rate: 48_000,
            max_rate: 48_000,
            buffer: BufferLimit::Unknown,
            format: StreamFormat::I16,
        };
        let line = render_config(&c);
        assert!(
            line.contains("48000 Hz, buffer unknown until opened"),
            "{line}"
        );
        assert!(line.contains("i16 native"), "{line}");
    }

    #[test]
    fn an_unsupported_format_is_named_as_unusable_not_f32() {
        let c = ConfigInfo {
            format: StreamFormat::Unsupported,
            ..cfg(2, 48_000, 48_000)
        };
        assert!(render_config(&c).contains("not usable by this probe"));
    }
}
