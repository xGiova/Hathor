//! The config-choice logic, with cpal kept outside the file.
//!
//! ## Why this exists as pure code
//!
//! `run` has exactly one shot at a device, in front of a human, on a machine whose
//! driver lies about what it supports. The decision — which of the reported configs
//! to open, at what rate, asking for what buffer — is therefore the part of the
//! binary that *must* be testable without hardware. Everything here speaks in plain
//! data ([`ConfigInfo`], [`Wants`], [`DevicePlan`]) so a unit test can hand it a fake
//! device list and check what the probe would have done.
//!
//! The cpal→[`ConfigInfo`] translation lives in `live_stream.rs` and is one
//! statement per field: a translation you can read in ten lines is a translation you
//! can trust without a sound card.
//!
//! ## The one decision that is not obvious
//!
//! `chunk_frames` (the DSP block shape) and the requested *driver* buffer are two
//! different numbers. A WASAPI shared-mode stream keeps the audio engine's own
//! period: requesting 64 frames does not give you 64-frame callbacks, and 64/48k is
//! the shape the M0 gate has to measure anyway. So the probe requests the nearest
//! legal driver buffer **and** pumps the chain in `chunk_frames`, reporting both.
//! Collapsing the two is how a probe ends up measuring 480-frame blocks while
//! believing it measured 64.

use core::fmt;

/// What the sample format is to this binary. cpal's own enum is `#[non_exhaustive]`
/// and carries types the probe cannot feed a planar f32 chain with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    /// Native `f32`: no conversion in the callback.
    F32,
    /// `i16`: the probe converts at the driver edge and says so.
    I16,
    /// Anything else: refused, with the name printed.
    Unsupported,
}

impl StreamFormat {
    /// Label for the report (`Unsupported` keeps cpal's debug text upstream).
    pub fn label(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::I16 => "i16",
            Self::Unsupported => "unsupported",
        }
    }

    /// Preference for the probe: f32 first (no edge conversion cost), i16 second.
    const fn rank(self) -> Option<u8> {
        match self {
            Self::F32 => Some(0),
            Self::I16 => Some(1),
            Self::Unsupported => None,
        }
    }
}

/// What a device says about buffer sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferLimit {
    /// Reported min/max in frames. WASAPI's software stack answers `0..=u32::MAX`
    /// ("free rein"); the engine's period still wins in shared mode.
    Range {
        /// Smallest legal buffer in frames.
        min: u32,
        /// Largest legal buffer in frames.
        max: u32,
    },
    /// The host cannot tell us before the stream exists.
    Unknown,
}

impl BufferLimit {
    /// True when the range is the "we do not constrain you" answer of a software
    /// audio stack: the request is legal, the *delivered* period is not decided here.
    pub fn is_free_rein(&self) -> bool {
        matches!(self, Self::Range { min, max } if *min <= 1 && *max == u32::MAX)
    }
}

/// One reported output configuration, in probe-local vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigInfo {
    /// Channel count.
    pub channels: u16,
    /// Lowest sample rate this config accepts.
    pub min_rate: u32,
    /// Highest sample rate this config accepts.
    pub max_rate: u32,
    /// Buffer size limits as reported.
    pub buffer: BufferLimit,
    /// Sample format.
    pub format: StreamFormat,
}

/// What the operator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wants {
    /// Sample rate to open at.
    pub rate: u32,
    /// DSP block shape, also used as the requested driver buffer.
    pub frames: u32,
}

/// How the requested rate survived contact with the reported range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The device accepts exactly what was asked.
    Exact {
        /// The rate, which is the requested one.
        asked: u32,
    },
    /// The rate was moved to the nearest end of the reported range.
    Clamped {
        /// What was asked.
        asked: u32,
        /// What we will open with.
        got: u32,
        /// The reported range.
        range: (u32, u32),
    },
}

/// How the requested buffer size survived contact with the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDecision {
    /// Inside the reported range: request it as `BufferSize::Fixed`.
    Exact {
        /// The frames, which are the requested ones.
        asked: u32,
    },
    /// Outside the reported range: request the nearest legal bound instead.
    Clamped {
        /// What was asked.
        asked: u32,
        /// What the device will legally accept.
        got: u32,
        /// The reported range.
        range: (u32, u32),
    },
    /// The host reports no range, so we ask for its default and measure what arrives.
    EngineDefault {
        /// The shape we still pump the DSP at.
        asked: u32,
    },
}

/// What we will hand cpal as the buffer size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferRequest {
    /// `BufferSize::Fixed(n)`.
    Fixed(u32),
    /// `BufferSize::Default`.
    Default,
}

/// The whole decision, ready to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevicePlan {
    /// Index of the chosen config in the list it was picked from.
    pub config_index: usize,
    /// Channels we will open with.
    pub channels: u16,
    /// Rate we will open with.
    pub rate: u32,
    /// Requested rate vs what the device allows.
    pub rate_decision: RateDecision,
    /// Frames we will hand cpal (or its default).
    pub buffer: BufferRequest,
    /// How the requested 64 survived.
    pub frame_decision: FrameDecision,
    /// DSP block shape: the number the budget is computed from. Never the driver's
    /// choice, always the requested one.
    pub chunk_frames: u32,
    /// Format we will build the stream for.
    pub format: StreamFormat,
    /// Reported buffer limits of the chosen config.
    pub reported: BufferLimit,
}

impl DevicePlan {
    /// A sentence that says, honestly, what the observed callback sizes will mean.
    /// Static because it is printed on the control thread and stored in the report.
    pub fn honesty_note(&self) -> &'static str {
        match self.frame_decision {
            FrameDecision::EngineDefault { .. } => {
                "the host reports no buffer range: the stream opens with the engine's \
                 default period and the printed callback sizes are the only truth"
            }
            _ if self.reported.is_free_rein() => {
                "the reported range is a software stack's 'free rein' (0..=max): in \
                 shared mode WASAPI keeps its own engine period, so the printed \
                 callback sizes — not this request — are what actually arrived"
            }
            _ => "the requested buffer is inside the reported range; the printed \
                  callback sizes still decide whether the driver honoured it",
        }
    }
}

/// Why no plan could be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The device reported no output configs at all.
    NoConfigs,
    /// Configs exist, none in a format the probe can pump f32 through.
    NoUsableFormat(Vec<StreamFormat>),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigs => write!(f, "the device reported no output configurations"),
            Self::NoUsableFormat(seen) => {
                write!(f, "no f32/i16 output config among the {} reported (", seen.len())?;
                for (i, s) in seen.iter().enumerate() {
                    write!(f, "{}{}", if i > 0 { ", " } else { "" }, s.label())?;
                }
                write!(f, ")")
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// Picks the config to open, the rate to open it at and the buffer to request.
///
/// Ranking (best first): usable format (`f32` before `i16`), a range that contains
/// the requested rate, stereo before mono before widest, then the smallest distance
/// to the requested rate. Ties break by input order, which is cpal's own heuristic
/// order — so the choice is reproducible, and `plan_is_stable_under_ties` pins it.
#[must_use]
pub fn plan_output(configs: &[ConfigInfo], wants: &Wants) -> Result<DevicePlan, PlanError> {
    if configs.is_empty() {
        return Err(PlanError::NoConfigs);
    }
    let mut best: Option<((u8, u8, u8, u32, usize), &ConfigInfo)> = None;
    for (i, cfg) in configs.iter().enumerate() {
        let Some(fmt_rank) = cfg.format.rank() else {
            continue;
        };
        let contains = wants.rate >= cfg.min_rate && wants.rate <= cfg.max_rate;
        let rate_rank = u8::from(!contains);
        let ch_rank = match cfg.channels {
            2 => 0,
            1 => 1,
            _ => 2,
        };
        let distance = if contains {
            0
        } else if wants.rate < cfg.min_rate {
            cfg.min_rate - wants.rate
        } else {
            wants.rate - cfg.max_rate
        };
        let key = (fmt_rank, rate_rank, ch_rank, distance, i);
        let better = match &best {
            None => true,
            Some(prev) => key < prev.0,
        };
        if better {
            best = Some((key, cfg));
        }
    }
    let Some(((_, _, _, _, config_index), cfg)) = best else {
        let seen: Vec<StreamFormat> = configs.iter().map(|c| c.format).collect();
        return Err(PlanError::NoUsableFormat(seen));
    };

    let rate = if wants.rate < cfg.min_rate {
        cfg.min_rate
    } else if wants.rate > cfg.max_rate {
        cfg.max_rate
    } else {
        wants.rate
    };
    let rate_decision = if rate == wants.rate {
        RateDecision::Exact { asked: wants.rate }
    } else {
        RateDecision::Clamped {
            asked: wants.rate,
            got: rate,
            range: (cfg.min_rate, cfg.max_rate),
        }
    };

    let (buffer, frame_decision) = match cfg.buffer {
        BufferLimit::Unknown => (
            BufferRequest::Default,
            FrameDecision::EngineDefault {
                asked: wants.frames,
            },
        ),
        BufferLimit::Range { min, max } => {
            // A reported `min` of 0 means "unspecified", not "pump 0 frames".
            let lo = min.max(1);
            let hi = if max < lo { lo } else { max };
            let got = wants.frames.clamp(lo, hi);
            let decision = if got == wants.frames {
                FrameDecision::Exact { asked: got }
            } else {
                FrameDecision::Clamped {
                    asked: wants.frames,
                    got,
                    range: (lo, hi),
                }
            };
            (BufferRequest::Fixed(got), decision)
        }
    };

    Ok(DevicePlan {
        config_index,
        channels: cfg.channels,
        rate,
        rate_decision,
        buffer,
        frame_decision,
        chunk_frames: wants.frames,
        format: cfg.format,
        reported: cfg.buffer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config a modern WASAPI endpoint reports: f32, stereo, 48k, free rein.
    fn wasapi_f32(min_rate: u32, max_rate: u32, buffer: BufferLimit) -> ConfigInfo {
        ConfigInfo {
            channels: 2,
            min_rate,
            max_rate,
            buffer,
            format: StreamFormat::F32,
        }
    }

    fn wants(rate: u32, frames: u32) -> Wants {
        Wants { rate, frames }
    }

    const FREE: BufferLimit = BufferLimit::Range {
        min: 0,
        max: u32::MAX,
    };

    #[test]
    fn picks_48k_stereo_f32_and_asks_for_exactly_64_frames() {
        let cfgs = [wasapi_f32(44_100, 192_000, FREE)];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.rate, 48_000, "48k is inside 44.1k..=192k");
        assert_eq!(p.rate_decision, RateDecision::Exact { asked: 48_000 });
        assert_eq!(p.buffer, BufferRequest::Fixed(64), "64 is legal");
        assert_eq!(p.frame_decision, FrameDecision::Exact { asked: 64 });
        assert_eq!(p.channels, 2);
        assert_eq!(p.format, StreamFormat::F32);
    }

    #[test]
    fn prefers_f32_over_i16_even_when_i16_came_first() {
        let cfgs = [
            ConfigInfo {
                channels: 2,
                min_rate: 48_000,
                max_rate: 48_000,
                buffer: FREE,
                format: StreamFormat::I16,
            },
            wasapi_f32(48_000, 48_000, FREE),
        ];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.format, StreamFormat::F32, "no edge conversion in RT");
        assert_eq!(p.config_index, 1, "f32 outranks the earlier i16");
    }

    #[test]
    fn prefers_stereo_then_mono_then_widest() {
        let cfgs = [
            ConfigInfo {
                channels: 8,
                ..wasapi_f32(48_000, 48_000, FREE)
            },
            ConfigInfo {
                channels: 1,
                ..wasapi_f32(48_000, 48_000, FREE)
            },
            wasapi_f32(48_000, 48_000, FREE),
        ];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.channels, 2, "a stereo chain is the M0 shape");
        // Only mono exists → mono is taken, not refused.
        let mono = [ConfigInfo {
            channels: 1,
            ..wasapi_f32(48_000, 48_000, FREE)
        }];
        assert_eq!(
            plan_output(&mono, &wants(48_000, 64)).expect("mono plan").channels,
            1,
            "a mono endpoint is still a real device"
        );
    }

    #[test]
    fn clamps_the_rate_to_the_device_and_says_so() {
        // The classic office endpoint: 44.1k only. The probe must not pretend it
        // opened at 48k.
        let cfgs = [wasapi_f32(44_100, 44_100, FREE)];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.rate, 44_100);
        assert_eq!(
            p.rate_decision,
            RateDecision::Clamped {
                asked: 48_000,
                got: 44_100,
                range: (44_100, 44_100)
            },
            "the printout must carry the honest rate"
        );
        assert_eq!(p.chunk_frames, 64, "the DSP shape does not follow the rate");
    }

    #[test]
    fn picks_the_nearest_rate_range_when_none_contains_it() {
        let cfgs = [
            wasapi_f32(96_000, 192_000, FREE),
            wasapi_f32(8_000, 16_000, FREE),
        ];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.rate, 16_000, "16k is 32k away, 96k is 48k away");
        assert_eq!(p.config_index, 1);
    }

    #[test]
    fn clamps_frames_to_the_device_minimum_but_keeps_the_dsp_chunk() {
        let cfgs = [wasapi_f32(
            48_000,
            48_000,
            BufferLimit::Range { min: 128, max: 4096 },
        )];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(
            p.buffer,
            BufferRequest::Fixed(128),
            "64 is illegal, so ask for the nearest legal driver buffer"
        );
        assert_eq!(
            p.frame_decision,
            FrameDecision::Clamped {
                asked: 64,
                got: 128,
                range: (128, 4096)
            }
        );
        assert_eq!(
            p.chunk_frames, 64,
            "the chain still pumps 64-frame blocks: that is the G1 shape"
        );
    }

    #[test]
    fn clamps_frames_to_the_device_maximum() {
        let cfgs = [wasapi_f32(
            48_000,
            48_000,
            BufferLimit::Range { min: 1, max: 48 },
        )];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.buffer, BufferRequest::Fixed(48));
    }

    #[test]
    fn unknown_buffer_range_requests_the_engine_default() {
        let cfgs = [wasapi_f32(48_000, 48_000, BufferLimit::Unknown)];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.buffer, BufferRequest::Default);
        assert_eq!(p.frame_decision, FrameDecision::EngineDefault { asked: 64 });
        assert!(p.honesty_note().contains("no buffer range"));
    }

    #[test]
    fn free_rein_range_is_flagged_in_the_note_not_in_the_request() {
        let cfgs = [wasapi_f32(48_000, 48_000, FREE)];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.buffer, BufferRequest::Fixed(64), "still request it");
        assert!(
            p.honesty_note().contains("shared mode WASAPI"),
            "…but say the engine decides: {}",
            p.honesty_note()
        );
    }

    #[test]
    fn a_reported_min_of_zero_is_not_a_zero_frame_block() {
        let cfgs = [wasapi_f32(
            48_000,
            48_000,
            BufferLimit::Range { min: 0, max: 4096 },
        )];
        let p = plan_output(&cfgs, &wants(48_000, 8)).expect("plan");
        assert_eq!(p.buffer, BufferRequest::Fixed(8), "8 >= max(0,1)");
    }

    #[test]
    fn refuses_when_no_config_is_a_format_we_can_pump() {
        let cfgs = [
            ConfigInfo {
                channels: 2,
                min_rate: 48_000,
                max_rate: 48_000,
                buffer: FREE,
                format: StreamFormat::Unsupported,
            },
            ConfigInfo {
                channels: 2,
                min_rate: 48_000,
                max_rate: 48_000,
                buffer: FREE,
                format: StreamFormat::Unsupported,
            },
        ];
        let e = plan_output(&cfgs, &wants(48_000, 64)).err().expect("refused");
        match e {
            PlanError::NoUsableFormat(seen) => assert_eq!(seen.len(), 2, "both refused"),
            other => panic!("wrong rejection: {other:?}"),
        }
        assert!(e.to_string().contains("unsupported"), "{e}");
    }

    #[test]
    fn refuses_an_empty_config_list() {
        assert_eq!(
            plan_output(&[], &wants(48_000, 64)).err(),
            Some(PlanError::NoConfigs)
        );
    }

    #[test]
    fn plan_is_stable_under_ties() {
        // Three identical configs: the first must win, every run, or the report
        // would depend on iteration accidents.
        let cfgs = [
            wasapi_f32(48_000, 48_000, FREE),
            wasapi_f32(48_000, 48_000, FREE),
            wasapi_f32(48_000, 48_000, FREE),
        ];
        for _ in 0..8 {
            assert_eq!(
                plan_output(&cfgs, &wants(48_000, 64))
                    .expect("plan")
                    .config_index,
                0
            );
        }
    }

    #[test]
    fn i16_is_accepted_when_nothing_better_exists_and_is_labelled() {
        let cfgs = [ConfigInfo {
            channels: 2,
            min_rate: 48_000,
            max_rate: 48_000,
            buffer: FREE,
            format: StreamFormat::I16,
        }];
        let p = plan_output(&cfgs, &wants(48_000, 64)).expect("plan");
        assert_eq!(p.format, StreamFormat::I16, "an i16 endpoint is still hardware");
    }
}
