//! The binary's error type: every way this probe can refuse, named.
//!
//! No `anyhow`, no `Box<dyn Error>` soup: five variants, each of which corresponds
//! to a different thing the human at the console has to *do*. `Gate` is the one that
//! matters for M0 — it is not a bug in the probe, it is the probe reporting that the
//! contract was broken, and it makes the process exit non-zero so a script cannot
//! mistake a failed gate for a successful run.

use core::fmt;

use crate::cli::CliError;
use crate::device_plan::PlanError;

/// Why the probe stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// The command line was rejected.
    Usage(String),
    /// No host/device, or no usable configuration on the device we were asked for.
    Device(String),
    /// The stream could not be built or played (driver said no).
    Stream(String),
    /// A node refused the block: the contract, not the driver.
    Node(&'static str),
    /// A measured gate failed: the run happened, the numbers did not pass.
    Gate(String),
}

impl ProbeError {
    /// The short tag printed before the message, so `error: gate: …` reads as a kind.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::Device(_) => "device",
            Self::Stream(_) => "stream",
            Self::Node(_) => "node",
            Self::Gate(_) => "gate",
        }
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(s) | Self::Device(s) | Self::Stream(s) | Self::Gate(s) => {
                write!(f, "{s}")
            }
            Self::Node(s) => write!(f, "a node refused the block: {s}"),
        }
    }
}

impl std::error::Error for ProbeError {}

impl From<CliError> for ProbeError {
    fn from(e: CliError) -> Self {
        Self::Usage(e.to_string())
    }
}

impl From<PlanError> for ProbeError {
    fn from(e: PlanError) -> Self {
        Self::Device(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_tag_and_a_message() {
        let cases = [
            (ProbeError::Usage("a".into()), "usage"),
            (ProbeError::Device("b".into()), "device"),
            (ProbeError::Stream("c".into()), "stream"),
            (ProbeError::Node("block exceeds the chain's declared shape"), "node"),
            (ProbeError::Gate("allocations != 0".into()), "gate"),
        ];
        for (e, tag) in cases {
            assert_eq!(e.tag(), tag, "{e}");
            assert!(!e.to_string().is_empty(), "an error that prints nothing helps nobody");
        }
    }

    #[test]
    fn a_node_error_keeps_the_static_message() {
        let e = ProbeError::Node("nope");
        assert_eq!(e.to_string(), "a node refused the block: nope");
    }

    #[test]
    fn conversions_carry_the_original_text() {
        assert_eq!(
            ProbeError::from(CliError("--frames out of range".into())).to_string(),
            "--frames out of range"
        );
        assert_eq!(
            ProbeError::from(PlanError::NoConfigs).tag(),
            "device",
            "an empty config list is a device problem, not a usage one"
        );
    }
}
