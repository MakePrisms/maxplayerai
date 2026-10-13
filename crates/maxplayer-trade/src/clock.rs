//! The one timestamp formatter shared by serve lifecycle lines and swap progress lines.
//!
//! DISPLAY ONLY: nothing here feeds a deadline, budget, retry or money decision; those keep using
//! `coordinator::now()` and the mint clock in `backoff`. Unit tests and lab builds may freeze
//! [`now`] so they can assert exact rendered deadlines and remaining times.

/// This host's wall clock in unix seconds, as shown in log lines.
pub fn now() -> u64 {
    #[cfg(any(test, feature = "lab"))]
    if let Some(t) = *FROZEN.lock().unwrap_or_else(|e| e.into_inner()) {
        return t;
    }
    crate::coordinator::now()
}

/// `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn iso(unix: u64) -> String {
    nostr_sdk::Timestamp::from(unix).to_human_datetime()
}

/// `1h02m03s` / `4m05s` / `6s`.
pub fn span(seconds: u64) -> String {
    crate::observe::span(seconds)
}

/// Time from `now` until `at`: `4m05s`, or `passed 12s ago` once it is behind.
pub fn remaining(at: u64, now: u64) -> String {
    if at > now {
        span(at - now)
    } else {
        format!("passed {} ago", span(now - at))
    }
}

#[cfg(any(test, feature = "lab"))]
static FROZEN: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);
/// Test/lab only: freeze the DISPLAY clock at `t` (unix seconds), or release it with `None`.
#[cfg(any(test, feature = "lab"))]
pub fn lab_freeze(t: Option<u64>) {
    *FROZEN.lock().unwrap_or_else(|e| e.into_inner()) = t;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn iso_and_remaining() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_791_837_598), "2026-10-12T20:39:58Z");
        assert_eq!(remaining(1_000 + 245, 1_000), "4m05s");
        assert_eq!(remaining(1_000, 1_012), "passed 12s ago");
        assert_eq!(remaining(1_000, 1_000), "passed 0s ago");
    }
}
