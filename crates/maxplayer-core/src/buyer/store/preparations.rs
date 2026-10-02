//! Durable deduplication, including across a daemon restart. NULL response means
//! owned work, never permission for a second handler to publish another offer.
//!
//! What a finished row means for the NEXT identical request:
//! - failed with no `offer_id`: nothing can have been published, so it re-runs;
//! - failed with an `offer_id` (the offer was about to be queued/published, or a
//!   pre-column build left it `unknown`): sticky for EVERY key kind and forever,
//!   because re-running could hire twice;
//! - succeeded: an automatic (argument-derived) key answers identical retries for
//!   [`IMPLICIT_DEDUP_SECS`] after completion; after that the same arguments are a
//!   new hire, exactly as before #1095. An explicit `request_id` never expires.
use super::*;
use crate::buyer::protocol::{CODE_INTERNAL, Response};
use serde_json::Value;

/// How long a finished automatic-key preparation answers identical retries.
pub(crate) const IMPLICIT_DEDUP_SECS: i64 = 600;
/// `offer_id` of a row a pre-column build left unfinished or failed.
pub(crate) const UNKNOWN_OFFER: &str = "unknown";

/// Refusal for a `request_id` reused with different arguments or changed inputs.
/// THIS call posted nothing; what the key already did is stated, never assumed,
/// because a new key is a second hire.
fn mismatch(response: Option<&str>, failed: bool, offer_id: Option<&str>) -> String {
    let earlier = match (response, failed, offer_id) {
        (None, _, _) => "Its earlier post_job is still preparing; poll that preparation_id with \
                         get_job"
            .to_owned(),
        (Some(_), _, Some(UNKNOWN_OFFER)) => "Its earlier post_job ran on an earlier build that \
             did not record its offer, so an offer may be live; inspect buyer jobs"
            .to_owned(),
        (Some(_), true, Some(offer)) => format!(
            "Its earlier post_job failed after queueing offer {offer}, which may be live; \
             inspect it with get_job job_id={offer}"
        ),
        (Some(_), true, None) => {
            "Its earlier post_job failed before any offer was queued; nothing was published".into()
        }
        (Some(_), false, Some(job)) => format!("Its earlier post_job posted job {job}"),
        (Some(body), false, None) => serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|r| r["result"]["job_id"].as_str().map(str::to_owned))
            .map_or_else(
                || "Its earlier post_job posted a job".to_owned(),
                |job| format!("Its earlier post_job posted job {job}"),
            ),
    };
    format!(
        "request_id already belongs to different post_job arguments (or input files that have \
         changed since); this call posted nothing. {earlier}. Repeat the original arguments to \
         get its answer; a new request_id is a new hire"
    )
}

impl BuyerStore {
    /// Returns `true` when the caller now owns (and must run) the preparation.
    pub(crate) fn claim_preparation(
        &self,
        handle: &str,
        fingerprint: &str,
        explicit: bool,
        now: i64,
    ) -> Result<bool, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        type Row = (String, Option<String>, bool, Option<String>, Option<i64>);
        let existing: Option<Row> = tx
            .query_row(
                "SELECT fingerprint, response, failed, offer_id, completed_at
                   FROM post_preparations WHERE handle=?1",
                [handle],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        if let Some((existing, response, failed, offer_id, completed_at)) = &existing {
            if existing != fingerprint {
                return Err(StoreError(mismatch(
                    response.as_deref(),
                    *failed,
                    offer_id.as_deref(),
                )));
            }
            let running = response.is_none();
            if running {
                return Ok(false);
            }
            let rerun = if *failed {
                offer_id.is_none()
            } else {
                !explicit
                    && completed_at.is_none_or(|t| now.saturating_sub(t) >= IMPLICIT_DEDUP_SECS)
            };
            if !rerun {
                return Ok(false);
            }
        }
        let running: i64 = tx.query_row(
            "SELECT count(*) FROM post_preparations WHERE response IS NULL",
            [],
            |r| r.get(0),
        )?;
        if running >= 4 {
            return Err(StoreError(
                "four preparations already running; poll their handles before starting another"
                    .into(),
            ));
        }
        tx.execute(
            "INSERT INTO post_preparations(handle, fingerprint) VALUES (?1, ?2)
             ON CONFLICT(handle) DO UPDATE SET
                 response = NULL, failed = 0, offer_id = NULL, completed_at = NULL",
            params![handle, fingerprint],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Record, BEFORE the offer is queued or published, that it may now exist. Fails
    /// closed: if this cannot be written the caller must not publish.
    pub(crate) fn record_preparation_offer(
        &self,
        handle: &str,
        offer_id: &str,
    ) -> Result<(), StoreError> {
        let changed = self.lock()?.execute(
            "UPDATE post_preparations SET offer_id=?2 WHERE handle=?1 AND response IS NULL",
            params![handle, offer_id],
        )?;
        if changed != 1 {
            return Err(StoreError("preparation is no longer owned".into()));
        }
        Ok(())
    }

    pub(crate) fn finish_preparation(
        &self,
        handle: &str,
        response: &Response,
        now: i64,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(response).map_err(|e| StoreError(e.to_string()))?;
        self.lock()?.execute(
            "UPDATE post_preparations SET response=?2, failed=?3, completed_at=?4
              WHERE handle=?1 AND response IS NULL",
            params![handle, json, response.error.is_some(), now],
        )?;
        Ok(())
    }

    pub(crate) fn preparation(&self, handle: &str) -> Result<Option<Option<Response>>, StoreError> {
        Ok(self
            .preparation_state(handle)?
            .map(|(response, _)| response))
    }

    /// The stored response (None while running) and the recorded offer id, if any.
    pub(crate) fn preparation_state(
        &self,
        handle: &str,
    ) -> Result<Option<(Option<Response>, Option<String>)>, StoreError> {
        let row: Option<(Option<String>, Option<String>)> = self
            .lock()?
            .query_row(
                "SELECT response, offer_id FROM post_preparations WHERE handle=?1",
                [handle],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(response, offer)| {
            let response = response
                .map(|s| serde_json::from_str(&s).map_err(|e| StoreError(e.to_string())))
                .transpose()?;
            Ok((response, offer))
        })
        .transpose()
    }

    /// A restart ends every unfinished preparation. Without a recorded offer nothing
    /// was queued or published, so an identical retry simply starts again; with one,
    /// the offer may be live and its auto-award was never armed.
    pub(crate) fn interrupt_preparations(&self, now: i64) -> Result<(), StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(String, Option<String>)> = {
            let mut statement = tx
                .prepare("SELECT handle, offer_id FROM post_preparations WHERE response IS NULL")?;
            let rows = statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            rows
        };
        for (handle, offer_id) in rows {
            let message = match offer_id.as_deref() {
                None => "preparation interrupted by daemon restart before any offer was queued; \
                         nothing was published. Repeat the identical post_job to start again."
                    .to_owned(),
                Some(UNKNOWN_OFFER) => "preparation interrupted by daemon restart; it was \
                     started by an earlier build that did not record its offer, so an offer may \
                     be live and auto-award was NOT armed. Inspect buyer jobs before posting \
                     again; repeating this post_job returns this message rather than posting."
                    .to_owned(),
                Some(id) => format!(
                    "preparation interrupted by daemon restart after offer {id} was queued for \
                     publication: it may be live, and auto-award was NOT armed for it. Inspect it \
                     with get_job job_id={id} and award with award_claim. Repeating this post_job \
                     returns this message rather than posting again."
                ),
            };
            let json = serde_json::to_string(&Response::err(Value::Null, CODE_INTERNAL, message))
                .map_err(|e| StoreError(e.to_string()))?;
            tx.execute(
                "UPDATE post_preparations SET response=?2, failed=1, completed_at=?3
                  WHERE handle=?1 AND response IS NULL",
                params![handle, json, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(job: &str) -> Response {
        Response::ok(Value::Null, serde_json::json!({ "job_id": job }))
    }
    fn failed() -> Response {
        Response::err(Value::Null, CODE_INTERNAL, "base input upload unavailable")
    }

    #[test]
    fn preparation_is_claimed_once_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buyer.sqlite");
        let store = BuyerStore::open(&path).unwrap();
        assert!(
            store
                .claim_preparation("preparation:a", "body", true, 0)
                .unwrap()
        );
        assert!(
            !store
                .claim_preparation("preparation:a", "body", true, 0)
                .unwrap()
        );
        let running = store
            .claim_preparation("preparation:a", "changed", true, 0)
            .unwrap_err()
            .0;
        assert!(running.contains("still preparing"), "{running}");
        store
            .finish_preparation("preparation:a", &ok("one"), 0)
            .unwrap();
        assert!(
            store
                .claim_preparation("preparation:b", "body", true, 0)
                .unwrap()
        );
        store
            .record_preparation_offer("preparation:b", "offer-b")
            .unwrap();
        assert!(
            store
                .claim_preparation("preparation:c", "body", true, 0)
                .unwrap()
        );
        drop(store);
        let store = BuyerStore::open(&path).unwrap();
        store.interrupt_preparations(5).unwrap();
        let read = |h| store.preparation(h).unwrap().unwrap().unwrap();
        assert_eq!(read("preparation:a").result.unwrap()["job_id"], "one");
        let b = read("preparation:b").error.unwrap().message;
        assert!(b.contains("offer-b") && b.contains("NOT armed"), "{b}");
        assert!(
            read("preparation:c")
                .error
                .unwrap()
                .message
                .contains("nothing was published")
        );
        // A possibly-published offer is never re-run; a never-queued one is.
        assert!(
            !store
                .claim_preparation("preparation:b", "body", true, 9)
                .unwrap()
        );
        assert!(
            store
                .claim_preparation("preparation:c", "body", true, 9)
                .unwrap()
        );
    }

    #[test]
    fn failure_before_any_offer_reruns_but_after_an_offer_stays_sticky() {
        let dir = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(dir.path().join("db")).unwrap();
        for explicit in [false, true] {
            let (early, late) = (format!("early-{explicit}"), format!("late-{explicit}"));
            assert!(store.claim_preparation(&early, "f", explicit, 0).unwrap());
            store.finish_preparation(&early, &failed(), 1).unwrap();
            assert!(
                store.claim_preparation(&early, "f", explicit, 2).unwrap(),
                "transient pre-publication failure re-runs"
            );

            assert!(store.claim_preparation(&late, "f", explicit, 0).unwrap());
            store.record_preparation_offer(&late, "offer").unwrap();
            store.finish_preparation(&late, &failed(), 1).unwrap();
            assert!(
                !store.claim_preparation(&late, "f", explicit, 2).unwrap(),
                "possibly published: never re-run in the window"
            );
            assert!(
                !store
                    .claim_preparation(&late, "f", explicit, 1 + IMPLICIT_DEDUP_SECS * 10)
                    .unwrap(),
                "possibly published: never re-run after the window either (#1096 advisor F1)"
            );
        }
    }

    /// Regression (#1096 advisor round 2, nit 1): a reused request_id was told
    /// "nothing was posted" even when that key had already posted a job.
    #[test]
    fn reused_request_id_states_what_the_key_already_did() {
        let dir = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(dir.path().join("db")).unwrap();
        let refusal = |h: &str| store.claim_preparation(h, "other", true, 3).unwrap_err().0;

        assert!(store.claim_preparation("posted", "f", true, 0).unwrap());
        store.record_preparation_offer("posted", "job-1").unwrap();
        store.finish_preparation("posted", &ok("job-1"), 1).unwrap();
        let posted = refusal("posted");
        assert!(posted.contains("posted job job-1"), "{posted}");
        assert!(!posted.contains("nothing was published"), "{posted}");

        assert!(store.claim_preparation("queued", "f", true, 0).unwrap());
        store.record_preparation_offer("queued", "offer-2").unwrap();
        store.finish_preparation("queued", &failed(), 1).unwrap();
        let queued = refusal("queued");
        assert!(
            queued.contains("offer-2") && queued.contains("may be live"),
            "{queued}"
        );

        assert!(store.claim_preparation("early", "f", true, 0).unwrap());
        store.finish_preparation("early", &failed(), 1).unwrap();
        assert!(refusal("early").contains("nothing was published"));

        for message in [posted, queued] {
            assert!(message.contains("this call posted nothing"), "{message}");
            assert!(message.contains("new hire"), "{message}");
        }
    }

    #[test]
    fn automatic_keys_expire_after_the_window_but_explicit_keys_never_do() {
        let dir = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(dir.path().join("db")).unwrap();
        for (handle, explicit) in [("auto", false), ("explicit", true)] {
            assert!(store.claim_preparation(handle, "f", explicit, 0).unwrap());
            store.finish_preparation(handle, &ok("old"), 100).unwrap();
            assert!(
                !store
                    .claim_preparation(handle, "f", explicit, 100 + IMPLICIT_DEDUP_SECS - 1)
                    .unwrap()
            );
            assert_eq!(
                store
                    .claim_preparation(handle, "f", explicit, 100 + IMPLICIT_DEDUP_SECS)
                    .unwrap(),
                !explicit,
                "{handle}"
            );
        }
        // A re-claimed row is clean: running, no old response or offer id.
        assert!(store.preparation("auto").unwrap().unwrap().is_none());
        store.record_preparation_offer("auto", "new-offer").unwrap();
    }

    #[test]
    fn offer_record_fails_closed_once_not_owned() {
        let dir = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(dir.path().join("db")).unwrap();
        assert!(store.record_preparation_offer("missing", "x").is_err());
        assert!(store.claim_preparation("h", "f", true, 0).unwrap());
        store.finish_preparation("h", &ok("x"), 0).unwrap();
        assert!(store.record_preparation_offer("h", "x").is_err());
    }

    #[test]
    fn pre_column_rows_migrate_failed_and_unfinished_as_possibly_published() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE post_preparations (handle TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, response TEXT);",
            )
            .unwrap();
            for (handle, response) in [
                ("bad", Some(failed())),
                ("good", Some(ok("old"))),
                ("inflight", None),
            ] {
                conn.execute(
                    "INSERT INTO post_preparations VALUES (?1, 'f', ?2)",
                    params![handle, response.map(|r| serde_json::to_string(&r).unwrap())],
                )
                .unwrap();
            }
        }
        let store = BuyerStore::open(&path).unwrap();
        store.interrupt_preparations(0).unwrap();
        for explicit in [true, false] {
            assert!(
                !store.claim_preparation("bad", "f", explicit, 0).unwrap(),
                "unknown publication stays sticky (explicit={explicit})"
            );
            assert!(
                !store
                    .claim_preparation("inflight", "f", explicit, 0)
                    .unwrap(),
                "a pre-column in-flight row may have published (explicit={explicit})"
            );
        }
        let inflight = store.preparation("inflight").unwrap().unwrap().unwrap();
        assert!(inflight.error.unwrap().message.contains("earlier build"));
        assert!(!store.claim_preparation("good", "f", true, 0).unwrap());
        assert!(
            store.claim_preparation("good", "f", false, 0).unwrap(),
            "a successful row with no completion time: automatic key expired"
        );
    }

    #[test]
    fn preparation_concurrent_claims_have_one_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(dir.path().join("db")).unwrap();
        let owners = std::thread::scope(|scope| {
            (0..8)
                .map(|_| {
                    let store = &store;
                    scope.spawn(move || store.claim_preparation("h", "same", false, 0).unwrap())
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(owners, 1);
    }
}
