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
//! A drain PASS is bounded ([`DrainLimits`]): a relay that stops acknowledging publications used to
//! cost one SDK OK timeout per pending row, serially, so a backlog of 120 rows held the caller for 20
//! minutes. A pass now stops at its deadline, and the rows it did not reach go first next time.
//!
//! The concrete relay transport (sign via the signer actor, send over a nostr client) is the
//! deployable [`EventPublisher`] the node wires at cutover; this module owns the durable-drain logic
//! and is exercised here against a fake publisher so the retry/confirm/expire behavior is pinned
//! independent of the network.

use std::time::Duration;

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
    /// Rows the pass never reached because its deadline arrived first. They are untouched — still
    /// `pending`, attempt count unchanged — and are tried before the rows this pass attempted.
    pub deferred: usize,
}

/// Bounds for ONE drain pass, so a slow relay costs the caller a bounded slice of time rather than
/// (pending rows × the SDK's per-publish OK timeout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainLimits {
    /// No publish starts at or after this instant, and a publish still waiting on the relay when it
    /// arrives is abandoned. An abandoned publish counts as a failed attempt and its row stays
    /// `pending`: the event may already have landed, so the retry re-sends the SAME signed event
    /// (fixed `created_at`, therefore the same id) and the relay collapses the duplicate. `None`
    /// means no deadline.
    pub deadline: Option<tokio::time::Instant>,
    /// At most this many rows are tried in one pass.
    pub max_rows: usize,
    /// Try only rows never attempted before. The run loop drains this way while routine drains are
    /// paused behind an unresponsive relay: a fresh claim is the row a buyer is racing other sellers
    /// for, so it gets its one attempt now rather than waiting out the pause behind rows the relay
    /// already failed. Each row costs at most one such attempt; after it, the row is paced.
    pub fresh_only: bool,
}

impl DrainLimits {
    /// A pass that may run for `budget` from now and try at most `max_rows` rows.
    pub fn within(budget: Duration, max_rows: usize) -> Self {
        Self {
            deadline: Some(tokio::time::Instant::now() + budget),
            max_rows,
            fresh_only: false,
        }
    }

    /// The same bound, restricted to rows never attempted before (see [`Self::fresh_only`]).
    pub fn fresh_within(budget: Duration, max_rows: usize) -> Self {
        Self {
            fresh_only: true,
            ..Self::within(budget, max_rows)
        }
    }

    /// No bound at all: every pending row, however long each publish takes — the shape
    /// [`drain_once`] always had. The node's run loop never drains this way.
    pub fn unbounded() -> Self {
        Self {
            deadline: None,
            max_rows: usize::MAX,
            fresh_only: false,
        }
    }
}

/// Run one unbounded drain pass: first expire rows past their retry window, then publish every
/// remaining pending row. A confirmed publish is marked `confirmed`; a failed one bumps its attempt
/// count and stays `pending` for the next pass.
pub async fn drain_once<P: EventPublisher>(
    store: &SellerStore,
    publisher: &P,
    now_unix: i64,
) -> Result<DrainReport, StoreError> {
    drain_within(store, publisher, now_unix, DrainLimits::unbounded()).await
}

/// Run one BOUNDED drain pass: expire rows past their retry window, then publish pending rows in
/// [`SellerStore::pending_outbox_batch`] order — never-attempted rows first, then the least recently
/// attempted — until every row of the batch has been tried or `limits.deadline` arrives.
///
/// The order is what makes the bound fair. A row the relay keeps failing is recorded as attempted
/// and rotates behind the rows a pass did not reach, so neither a failing backlog nor one hung
/// publish can take the head of every pass, and a fresh publication — a claim a buyer is waiting
/// on — gets its first attempt on the very next pass.
///
/// Cancel-safe: the store is only written after a publish settles, so dropping this future leaves
/// every row exactly as a failed send would (`pending`, retried with the same event id).
pub async fn drain_within<P: EventPublisher>(
    store: &SellerStore,
    publisher: &P,
    now_unix: i64,
    limits: DrainLimits,
) -> Result<DrainReport, StoreError> {
    let mut report = DrainReport::default();
    report.expired = store.expire_outbox(now_unix)?;

    let batch = store.pending_outbox_batch(now_unix, limits.max_rows, limits.fresh_only)?;
    let total = batch.len();
    for (tried, item) in batch.into_iter().enumerate() {
        match publish_one(store, publisher, &item, now_unix, limits.deadline, &mut report).await? {
            Published::Settled => {}
            Published::NotStarted => {
                report.deferred = total - tried;
                break;
            }
            Published::CutShort => {
                report.deferred = total - tried - 1;
                break;
            }
        }
    }
    Ok(report)
}

/// Publish the ONE pending row with `dedup_key` now, within `limits.deadline` — for a caller that
/// must read that row's published id next (an award names the claim it selects), and must not wait
/// behind, or be cut off by, whatever else the outbox holds. A row that is absent, already
/// confirmed or expired is left alone and the report is empty.
pub async fn drain_key<P: EventPublisher>(
    store: &SellerStore,
    publisher: &P,
    dedup_key: &str,
    now_unix: i64,
    limits: DrainLimits,
) -> Result<DrainReport, StoreError> {
    let mut report = DrainReport::default();
    if let Some(item) = store.pending_outbox_row(dedup_key, now_unix)? {
        if publish_one(store, publisher, &item, now_unix, limits.deadline, &mut report).await?
            == Published::NotStarted
        {
            report.deferred = 1;
        }
    }
    Ok(report)
}

/// How one row's publish ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Published {
    /// Confirmed or failed on its own; the pass may continue.
    Settled,
    /// The deadline had already arrived: nothing was sent and nothing recorded.
    NotStarted,
    /// The deadline arrived mid-publish: recorded as a failed attempt, and the pass is over.
    CutShort,
}

async fn publish_one<P: EventPublisher>(
    store: &SellerStore,
    publisher: &P,
    item: &OutboxItem,
    now_unix: i64,
    deadline: Option<tokio::time::Instant>,
    report: &mut DrainReport,
) -> Result<Published, StoreError> {
    let outcome = match deadline {
        None => Ok(publisher.publish(item).await),
        Some(deadline) => {
            let Some(remaining) = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .filter(|remaining| !remaining.is_zero())
            else {
                return Ok(Published::NotStarted);
            };
            tokio::time::timeout(remaining, publisher.publish(item)).await
        }
    };
    match outcome {
        Ok(Ok(event_id)) => {
            store.mark_confirmed(item.id, &event_id, now_unix)?;
            report.confirmed += 1;
            Ok(Published::Settled)
        }
        Ok(Err(_)) => {
            store.record_attempt(item.id, now_unix)?;
            report.failed += 1;
            Ok(Published::Settled)
        }
        Err(_) => {
            // The send may already have reached the relay, so the row stays pending for a retry of
            // the same event. Recording the attempt rotates it behind the rows this pass never
            // reached: a row that hangs must not take the head of every later pass.
            store.record_attempt(item.id, now_unix)?;
            report.failed += 1;
            Ok(Published::CutShort)
        }
    }
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
            "maxplayer-seller-outbox-{label}-{}-{id}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        (SellerStore::open(&path).expect("open"), path)
    }

    fn claim_draft() -> crate::gateway::EventDraft {
        crate::gateway::claim_draft(&"e".repeat(64), &"b".repeat(64), &"s".repeat(64), crate::gateway::ClaimPayment::Sat("creqA"), &[], &Default::default())
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
        store.claim_and_enqueue(&job, &"o".repeat(64), Some("creqA"), &claim_draft(), 1, 9_999, 1).expect("claim");

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
        store.claim_and_enqueue(&job, &"o".repeat(64), Some("creqA"), &claim_draft(), 1, 9_999, 1).expect("claim");

        let publisher = FakePublisher { calls: RefCell::new(vec![]), fail: true };
        let report = drain_once(&store, &publisher, 2).await.expect("drain");
        assert_eq!(report.failed, 1);
        let row = store.outbox_row(&format!("claim:{job}")).expect("row").expect("exists");
        assert_eq!(row.0, "pending", "a failed publish retries");
        assert_eq!(row.1, 1, "attempt was recorded");
        let _ = std::fs::remove_file(&path);
    }

    fn enqueue_claim(store: &SellerStore, n: u8, now: i64) -> String {
        let job = format!("{n:02x}").repeat(32);
        store
            .claim_and_enqueue(&job, &"o".repeat(64), Some("creqA"), &claim_draft(), 1, 9_999, now)
            .expect("claim");
        format!("claim:{job}")
    }

    /// Per-row behavior: rows in `hang` never get an answer, rows in `fail` are refused, the rest are
    /// confirmed. Every call is recorded with the item it was handed.
    #[derive(Default)]
    struct ScriptedPublisher {
        calls: RefCell<Vec<OutboxItem>>,
        hang: RefCell<Vec<String>>,
        fail: RefCell<Vec<String>>,
    }

    impl ScriptedPublisher {
        fn keys(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|item| item.dedup_key.clone()).collect()
        }
    }

    impl EventPublisher for ScriptedPublisher {
        async fn publish(&self, item: &OutboxItem) -> Result<String, String> {
            self.calls.borrow_mut().push(item.clone());
            if self.hang.borrow().contains(&item.dedup_key) {
                std::future::pending::<()>().await;
            }
            if self.fail.borrow().contains(&item.dedup_key) {
                return Err("refused".into());
            }
            Ok(format!("evt-{}", item.dedup_key))
        }
    }

    /// A pass stops at its deadline: the hung publish is cut short and recorded as one attempt, and
    /// the rows behind it are untouched (still pending, zero attempts) and counted as deferred.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn bounded_pass_stops_at_its_deadline_and_leaves_unreached_rows_untouched() {
        let (store, path) = fresh_store("budget");
        let hung = enqueue_claim(&store, 1, 10);
        let behind: Vec<String> = (2..5).map(|n| enqueue_claim(&store, n, 11)).collect();
        let publisher = ScriptedPublisher::default();
        publisher.hang.borrow_mut().push(hung.clone());

        let started = tokio::time::Instant::now();
        let limits = DrainLimits::within(Duration::from_secs(15), 256);
        let report = drain_within(&store, &publisher, 20, limits).await.expect("drain");
        assert_eq!(started.elapsed(), Duration::from_secs(15), "the pass ends at its deadline");
        assert_eq!(report, DrainReport { confirmed: 0, failed: 1, expired: 0, deferred: 3 });
        assert_eq!(publisher.keys(), vec![hung.clone()]);
        assert_eq!(store.outbox_row(&hung).expect("row").expect("exists").0, "pending");
        assert_eq!(store.outbox_row(&hung).expect("row").expect("exists").1, 1);
        for key in &behind {
            let row = store.outbox_row(key).expect("row").expect("exists");
            assert_eq!((row.0.as_str(), row.1), ("pending", 0), "{key} was never reached");
        }

        // A deadline already past starts nothing and records nothing.
        let calls = publisher.calls.borrow().len();
        let spent = DrainLimits {
            deadline: Some(tokio::time::Instant::now()),
            max_rows: 256,
            fresh_only: false,
        };
        let report = drain_within(&store, &publisher, 21, spent).await.expect("drain");
        assert_eq!(report.deferred, 4);
        assert_eq!(publisher.calls.borrow().len(), calls, "a spent budget publishes nothing");
        let _ = std::fs::remove_file(&path);
    }

    /// The next pass starts with the rows the last one never reached, so a row that hangs every time
    /// cannot starve the rows behind it; the hung row is retried as the SAME signed event.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_hung_row_rotates_behind_rows_the_last_pass_never_reached() {
        let (store, path) = fresh_store("rotate");
        let hung = enqueue_claim(&store, 1, 10);
        let fresh = enqueue_claim(&store, 2, 11);
        let publisher = ScriptedPublisher::default();
        publisher.hang.borrow_mut().push(hung.clone());

        let limits = || DrainLimits::within(Duration::from_secs(15), 256);
        let first = drain_within(&store, &publisher, 20, limits()).await.expect("first");
        assert_eq!((first.failed, first.deferred), (1, 1));
        let second = drain_within(&store, &publisher, 30, limits()).await.expect("second");
        assert_eq!(second.confirmed, 1, "the row behind the hung one is published next pass");
        assert_eq!(store.outbox_row(&fresh).expect("row").expect("exists").0, "confirmed");
        assert_eq!(publisher.keys(), vec![hung.clone(), fresh.clone(), hung.clone()]);

        // The relay answers again: the retry carries the same draft and authored-at second, so it
        // signs to the same event id.
        publisher.hang.borrow_mut().clear();
        let third = drain_within(&store, &publisher, 40, limits()).await.expect("third");
        assert_eq!(third.confirmed, 1);
        let calls = publisher.calls.borrow();
        let attempts: Vec<&OutboxItem> = calls.iter().filter(|item| item.dedup_key == hung).collect();
        assert_eq!(attempts.len(), 3);
        for retry in &attempts[1..] {
            assert_eq!(retry.id, attempts[0].id);
            assert_eq!(retry.draft, attempts[0].draft);
            assert_eq!(retry.created_at_unix, attempts[0].created_at_unix);
        }
        let _ = std::fs::remove_file(&path);
    }

    /// A refused row is recorded as attempted and moves behind a row enqueued later, so a backlog
    /// the relay keeps refusing never sits ahead of a fresh claim.
    #[tokio::test(flavor = "current_thread")]
    async fn never_attempted_rows_go_first() {
        let (store, path) = fresh_store("fresh-first");
        let old = enqueue_claim(&store, 1, 10);
        let publisher = ScriptedPublisher::default();
        publisher.fail.borrow_mut().push(old.clone());
        drain_within(&store, &publisher, 20, DrainLimits::unbounded()).await.expect("first");
        let fresh = enqueue_claim(&store, 2, 30);
        publisher.calls.borrow_mut().clear();
        drain_within(&store, &publisher, 40, DrainLimits::unbounded()).await.expect("second");
        assert_eq!(publisher.keys(), vec![fresh, old]);
        let _ = std::fs::remove_file(&path);
    }

    /// While routine drains are paused, a fresh-only pass tries never-attempted rows and leaves the
    /// paced backlog alone.
    #[tokio::test(flavor = "current_thread")]
    async fn fresh_only_pass_skips_rows_already_attempted() {
        let (store, path) = fresh_store("fresh-only");
        let old = enqueue_claim(&store, 1, 10);
        let publisher = ScriptedPublisher::default();
        publisher.fail.borrow_mut().push(old.clone());
        drain_within(&store, &publisher, 20, DrainLimits::unbounded()).await.expect("first");
        let fresh = enqueue_claim(&store, 2, 30);
        publisher.calls.borrow_mut().clear();
        let limits = DrainLimits::fresh_within(Duration::from_secs(15), 256);
        let report = drain_within(&store, &publisher, 40, limits).await.expect("fresh-only");
        assert_eq!(report, DrainReport { confirmed: 1, failed: 0, expired: 0, deferred: 0 });
        assert_eq!(publisher.keys(), vec![fresh]);
        assert_eq!(store.outbox_row(&old).expect("row").expect("exists").1, 1, "backlog untouched");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_rows_are_not_published() {
        let (store, path) = fresh_store("expire");
        let job = "j".repeat(64);
        store.claim_and_enqueue(&job, &"o".repeat(64), Some("creqA"), &claim_draft(), 1, 100, 1).expect("claim");

        let publisher = FakePublisher { calls: RefCell::new(vec![]), fail: false };
        // now=200 is past expires_at=100 ⇒ the row expires and is never handed to the publisher.
        let report = drain_once(&store, &publisher, 200).await.expect("drain");
        assert_eq!(report.expired, 1);
        assert_eq!(report.confirmed, 0);
        assert!(publisher.calls.borrow().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
