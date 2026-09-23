//! Local operator-visible state and explicit seller retry tickets. Never review authority.
use super::*;
use std::path::Path;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub subject: Subject,
    pub state: String,
    pub detail: String,
    pub retry: String,
}
pub fn write(root: &Path, subject: &Subject, state: &str, detail: &str) -> Result<(), String> {
    subject.validate()?;
    let dir = root.join("review-status");
    std::fs::create_dir_all(&dir).map_err(|_| "review: status directory")?;
    let status = Status {
        subject: subject.clone(),
        state: state.into(),
        detail: detail.into(),
        retry: if subject.kind == JOB_OFFER_KIND {
            format!("maxplayer review retry {}", subject.event)
        } else {
            "Retry the same collect or accept operation; no new job or delivery is needed.".into()
        },
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
        "pending" | "error" | "policy_refused"
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
    if status.state != "error" {
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
            if e.contains("threshold") || e.contains("conflicting") {
                "policy_refused"
            } else {
                "error"
            },
            e,
        ),
    }
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
}

#[cfg(test)]
mod buyer_hold_tests {
    use super::*;
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
