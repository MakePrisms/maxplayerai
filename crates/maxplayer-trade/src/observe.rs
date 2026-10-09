//! Read-only views of a trade home: no owner lock, no journal writes, no network.
//!
//! Everything here is DERIVED display state. Nothing in this module feeds back into the
//! swap state machine, its deadlines, lock durations or refund decisions.
use crate::coordinator::Swap;
use anyhow::Result;
use std::{collections::HashMap, path::Path};

/// A swap waiting on its counterparty is reported `counterparty_unresponsive` after this long.
pub const UNRESPONSIVE_SECONDS: u64 = 180;
/// Repeat an unresponsive warning in live output at most this often.
pub const REPEAT_SECONDS: u64 = 300;

/// Threshold in seconds. Lab builds may shorten it for loopback tests only.
pub fn unresponsive_after() -> u64 {
    #[cfg(feature = "lab")]
    if let Some(seconds) = std::env::var("TRADE_LAB_UNRESPONSIVE_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return seconds;
    }
    UNRESPONSIVE_SECONDS
}

/// Open an SQLite file read-only, or `None` when it does not exist (nothing is created).
pub fn open(path: &Path) -> Result<Option<rusqlite::Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(Some(db))
}

/// `(key, value, updated_time)` for one journal namespace, read-only.
pub fn journal_rows(home: &Path, ns: &str) -> Result<Vec<(String, Vec<u8>, u64)>> {
    let Some(db) = open(&home.join("trade.sqlite"))? else {
        return Ok(vec![]);
    };
    let mut stmt = db.prepare(
        "SELECT key, value, updated_time FROM kv_store WHERE primary_namespace='trade-v1' \
         AND secondary_namespace=?1 ORDER BY created_time, key",
    )?;
    let rows = stmt.query_map([ns], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (k, v, t) = row?;
        Ok((k, v, u64::try_from(t).unwrap_or(0)))
    })
    .collect()
}

/// True when the journal holds a swap that is not terminal (read-only).
pub fn open_swap(home: &Path) -> Result<bool> {
    Ok(journal_rows(home, "swap")?.iter().any(|(_, v, _)| {
        serde_json::from_slice::<Swap>(v).map_or(true, |s| !crate::coordinator::terminal(&s))
    }))
}

/// What a swap in `state` is waiting for from its counterparty, if anything.
pub fn waiting_for(role: &str, state: &str) -> Option<&'static str> {
    match (role, state) {
        ("taker", "requested") => Some("maker_quote"),
        ("taker", "first_locked") => Some("maker_second_lock"),
        ("maker", "quoted") => Some("taker_first_lock"),
        ("maker", "second_locked") => Some("taker_claim"),
        _ => None,
    }
}

/// Earliest unix time at which our own outgoing lock becomes refundable, if we hold one.
/// Mirrors the refund gates in the coordinator (`time > long + margin` for the taker,
/// `time > short + margin` for the maker); it does not decide anything.
pub fn refund_available_at(s: &Swap) -> Option<u64> {
    let q = s.quote.as_ref()?;
    match (s.role.as_str(), s.state.as_str()) {
        ("taker", "first_locked") => Some(q.long.saturating_add(q.margin).saturating_add(1)),
        ("maker", "second_locked") => Some(q.short.saturating_add(q.margin).saturating_add(1)),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unresponsive {
    pub swap_id: String,
    pub role: String,
    pub state: String,
    pub waiting_for: &'static str,
    pub elapsed_seconds: u64,
    pub refund_available_at: Option<u64>,
}
impl Unresponsive {
    pub fn json(&self, now: u64) -> serde_json::Value {
        serde_json::json!({
            "warning": "counterparty_unresponsive",
            "swap_id": self.swap_id,
            "role": self.role,
            "state": self.state,
            "waiting_for": self.waiting_for,
            "elapsed_seconds": self.elapsed_seconds,
            "refund_available_at": self.refund_available_at,
            "refund_available_in_seconds": self.refund_available_at.map(|t| t.saturating_sub(now)),
        })
    }
}

/// Pure derivation. `entered` is when the swap record last changed, i.e. when it entered its
/// current waiting state; nothing from the counterparty has advanced it since.
pub fn derive(
    swap_id: &str,
    role: &str,
    state: &str,
    entered: u64,
    refund_available_at: Option<u64>,
    now: u64,
    after: u64,
) -> Option<Unresponsive> {
    let waiting_for = waiting_for(role, state)?;
    let elapsed_seconds = now.saturating_sub(entered);
    (elapsed_seconds > after).then(|| Unresponsive {
        swap_id: swap_id.into(),
        role: role.into(),
        state: state.into(),
        waiting_for,
        elapsed_seconds,
        refund_available_at,
    })
}

/// Every swap currently waiting on an unresponsive counterparty (read-only).
pub fn unresponsive(home: &Path, now: u64) -> Result<Vec<Unresponsive>> {
    let after = unresponsive_after();
    let mut out = vec![];
    for (_, v, updated) in journal_rows(home, "swap")? {
        let Ok(s) = serde_json::from_slice::<Swap>(&v) else {
            continue;
        };
        if let Some(u) = derive(
            &s.id,
            &s.role,
            &s.state,
            updated,
            refund_available_at(&s),
            now,
            after,
        ) {
            out.push(u);
        }
    }
    Ok(out)
}

/// Rate limiter for live warnings: first crossing, then every [`REPEAT_SECONDS`].
#[derive(Default)]
pub struct Warned(HashMap<String, u64>);
impl Warned {
    pub fn due(&mut self, current: &[Unresponsive]) -> Vec<Unresponsive> {
        self.0
            .retain(|id, _| current.iter().any(|u| &u.swap_id == id));
        let mut out = vec![];
        for u in current {
            let due = match self.0.get(&u.swap_id) {
                None => true,
                Some(last) => u.elapsed_seconds >= last.saturating_add(REPEAT_SECONDS),
            };
            if due {
                self.0.insert(u.swap_id.clone(), u.elapsed_seconds);
                out.push(u.clone());
            }
        }
        out
    }
}

/// `YYYY-MM-DD HH:MM:SS UTC` without a date dependency.
pub fn utc(ts: u64) -> String {
    let days = (ts / 86400) as i64;
    let secs = ts % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// `1h02m03s` / `4m05s` / `6s`.
pub fn span(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn warning_only_after_threshold_and_only_while_waiting() {
        let after = UNRESPONSIVE_SECONDS;
        assert_eq!(after, 180);
        assert!(derive("s", "taker", "first_locked", 1000, Some(5000), 1180, after).is_none());
        let w = derive("s", "taker", "first_locked", 1000, Some(5000), 1181, after).unwrap();
        assert_eq!(
            (w.elapsed_seconds, w.waiting_for, w.refund_available_at),
            (181, "maker_second_lock", Some(5000))
        );
        assert_eq!(
            derive("s", "maker", "second_locked", 0, None, 999, after)
                .unwrap()
                .waiting_for,
            "taker_claim"
        );
        // Our own pending actions and terminal states never warn.
        for (role, state) in [
            ("taker", "accepted"),
            ("taker", "second_validated"),
            ("taker", "claimed"),
            ("taker", "complete"),
            ("maker", "first_validated"),
            ("maker", "claiming"),
            ("maker", "settling"),
            ("maker", "refunded"),
        ] {
            assert!(derive("s", role, state, 0, None, 100_000, after).is_none());
        }
        assert_eq!(w.json(4000)["refund_available_in_seconds"], 1000);
    }
    #[test]
    fn repeat_rate_limited() {
        let mut w = Warned::default();
        let u = |e| Unresponsive {
            swap_id: "a".into(),
            role: "taker".into(),
            state: "first_locked".into(),
            waiting_for: "maker_second_lock",
            elapsed_seconds: e,
            refund_available_at: None,
        };
        assert_eq!(w.due(&[u(181)]).len(), 1);
        assert!(w.due(&[u(400)]).is_empty());
        assert_eq!(w.due(&[u(481)]).len(), 1);
        assert!(w.due(&[]).is_empty());
        assert_eq!(w.due(&[u(500)]).len(), 1, "a fresh wait warns again");
    }
    #[test]
    fn utc_and_span_format() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_791_504_000), "2026-10-09 00:00:00 UTC");
        assert_eq!(utc(951_825_600), "2000-02-29 12:00:00 UTC");
        assert_eq!(span(3723), "1h02m03s");
        assert_eq!(span(245), "4m05s");
        assert_eq!(span(6), "6s");
    }
}
