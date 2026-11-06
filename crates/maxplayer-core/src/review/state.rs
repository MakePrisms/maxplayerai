//! Local operator-visible state and explicit seller retry tickets. Never review authority.
use super::*;
use std::path::Path;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub subject: Subject,
    pub state: String,
    pub detail: String,
    pub retry: String,
    /// When this record was written. Old files read as 0 (never fresh), so the
    /// read-path throttle in `protect_job_view` cannot trust a stale record.
    #[serde(default)]
    pub updated_unix: u64,
}
pub fn write(root: &Path, subject: &Subject, state: &str, detail: &str) -> Result<(), String> {
    subject.validate()?;
    let dir = root.join("review-status");
    std::fs::create_dir_all(&dir).map_err(|_| "review: status directory")?;
    let status = Status {
        subject: subject.clone(),
        state: state.into(),
        detail: detail.into(),
        retry: if state == "input_refused" {
            "Retrying unchanged input cannot help. Correct the input or update the reviewer, then explicitly retry.".into()
        } else if subject.kind == JOB_OFFER_KIND {
            format!("maxplayer review retry {}", subject.event)
        } else {
            "Retry the same collect or accept operation; no new job or delivery is needed.".into()
        },
        updated_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| "review: random source unavailable")?;
    let tmp = dir.join(format!("{}.{}.tmp", subject.event, hex::encode(nonce)));
    std::fs::write(
        &tmp,
        serde_json::to_vec(&status).map_err(|_| "review: status encoding")?,
    )
    .map_err(|_| "review: status write")?;
    std::fs::rename(tmp, dir.join(format!("{}.json", subject.event)))
        .map_err(|_| "review: status save".to_owned())?;
    if subject.kind == JOB_RESULT_KIND {
        let dir = root.join("review-job-status");
        std::fs::create_dir_all(&dir).map_err(|_| "review: status directory")?;
        let tmp = dir.join(format!("{}.{}.tmp", subject.offer, hex::encode(nonce)));
        std::fs::write(
            &tmp,
            serde_json::to_vec(&status).map_err(|_| "review: status encoding")?,
        )
        .map_err(|_| "review: status write")?;
        std::fs::rename(tmp, dir.join(format!("{}.json", subject.offer)))
            .map_err(|_| "review: status save")?;
    }
    Ok(())
}
/// A failed foreground review must not become repeated provider spending through
/// the buyer's existing automatic settlement sweep. Explicit collect still retries.
pub fn blocked_job(root: &Path, job_id: &str) -> Result<Option<Status>, String> {
    if !hex_id(job_id) {
        return Ok(None);
    }
    let bytes = match std::fs::read(
        root.join("review-job-status")
            .join(format!("{job_id}.json")),
    ) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("review: cannot read automatic-settlement hold".into()),
    };
    let status: Status =
        serde_json::from_slice(&bytes).map_err(|_| "review: invalid automatic-settlement hold")?;
    if status.subject.offer != job_id {
        return Err("review: incorrect automatic-settlement hold".into());
    }
    Ok(matches!(
        status.state.as_str(),
        "pending" | "error" | "policy_refused" | "input_refused"
    )
    .then_some(status))
}
pub fn read(root: &Path, id: &str) -> Result<Status, String> {
    if !hex_id(id) {
        return Err("review: invalid subject id".into());
    }
    let bytes = std::fs::read(root.join("review-status").join(format!("{id}.json")))
        .map_err(|_| "review: no local status for this subject")?;
    serde_json::from_slice(&bytes).map_err(|_| "review: invalid local status".into())
}
pub fn retry(root: &Path, id: &str) -> Result<(), String> {
    let status = read(root, id)?;
    if status.subject.kind != JOB_OFFER_KIND {
        return Err(status.retry);
    }
    if !matches!(status.state.as_str(), "error" | "input_refused") {
        return Err("review: only availability errors can be retried; pending work is already running and policy refusals require a policy decision".into());
    }
    let dir = root.join("review-retry");
    std::fs::create_dir_all(&dir).map_err(|_| "review: retry directory")?;
    std::fs::write(dir.join(id), b"retry").map_err(|_| "review: retry write".into())
}
pub fn take_retry(root: &Path, id: &str) -> bool {
    hex_id(id) && std::fs::remove_file(root.join("review-retry").join(id)).is_ok()
}
pub fn completed(
    root: &Path,
    subject: &Subject,
    result: &Result<Option<String>, String>,
) -> Result<(), String> {
    match result {
        Ok(Some(id)) => write(root, subject, "passed", id),
        Ok(None) => write(
            root,
            subject,
            "disabled",
            "explicit local configuration or counterparty public-key skip",
        ),
        Err(e) => write(
            root,
            subject,
            if permanent_error(e) {
                "input_refused"
            } else if policy_refusal(e) {
                "policy_refused"
            } else {
                "error"
            },
            e,
        ),
    }
}
fn policy_refusal(error: &str) -> bool {
    error.contains("threshold") || error.contains("conflicting")
}
/// #1057: a review failure that cannot change by waiting (`input_refused` or `policy_refused`).
/// Re-requesting it reproduces the same answer, so the seller stops re-driving the offer until an
/// explicit `review retry` (permanent input errors only) or a restart (a changed local threshold).
/// Everything else (`error`: timeouts, reviewer unreachable, relay refusals) is availability.
pub fn terminal(error: &str) -> bool {
    permanent_error(error) || policy_refusal(error)
}
/// Offer ids with an explicit retry ticket waiting to be consumed by the seller's reconsider tick.
pub fn queued_retries(root: &Path) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(root.join("review-retry")) else {
        return Vec::new();
    };
    dir.filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|id| hex_id(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seller_retry_ticket_is_explicit_consumed_once_and_refusal_cannot_retry() {
        let root = std::env::temp_dir().join(format!("review-state-{}", uuid::Uuid::new_v4()));
        let s = Subject {
            offer: "a".repeat(64),
            event: "a".repeat(64),
            kind: JOB_OFFER_KIND,
            commit: None,
        };
        write(&root, &s, "error", "review timed out").unwrap();
        assert!(!take_retry(&root, &s.event));
        retry(&root, &s.event).unwrap();
        assert!(take_retry(&root, &s.event));
        assert!(!take_retry(&root, &s.event));
        write(&root, &s, "policy_refused", "unsafe").unwrap();
        assert!(retry(&root, &s.event).is_err());
        assert!(read(&root, "../../elsewhere").is_err());
    }

    #[test]
    fn terminal_outcomes_are_refusals_and_permanent_input_errors_only() {
        assert!(terminal("review: execution-safety probability exceeds local threshold"));
        assert!(terminal("review: conflicting results"));
        assert!(terminal("review: permanent input error (invalid_subject); retrying unchanged input cannot help"));
        for availability in [
            "review: timeout; next step blocked; retry available",
            "review: timeout; last reviewer error (rate_limited); next step blocked; retry available",
            "review: reviewer error (queue_full); next step blocked; explicit retry available",
            "review: relay refused the review request: rate-limited",
            "review: configure a reviewer key for this relay, or explicitly skip review",
        ] {
            assert!(!terminal(availability), "{availability}");
        }
        let root = tempfile::tempdir().unwrap();
        assert!(queued_retries(root.path()).is_empty());
        let s = Subject { offer: "c".repeat(64), event: "c".repeat(64), kind: JOB_OFFER_KIND, commit: None };
        write(root.path(), &s, "input_refused", "review: permanent input error").unwrap();
        retry(root.path(), &s.event).unwrap();
        assert_eq!(queued_retries(root.path()), vec![s.event.clone()]);
        assert!(take_retry(root.path(), &s.event));
        assert!(queued_retries(root.path()).is_empty());
    }
}

#[cfg(test)]
mod buyer_hold_tests {
    use super::*;
    #[test]
    fn permanent_input_refusal_holds_watcher_and_has_no_blind_retry_advice() {
        let root = tempfile::tempdir().unwrap();
        let subject = Subject {
            offer: "a".repeat(64),
            event: "b".repeat(64),
            kind: JOB_RESULT_KIND,
            commit: None,
        };
        completed(
            root.path(),
            &subject,
            &Err(reviewer_error("unsupported_input")),
        )
        .unwrap();
        let status = blocked_job(root.path(), &subject.offer).unwrap().unwrap();
        assert_eq!(status.state, "input_refused");
        assert!(status.retry.contains("unchanged input cannot help"));
        assert!(!status.retry.contains("no new job"));
        completed(root.path(), &subject, &Ok(Some("c".repeat(64)))).unwrap();
        assert!(blocked_job(root.path(), &subject.offer).unwrap().is_none());
    }

    #[test]
    fn review_failure_holds_automatic_settlement_until_explicit_recovery() {
        let root = std::env::temp_dir().join(format!("review-hold-{}", uuid::Uuid::new_v4()));
        let s = Subject {
            offer: "a".repeat(64),
            event: "b".repeat(64),
            kind: JOB_RESULT_KIND,
            commit: None,
        };
        assert!(blocked_job(&root, &s.offer).unwrap().is_none());
        write(&root, &s, "error", "timeout").unwrap();
        assert!(blocked_job(&root, &s.offer).unwrap().is_some());
        write(&root, &s, "pending", "explicit retry").unwrap();
        assert!(blocked_job(&root, &s.offer).unwrap().is_some());
        write(&root, &s, "passed", "signed result").unwrap();
        assert!(blocked_job(&root, &s.offer).unwrap().is_none());
    }
}
