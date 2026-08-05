//! Operator console output — the stderr an operator actually watches.
//!
//! Every operator-facing line carries a `HH:MM:SSZ` UTC stamp, because an unstamped stream cannot
//! answer the two questions an operator asks of it: *did anything happen since I last looked*, and
//! *does this line line up with that relay event*. Before this the seller printed bare lines and
//! neither question was answerable from the log alone (#489).
//!
//! Time-of-day only, deliberately: it is exact epoch arithmetic with no calendar involved, so there
//! is no date-conversion bug to have. A run spanning midnight is still ordered by arrival.
//!
//! This is the console channel. It is NOT [`crate::log`] (the durable event envelope log), NOT
//! [`crate::telemetry`] (the episode/brain stream), and NOT [`crate::announce`] (lifecycle signals).
//! Nothing here is durable and nothing here is money-path; it is what a human reads.

use std::time::{SystemTime, UNIX_EPOCH};

/// Env var that opts into the verbose console lines. Any non-empty value other than `0` enables.
pub const VERBOSE_ENV: &str = "MAXPLAYER_VERBOSE";

/// `HH:MM:SSZ` in UTC. Falls back to `--:--:--Z` if the clock is before the Unix epoch, so a
/// broken clock degrades one field instead of panicking a daemon.
pub fn stamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => format_seconds_of_day(since.as_secs() % 86_400),
        Err(_) => "--:--:--Z".to_owned(),
    }
}

/// Render a second-of-day as `HH:MM:SSZ`.
///
/// Split from [`stamp`] so the formatting is testable at chosen times. Asserting the width of a
/// live clock reading cannot prove zero-padding: at 15:42:33 an unpadded formatter produces nine
/// characters too, so that check passes for most of the day no matter what the code does.
fn format_seconds_of_day(seconds_of_day: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// Whether verbose console lines are enabled.
///
/// Read fresh each call rather than cached: a daemon started without it should be able to have the
/// variable set and be restarted, and a cached read makes the knob look broken in tests.
pub fn verbose_enabled() -> bool {
    verbose_from(std::env::var(VERBOSE_ENV).ok().as_deref())
}

/// The knob's decision, split from the env read so it is testable without mutating process-global
/// state. Off unless explicitly set: a verbose default would defeat the point of the noise pass.
fn verbose_from(value: Option<&str>) -> bool {
    match value {
        Some(value) => {
            let value = value.trim();
            !value.is_empty() && value != "0"
        }
        None => false,
    }
}

/// Compose one console line: the stamp, a space, then the message.
///
/// The macros below print exactly this. Keeping composition in a function rather than inside the
/// macro is what makes the rendered line assertable in a test — a macro that formats straight to
/// stderr can only be eyeballed.
pub fn line(message: std::fmt::Arguments<'_>) -> String {
    format!("{} {message}", stamp())
}

/// One operator-facing line, timestamped, to stderr. Same argument shape as `eprintln!`.
#[macro_export]
macro_rules! opline {
    ($($arg:tt)*) => {
        eprintln!("{}", $crate::oplog::line(format_args!($($arg)*)))
    };
}

/// An operator-facing line that only prints under [`VERBOSE_ENV`] — for output that does not inform
/// an operator decision. If a line answers "is it working" or "what went wrong", it is NOT this.
#[macro_export]
macro_rules! opline_verbose {
    ($($arg:tt)*) => {
        if $crate::oplog::verbose_enabled() {
            eprintln!("{}", $crate::oplog::line(format_args!($($arg)*)));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_is_a_utc_time_of_day() {
        let stamp = stamp();
        assert_eq!(stamp.len(), 9, "expected HH:MM:SSZ, got {stamp}");
        assert!(stamp.ends_with('Z'), "not UTC-marked: {stamp}");

        let fields: Vec<&str> = stamp.trim_end_matches('Z').split(':').collect();
        assert_eq!(fields.len(), 3, "expected three fields: {stamp}");
        let hours: u32 = fields[0].parse().expect("hours");
        let minutes: u32 = fields[1].parse().expect("minutes");
        let seconds: u32 = fields[2].parse().expect("seconds");
        assert!(hours < 24, "hours out of range: {stamp}");
        assert!(minutes < 60, "minutes out of range: {stamp}");
        assert!(seconds < 60, "seconds out of range: {stamp}");
    }

    /// Zero-padding keeps the column aligned and greppable. Tested at CHOSEN times, including one
    /// where every field is single-digit — the only input at which a missing `{:02}` is visible.
    #[test]
    fn seconds_of_day_render_zero_padded() {
        assert_eq!(format_seconds_of_day(0), "00:00:00Z");
        assert_eq!(format_seconds_of_day(9 * 3600 + 5 * 60 + 3), "09:05:03Z");
        assert_eq!(format_seconds_of_day(23 * 3600 + 59 * 60 + 59), "23:59:59Z");
        assert_eq!(format_seconds_of_day(12 * 3600 + 34 * 60 + 56), "12:34:56Z");
    }

    /// Every second of a day renders at the fixed width the log column depends on.
    #[test]
    fn every_second_of_day_renders_at_fixed_width() {
        for seconds in (0..86_400).step_by(37) {
            let rendered = format_seconds_of_day(seconds);
            assert_eq!(rendered.len(), 9, "width drifted at {seconds}: {rendered}");
        }
    }

    /// The whole point of #489: an operator-facing line arrives STAMPED, with the message intact
    /// after it. Asserting the rendered line is what proves the prefix is actually applied — the
    /// macro compiling proves nothing about what it prints.
    #[test]
    fn a_composed_line_is_stamped_and_keeps_its_message() {
        let rendered = line(format_args!("seller node live: pubkey={} jobs={}", "abc", 3));

        let (prefix, message) = rendered
            .split_once(' ')
            .unwrap_or_else(|| panic!("no stamp separator in {rendered:?}"));
        assert_eq!(message, "seller node live: pubkey=abc jobs=3");
        assert_eq!(prefix.len(), 9, "stamp width wrong in {rendered:?}");
        assert!(prefix.ends_with('Z'), "stamp not UTC-marked in {rendered:?}");
    }

    /// The knob's parsing, without mutating process-global env (which races parallel tests).
    #[test]
    fn verbose_is_off_unless_explicitly_set() {
        assert!(!verbose_from(None), "absent must mean off");
        assert!(!verbose_from(Some("")), "empty must mean off");
        assert!(!verbose_from(Some("   ")), "whitespace must mean off");
        assert!(!verbose_from(Some("0")), "0 must mean off");
        assert!(verbose_from(Some("1")));
        assert!(verbose_from(Some("true")));
    }
}
