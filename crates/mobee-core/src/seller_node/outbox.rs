//! The outbox publisher: drain the durable event outbox to the relay, retrying until each event is
//! confirmed or expires.
//!
//! The store is the source of truth: a state change and the event it must publish are written in one
//! transaction ([`super::store`]). This publisher is the async half — it reads `pending` rows, hands
//! each to an [`EventPublisher`], and records the outcome. Because every enqueue is deduped on a
//! stable key and every event is signed at a fixed authored-at second, a re-publish after a crash is
//! idempotent at the relay: the same event id is sent again and the relay collapses it. That is what
//! lets the publisher retry freely without ever double-paying or double-delivering.
//!
//! The concrete relay transport (sign via the signer actor, send over a nostr client) is the
//! deployable [`EventPublisher`] the node wires at cutover; this module owns the durable-drain logic
//! and is exercised here against a fake publisher so the retry/confirm/expire behavior is pinned
//! independent of the network.

use super::store::{OutboxItem, SellerStore, StoreError};

/// Publishes one outbox event and returns the published event id on success.
///
/// An internal seam driven by the node's own single-threaded drain loop, so the `async fn` in trait
/// (no `Send` bound on the returned future) is intentional — the lint is suppressed here.
#[allow(async_fn_in_trait)]
pub trait EventPublisher {
    /// Publish `item` (sign it at its fixed `created_at_unix`, send it to the relay). Returns the
    /// published event id, or an error string to retry later.
    async fn publish(&self, item: &OutboxItem) -> Result<String, String>;
}

/// What one drain pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    pub confirmed: usize,
    pub failed: usize,
    pub expired: usize,
}

/// Run one drain pass: first expire rows past their retry window, then publish every remaining
/// pending row. A confirmed publish is marked `confirmed`; a failed one bumps its attempt count and
/// stays `pending` for the next pass.
pub async fn drain_once<P: EventPublisher>(
    store: &SellerStore,
    publisher: &P,
    now_unix: i64,
) -> Result<DrainReport, StoreError> {
    let mut report = DrainReport::default();
    report.expired = store.expire_outbox(now_unix)?;

    for item in store.pending_outbox(now_unix)? {
        match publisher.publish(&item).await {
            Ok(event_id) => {
                store.mark_confirmed(item.id, &event_id, now_unix)?;
                report.confirmed += 1;
            }
            Err(_) => {
                store.record_attempt(item.id, now_unix)?;
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fresh_store(label: &str) -> (SellerStore, std::path::PathBuf) {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "mobee-seller-outbox-{label}-{}-{id}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        (SellerStore::open(&path).expect("open"), path)
    }

    fn claim_draft() -> crate::gateway::EventDraft {
        crate::gateway::claim_draft(&"e".repeat(64), &"b".repeat(64), &"s".repeat(64), "creqA")
    }

    /// Records every publish call; can be told to fail so the retry path is exercised.
    struct FakePublisher {
        calls: RefCell<Vec<String>>,
        fail: bool,
    }

    impl EventPublisher for FakePublisher {
        async fn publish(&self, item: &OutboxItem) -> Result<String, String> {
            self.calls.borrow_mut().push(item.dedup_key.clone());
            if self.fail {
                Err("relay down".into())
            } else {
                Ok(format!("evt-{}", item.dedup_key))
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_confirms_pending_and_is_a_noop_second_time() {
        let (store, path) = fresh_store("confirm");
        let job = "j".repeat(64);
        store.claim_and_enqueue(&job, &"o".repeat(64), &claim_draft(), 1, 9_999, 1).expect("claim");

        let publisher = FakePublisher { calls: RefCell::new(vec![]), fail: false };
        let report = drain_once(&store, &publisher, 2).await.expect("drain");
        assert_eq!(report.confirmed, 1);
        assert_eq!(publisher.calls.borrow().len(), 1);
        assert_eq!(
            store.outbox_row(&format!("claim:{job}")).expect("row").expect("exists").0,
            "confirmed"
        );

        // Second pass: the row is confirmed, so nothing is published again.
        let report = drain_once(&store, &publisher, 3).await.expect("drain2");
        assert_eq!(report.confirmed, 0);
        assert_eq!(publisher.calls.borrow().len(), 1, "no re-publish of a confirmed row");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_publish_stays_pending_and_bumps_attempts() {
        let (store, path) = fresh_store("retry");
        let job = "j".repeat(64);
        store.claim_and_enqueue(&job, &"o".repeat(64), &claim_draft(), 1, 9_999, 1).expect("claim");

        let publisher = FakePublisher { calls: RefCell::new(vec![]), fail: true };
        let report = drain_once(&store, &publisher, 2).await.expect("drain");
        assert_eq!(report.failed, 1);
        let row = store.outbox_row(&format!("claim:{job}")).expect("row").expect("exists");
        assert_eq!(row.0, "pending", "a failed publish retries");
        assert_eq!(row.1, 1, "attempt was recorded");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_rows_are_not_published() {
        let (store, path) = fresh_store("expire");
        let job = "j".repeat(64);
        store.claim_and_enqueue(&job, &"o".repeat(64), &claim_draft(), 1, 100, 1).expect("claim");

        let publisher = FakePublisher { calls: RefCell::new(vec![]), fail: false };
        // now=200 is past expires_at=100 ⇒ the row expires and is never handed to the publisher.
        let report = drain_once(&store, &publisher, 200).await.expect("drain");
        assert_eq!(report.expired, 1);
        assert_eq!(report.confirmed, 0);
        assert!(publisher.calls.borrow().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
