//! The single relay ingester: one stream of marketplace events in, durable rows out.
//!
//! Exactly one ingester writes to the DB. It pulls parsed marketplace events from an
//! [`EventStream`] — offers to consider, awards selecting a claim — and lands each in the store
//! idempotently ([`super::store`]). Nothing else mutates the offer/award tables, so there is a
//! single writer and a single ordering; execution and payment read this state, they never race the
//! relay.
//!
//! The stream is abstracted so the pipeline (source → store) is pinned by tests against an
//! in-memory source. The concrete nostr subscription that decodes relay events into
//! [`IngestEvent`]s is the deployable [`EventStream`] the node wires at cutover.

use super::store::{Offer, SellerStore, StoreError};

/// A decoded marketplace event the ingester persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestEvent {
    /// A buyer's offer the node may claim.
    Offer(Offer),
    /// A buyer's award selecting a claim: `(award_id, job_id, buyer_pubkey)`.
    Award {
        award_id: String,
        job_id: String,
        buyer_pubkey: String,
    },
}

/// A source of decoded marketplace events. `next` yields the next event, or `None` when the stream
/// ends (the live relay stream never ends until shutdown).
///
/// An internal seam driven by the node's own single ingester loop, so the `async fn` in trait (no
/// `Send` bound on the returned future) is intentional — the lint is suppressed here.
#[allow(async_fn_in_trait)]
pub trait EventStream {
    async fn next(&mut self) -> Option<IngestEvent>;
}

/// What the ingester has persisted so far (running counts; useful for tests and status).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestCounts {
    pub offers: usize,
    pub awards: usize,
}

/// Persist one event into the store, idempotently. Returns the running counts unchanged-or-bumped.
pub fn ingest(
    store: &SellerStore,
    event: &IngestEvent,
    counts: &mut IngestCounts,
    now_unix: i64,
) -> Result<(), StoreError> {
    match event {
        IngestEvent::Offer(offer) => {
            if store.record_offer(offer, now_unix)? {
                counts.offers += 1;
            }
        }
        IngestEvent::Award {
            award_id,
            job_id,
            buyer_pubkey,
        } => {
            store.record_award(award_id, job_id, buyer_pubkey, now_unix)?;
            counts.awards += 1;
        }
    }
    Ok(())
}

/// Drain `stream` into the store until it ends, persisting each event idempotently. `clock` supplies
/// the ingest timestamp per event.
pub async fn run_ingester<S, C>(
    store: &SellerStore,
    stream: &mut S,
    mut clock: C,
) -> Result<IngestCounts, StoreError>
where
    S: EventStream,
    C: FnMut() -> i64,
{
    let mut counts = IngestCounts::default();
    while let Some(event) = stream.next().await {
        ingest(store, &event, &mut counts, clock())?;
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fresh_store(label: &str) -> (SellerStore, std::path::PathBuf) {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "mobee-seller-ingest-{label}-{}-{id}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        (SellerStore::open(&path).expect("open"), path)
    }

    struct VecStream(std::collections::VecDeque<IngestEvent>);

    impl EventStream for VecStream {
        async fn next(&mut self) -> Option<IngestEvent> {
            self.0.pop_front()
        }
    }

    fn offer(id: &str) -> Offer {
        Offer {
            offer_id: id.to_owned(),
            buyer_pubkey: "b".repeat(64),
            amount_sats: 100,
            unit: "sat".to_owned(),
            task: "task".to_owned(),
            deadline_unix: 9_999,
            targeted: true,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ingests_offers_idempotently() {
        let (store, path) = fresh_store("offers");
        let o = offer(&"a".repeat(64));
        let mut stream = VecStream(vec![IngestEvent::Offer(o.clone()), IngestEvent::Offer(o)].into());
        let counts = run_ingester(&store, &mut stream, || 1).await.expect("run");
        assert_eq!(counts.offers, 1, "a re-seen offer is not counted twice");
        assert_eq!(store.health().expect("h").offers, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_award_after_a_claim_creates_the_job() {
        let (store, path) = fresh_store("award");
        let job = "j".repeat(64);
        let offer_id = "o".repeat(64);
        let draft = crate::gateway::claim_draft(&offer_id, &"b".repeat(64), &"s".repeat(64), "creqA");
        store.claim_and_enqueue(&job, &offer_id, &draft, 1, 9_999, 1).expect("claim");

        let mut stream = VecStream(
            vec![IngestEvent::Award {
                award_id: "w".repeat(64),
                job_id: job.clone(),
                buyer_pubkey: "b".repeat(64),
            }]
            .into(),
        );
        let counts = run_ingester(&store, &mut stream, || 2).await.expect("run");
        assert_eq!(counts.awards, 1);
        assert_eq!(
            store.job_state(&job).expect("state"),
            Some(super::super::store::JobState::Awarded)
        );
        let _ = std::fs::remove_file(&path);
    }
}
