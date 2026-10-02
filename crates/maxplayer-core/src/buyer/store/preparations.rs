//! Durable deduplication, including across a daemon restart. NULL response means
//! owned work, never permission for a second handler to publish another offer.
use super::*;
use crate::buyer::protocol::{CODE_INTERNAL, Response};
use serde_json::Value;

impl BuyerStore {
    pub(crate) fn claim_preparation(
        &self,
        handle: &str,
        fingerprint: &str,
    ) -> Result<bool, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT fingerprint FROM post_preparations WHERE handle=?1",
                [handle],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != fingerprint {
                return Err(StoreError(
                    "request_id already belongs to different post_job arguments".into(),
                ));
            }
            return Ok(false);
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
            "INSERT INTO post_preparations(handle,fingerprint) VALUES (?1,?2)",
            params![handle, fingerprint],
        )?;
        tx.commit()?;
        Ok(true)
    }
    pub(crate) fn finish_preparation(
        &self,
        handle: &str,
        response: &Response,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(response).map_err(|e| StoreError(e.to_string()))?;
        self.lock()?.execute(
            "UPDATE post_preparations SET response=?2 WHERE handle=?1 AND response IS NULL",
            params![handle, json],
        )?;
        Ok(())
    }
    pub(crate) fn preparation(&self, handle: &str) -> Result<Option<Option<Response>>, StoreError> {
        let row: Option<Option<String>> = self
            .lock()?
            .query_row(
                "SELECT response FROM post_preparations WHERE handle=?1",
                [handle],
                |r| r.get(0),
            )
            .optional()?;
        row.map(|s| {
            s.map(|s| serde_json::from_str(&s).map_err(|e| StoreError(e.to_string())))
                .transpose()
        })
        .transpose()
    }
    pub(crate) fn interrupt_preparations(&self) -> Result<(), StoreError> {
        // A crash may have occurred AFTER offer publication but BEFORE recording its
        // result. Never auto-repost or invite a blind retry in this uncertain state.
        let response = Response::err(
            Value::Null,
            CODE_INTERNAL,
            "preparation interrupted by daemon restart; publication may have occurred. Inspect buyer jobs/private content before deliberately starting a new request_id; this handle will not repost.",
        );
        let json = serde_json::to_string(&response).map_err(|e| StoreError(e.to_string()))?;
        self.lock()?.execute(
            "UPDATE post_preparations SET response=?1 WHERE response IS NULL",
            [json],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparation_is_claimed_once_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buyer.sqlite");
        let store = BuyerStore::open(&path).unwrap();
        assert!(store.claim_preparation("preparation:a", "body").unwrap());
        assert!(!store.claim_preparation("preparation:a", "body").unwrap());
        assert!(store.claim_preparation("preparation:a", "changed").is_err());
        store
            .finish_preparation(
                "preparation:a",
                &Response::ok(Value::Null, serde_json::json!({"job_id":"one"})),
            )
            .unwrap();
        assert!(store.claim_preparation("preparation:b", "body").unwrap());
        drop(store);
        let store = BuyerStore::open(&path).unwrap();
        store.interrupt_preparations().unwrap();
        assert_eq!(
            store
                .preparation("preparation:a")
                .unwrap()
                .unwrap()
                .unwrap()
                .result
                .unwrap()["job_id"],
            "one"
        );
        assert!(
            store
                .preparation("preparation:b")
                .unwrap()
                .unwrap()
                .unwrap()
                .error
                .is_some()
        );
        assert!(!store.claim_preparation("preparation:b", "body").unwrap());
    }
    #[test]
    fn preparation_concurrent_claims_have_one_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store = BuyerStore::open(dir.path().join("db")).unwrap();
        let owners = std::thread::scope(|scope| {
            (0..8)
                .map(|_| {
                    let store = &store;
                    scope.spawn(move || store.claim_preparation("h", "same").unwrap())
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(owners, 1);
    }
}
