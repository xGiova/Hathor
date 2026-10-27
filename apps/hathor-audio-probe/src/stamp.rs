//! The one line that says when a number was taken, and how honest its date is.
//!
//! ## Why a civil-date helper instead of a crate
//!
//! "measured `<date>`, dev machine, release build" is part of the G1 deliverable: a
//! number without a date is a number without an expiry. Pulling in `chrono` for one
//! string is not this project's way (and the dependency list is frozen), so the
//! days→civil conversion from Howard Hinnant's `chrono`-compatible algorithm is
//! written here, in integers, and unit-tested against dates whose day numbers are
//! computed independently. It is ~20 lines, and the tests are the argument that it
//! is right.
//!
//! ## What the label claims, and what it does not
//!
//! The date is **UTC**: `SystemTime` gives seconds since the epoch and nothing more,
//! and this module refuses to guess a local zone (the session's zone is not the
//! machine's problem to know here). "dev machine" is a *claim about the machine*,
//! not about the build host's clock quality — the probe does not assert NTP-synced
//! time, only that the reading is stamped with what the OS reported. And the build
//! profile is read from `debug_assertions`, so the line can say `debug build` when
//! someone runs the probe without `--release`: an honest label, not a wish.

use core::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// A civil date (year, month, day) in the proleptic Gregorian calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date {
    /// Year, `1970` and later for anything this probe stamps.
    pub year: i32,
    /// Month `1..=12`.
    pub month: u8,
    /// Day `1..=31`.
    pub day: u8,
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Days since 1970-01-01 → civil date. Negative input is pre-epoch and works.
///
/// Integer-only (no floats, no division by zero, no panics): the era/leap arithmetic
/// is exact for any `i64` day count the OS can produce.
#[must_use]
pub fn civil_from_days(z: i64) -> Date {
    // Shift the epoch to 0000-03-01, the start of Hinnant's "era" calendar: March-based
    // years put the leap day at the end, which makes the month formula below exact.
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // year of era, [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365], March-based
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    Date {
        year: (y + i64::from(if m <= 2 { 1 } else { 0 })) as i32,
        month: m as u8,
        day: d as u8,
    }
}

/// Today's UTC date according to the OS clock, or `None` if it is before the epoch
/// (which a real machine should not be, but a misconfigured BIOS can).
#[must_use]
pub fn utc_today() -> Option<Date> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(civil_from_days((secs / 86_400) as i64))
}

/// The build profile, read from the compiler rather than asserted: `cargo test`
/// without `--release` is a debug build, and the line must say so.
pub fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// The closing line of `bench` and of a `run`: the G1 deliverable's provenance.
///
/// `None` date prints `unknown` rather than a guessed one — a fabricated timestamp
/// on a measured number is the exact failure mode this line exists to prevent.
#[must_use]
pub fn measured_line(date: Option<Date>) -> String {
    let d = date.map_or_else(|| "unknown".to_string(), |d| d.to_string());
    format!("measured {d}, dev machine, {} build", profile())
}

/// The same line with the honesty footnote the task asks for, as one string.
#[must_use]
pub fn measured_block(date: Option<Date>) -> Vec<String> {
    let mut v = vec![measured_line(date)];
    v.push(format!(
        "  (date is UTC from the OS clock, profile from debug_assertions: {})",
        if date.is_some() {
            "no local zone was assumed"
        } else {
            "the OS clock refused to answer"
        }
    ));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The day numbers below are computed independently (year lengths 1970→2026,
    /// with leap years every 4 except 1900-style centuries) — see the comments.
    #[test]
    fn the_epoch_itself() {
        assert_eq!(civil_from_days(0), Date { year: 1970, month: 1, day: 1 });
    }

    #[test]
    fn one_day_before_the_epoch_is_december_1969() {
        assert_eq!(civil_from_days(-1), Date { year: 1969, month: 12, day: 31 });
    }

    #[test]
    fn known_new_year_day_anchors() {
        // 18262 = 2020-01-01; +366 (2020 is a leap year) = 18628; +365×3 = 19723.
        assert_eq!(civil_from_days(18_262), Date { year: 2020, month: 1, day: 1 });
        assert_eq!(civil_from_days(18_628), Date { year: 2021, month: 1, day: 1 });
        assert_eq!(civil_from_days(19_723), Date { year: 2024, month: 1, day: 1 });
        assert_eq!(civil_from_days(20_089), Date { year: 2025, month: 1, day: 1 });
        assert_eq!(civil_from_days(20_454), Date { year: 2026, month: 1, day: 1 });
    }

    #[test]
    fn a_leap_day_exists_only_in_leap_years() {
        // 2024-02-29 = 19723 + 31 + 29 = day 19783.
        assert_eq!(
            civil_from_days(19_783),
            Date { year: 2024, month: 2, day: 29 },
            "2024 is a leap year"
        );
        // 2023-03-01 = 19358 + 59 = 19417 → March 1, so Feb had 28 days.
        assert_eq!(
            civil_from_days(19_417),
            Date { year: 2023, month: 3, day: 1 },
            "2023 is not"
        );
    }

    #[test]
    fn the_session_date_is_what_the_human_will_see() {
        // 2026-10-27: 20454 (Jan 1) + 273 days to Oct 1 + 26 = 20753.
        assert_eq!(
            civil_from_days(20_753),
            Date { year: 2026, month: 10, day: 27 },
            "the date this probe is being written on"
        );
    }

    #[test]
    fn far_future_and_far_past_do_not_panic() {
        // Century rules are where hand-written date code dies; these are the anchors
        // that would catch a missing `doe/36_524` or `doe/146_096` term.
        assert_eq!(
            civil_from_days(73_414),
            Date { year: 2071, month: 4, day: 24 },
            "past 2038"
        );
        assert_eq!(
            civil_from_days(-719_162),
            Date { year: 1601, month: 1, day: 1 },
            "before 1970 by centuries"
        );
    }

    #[test]
    fn the_display_is_iso_padded() {
        assert_eq!(
            Date { year: 2026, month: 1, day: 7 }.to_string(),
            "2026-01-07",
            "a date that sorts lexically is a date that sorts"
        );
    }

    #[test]
    fn utc_today_answers_with_a_sane_date() {
        let d = utc_today().expect("the OS clock has an epoch");
        assert!(d.year >= 2020, "a clock this wrong is not a measured date: {d}");
        assert!((1..=12).contains(&d.month), "month out of range: {d}");
        assert!((1..=31).contains(&d.day), "day out of range: {d}");
    }

    #[test]
    fn the_measured_line_names_the_profile_it_was_built_with() {
        let line = measured_line(Some(Date { year: 2026, month: 10, day: 27 }));
        assert!(
            line.starts_with("measured 2026-10-27, dev machine, "),
            "{line}"
        );
        assert!(
            line.ends_with("build") && (line.contains("release") || line.contains("debug")),
            "the profile must be the truth of this binary, not a wish: {line}"
        );
        assert_eq!(profile(), if cfg!(debug_assertions) { "debug" } else { "release" });
    }

    #[test]
    fn a_missing_date_is_printed_as_unknown_not_guessed() {
        assert!(measured_line(None).contains("measured unknown"), "{}", measured_line(None));
        let block = measured_block(None);
        assert_eq!(block.len(), 2, "the line plus its footnote");
        assert!(block[1].contains("refused to answer"), "{}", block[1]);
    }
}
