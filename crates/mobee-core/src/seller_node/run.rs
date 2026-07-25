//! The seller node's live run loop — the durable node's relay surface.
//!
//! Boot opens the durable [`SellerNode`] (exclusive home lock + store + wallet/signer actors),
//! reconciles durable state, then connects ONE authenticated relay client (NIP-42) that both
//! ingests marketplace events and — via the shared [`RelayPublisher`] — drains the outbox. The loop
//! routes each event to the store: offers to consider, awards that bind a claim, gift-wraps that
//! settle a delivery.
//!
//! SCAFFOLD STATUS: boot + the drain/dispatch skeleton are wired. The offer→claim, award→execute,
//! and gift-wrap→pay arms + the #150 relay-stall watchdog + #162 recovery-retry are ported on top of
//! this in the following cutover steps (marked `PORT` below); `mobee sell` is NOT yet pointed here.

use std::time::Duration;

use nostr_sdk::prelude::{
    Client, Filter, Keys, Kind, RelayOptions, RelayPoolNotification, RelayUrl,
};

use crate::gateway::{self, claim_draft, git_result_draft, parse_award, parse_offer, ParsedOffer};
use crate::home::{self, MobeeHome};
use crate::job_lifecycle::{event_to_draft, job_hash_for_offer};
use crate::kinds::{JOB_AWARD_KIND, JOB_OFFER_KIND};
use crate::receipt::{ReceiptPreimage, EXEC_METADATA_COMMITMENT_EMPTY};
use crate::relay_auth::{self, AuthWait};
use crate::seller::rate_gate_allows;
use crate::seller_exec::{
    compose_agent_prompt, delivery_message, job_workdir, run_agent_job, run_agent_with_retry,
    seller_delivery_kind, seller_exec_metadata, unified_job_timeout,
};
use crate::seller_git::{self, DeliveryAgentIdentity};

use super::outbox::drain_once;
use super::publisher::RelayPublisher;
use super::{now_unix, NodeError, SellerNode};

/// How long (seconds) the outbox publisher keeps retrying a claim event before it expires. Matches
/// the legacy claim TTL: a claim outlives a slow relay but never lingers indefinitely.
const CLAIM_PUBLISH_WINDOW_SECS: i64 = 3600;
/// Upper bound on parked claims awaiting an award (bounded memory / back-pressure), mirroring the
/// legacy AWAITING_AWARD_CAP: a claim is cheap (no compute until the award), so several may be held.
const AWAITING_AWARD_CAP: i64 = 32;
/// Bounded agent-run attempts within the job deadline before the claim is failed (mirrors the legacy
/// MAX_AGENT_ATTEMPTS): a transient agent error is retried while the deadline still has room.
const MAX_AGENT_ATTEMPTS: u32 = 3;
/// How long (seconds) the outbox publisher keeps retrying a result event before it expires. Longer
/// than the claim window — the delivery is the earned artifact and must survive a slow/absent buyer.
const RESULT_PUBLISH_WINDOW_SECS: i64 = 86_400;

/// The pure claim/skip decision over a parsed offer — no I/O, so the money-safety ordering
/// (targeting, deadline-expiry, rate floor) is unit-testable. Mirrors the legacy `classify_offer`
/// gates that do not need durable state; the store-backed dedup + capacity checks ride on top in
/// [`SellerNodeRunner::on_offer`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaimDecision {
    /// Claim it — carries the job deadline resolved for execution.
    Claim { deadline_unix: u64 },
    /// Skip it, with a named reason (never a silent drop).
    Skip(&'static str),
}

/// The pure award-match decision over a parked claim — no I/O, so the security-critical rule is
/// unit-testable. An award binds our claim ONLY when its author is the offer's buyer (a third party
/// can never drive execute or release) AND it names OUR published claim id; if it names a different
/// claim the buyer picked another seller and we release; if our claim is not yet on the wire, or the
/// author is not the buyer, we ignore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AwardMatch {
    /// The award names our claim — bind it and execute.
    Execute,
    /// The award names a different claim — the buyer picked another seller; release ours.
    Release,
    /// Not ours / not the buyer / our claim not yet published — do nothing.
    Ignore,
}

/// Match an award against our parked claim. `our_claim_id` is the published id of our claim for this
/// offer (`None` until it has been confirmed on the relay). Pure over its inputs.
fn match_award(
    award_claim_id: &str,
    our_claim_id: Option<&str>,
    award_author: &str,
    offer_buyer: &str,
) -> AwardMatch {
    // Authorization: only the offer's buyer may award. A spoofed award (author != buyer) can never
    // drive execute OR release.
    if award_author != offer_buyer {
        return AwardMatch::Ignore;
    }
    match our_claim_id {
        Some(id) if id == award_claim_id => AwardMatch::Execute,
        Some(_) => AwardMatch::Release,
        None => AwardMatch::Ignore,
    }
}

/// Build the delivery co-signature preimage. `creq_hash` is derived from the STORED claim-time creq
/// (`stored_creq`) — never a rebuild from live config — so a config change between claim and delivery
/// cannot break the buyer/seller cosignature (audit N-4 / invariant 8). The specific realized mint is
/// deliberately NOT in the preimage (the seller signs at delivery, before the buyer picks a mint); the
/// accepted-mint SET is bound via this creq hash, so buyer/seller cosigs agree for ANY accepted mint.
#[allow(clippy::too_many_arguments)]
fn delivery_receipt_preimage(
    job_id: &str,
    task: &str,
    amount: u64,
    buyer_pubkey: &str,
    seller_pubkey: &str,
    commit_oid: &str,
    delivery_kind: &str,
    stored_creq: &str,
) -> ReceiptPreimage {
    ReceiptPreimage {
        job_hash: job_hash_for_offer(job_id, task, amount),
        offer_id: job_id.to_owned(),
        amount,
        unit: "sat".to_owned(),
        buyer_pubkey: buyer_pubkey.to_owned(),
        seller_pubkey: seller_pubkey.to_owned(),
        delivery_integrity_hash: commit_oid.to_owned(),
        delivery_kind: delivery_kind.to_owned(),
        exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
        creq_hash: Some(gateway::creq_hash_hex(stored_creq)),
    }
}

/// Relay-stall watchdog threshold (seconds): `interval_secs * missed_intervals`, each clamped to at
/// least 1 so the product is always positive (the watchdog can never trip on the first tick). Pure,
/// unit-testable. Ported verbatim from the daemon (#150/#142).
fn stall_threshold_secs(interval_secs: u64, missed_intervals: u32) -> u64 {
    interval_secs
        .max(1)
        .saturating_mul(u64::from(missed_intervals.max(1)))
}

/// Whether the live subscription is presumed dead: no own heartbeat has round-tripped for at least
/// `threshold_secs`. Pure over an elapsed-seconds reading (fake-clock testable).
fn subscription_stalled(elapsed_secs: u64, threshold_secs: u64) -> bool {
    elapsed_secs >= threshold_secs
}

/// Overlap margin (seconds) subtracted from the last-known-good heartbeat timestamp when computing
/// the post-stall resubscribe `since` cursor, so events published during the stall backfill; the
/// idempotent handlers (offer dedup, award match against still-parked claims, wrap pay-once via the
/// receipt dedup) absorb the overlap re-delivery.
const STALL_OVERLAP_MARGIN_SECS: u64 = 60;
/// Bounded connect-phase recovery attempts within ONE stall recovery before yielding to the next
/// heartbeat tick (#162): a relay that drops the socket before completing NIP-42 is retried with a
/// short backoff rather than waiting a whole stall interval.
const RECOVERY_MAX_ATTEMPTS: u32 = 3;
/// Short backoff between the bounded recovery attempts (#162).
const RECOVERY_BACKOFF: Duration = Duration::from_secs(2);

/// The own-heartbeat watchdog subscription: the seller's OWN kind-30340 addressable heartbeat, so
/// each published heartbeat round-trips back on this live subscription and proves the receive path is
/// alive. Keyed by author + kind + `d` (an addressable event is superseded in place, but each
/// republish still arrives as a fresh delivery on the sub).
fn self_heartbeat_subscription_filter(seller_pubkey: nostr_sdk::PublicKey) -> Filter {
    Filter::new()
        .kind(Kind::Custom(crate::heartbeat::SELLER_HEARTBEAT_KIND))
        .author(seller_pubkey)
        .identifier(crate::heartbeat::SELLER_HEARTBEAT_D)
}

/// The seller-receive classification on the node redeem path (finding S, ported from the daemon).
#[derive(Debug)]
enum RedeemDecision {
    /// Receive succeeded — finalize a receipt for this redeemed amount.
    Finalize(u64),
    /// Idempotent re-see: already spent AND a COMPLETED receipt exists — we already collected and
    /// receipted it. No-op; never double-collect / re-receipt.
    IdempotentNoOp,
    /// Fail closed — do NOT finalize; refuse (buffer for manual reconcile), with a named reason.
    Refuse(String),
}

/// True when a receive error is the mint reporting the token already spent — the one idempotent
/// surface on the node redeem path (the node's receipt dedup lives in the store, so there is no
/// journal "already receipted" string). Substring match: cdk surfaces no typed already-spent error.
fn is_already_spent(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("already spent") || lower.contains("already redeemed")
}

/// Classify a seller-receive result (finding S). The load-bearing rule: NEVER infer "our swap already
/// landed" from a pending-receive breadcrumb — the breadcrumb is written before EVERY swap, so it
/// proves only intent. Inferring collection from it would let a malicious buyer replay an
/// already-redeemed seller-locked token against a NEW same-value job and forge a receipt for zero new
/// funds (theft-of-service). The ONLY positive proof of OUR OWN prior collection is a COMPLETED
/// receipt for this job, read FAIL-CLOSED: already-spent + has_receipt(true) ⇒ idempotent no-op;
/// already-spent + has_receipt(false) ⇒ refuse (replay/theft or a genuine interrupted redeem — both
/// indistinguishable, so fail closed); has_receipt read error ⇒ refuse. Any non-already-spent error
/// also refuses. `has_receipt` is a closure so the store is read only on the already-spent branch and
/// the decision is unit-testable without a mint.
fn classify_redeem_outcome(
    receive_result: Result<u64, String>,
    has_receipt: impl FnOnce() -> Result<bool, String>,
) -> RedeemDecision {
    match receive_result {
        Ok(amount) => RedeemDecision::Finalize(amount),
        Err(error) if !is_already_spent(&error) => RedeemDecision::Refuse(error),
        Err(error) => match has_receipt() {
            Ok(true) => RedeemDecision::IdempotentNoOp,
            Ok(false) => RedeemDecision::Refuse(error),
            Err(read_err) => {
                RedeemDecision::Refuse(format!("receipt read failed (fail-closed): {read_err}"))
            }
        },
    }
}

/// The seal-sender guard: a payment settles a job ONLY when the authenticated NIP-17 seal sender is
/// the bound offer buyer (the pubkey folded into the seller-signed receipt preimage). A third party
/// can never pay-once and close someone else's job.
fn seal_sender_is_bound_buyer(seal_sender: &str, offer_buyer: &str) -> bool {
    seal_sender == offer_buyer
}

/// Whether a job resumed from durable state on (re)start must be re-driven to execution (charter
/// invariant 4, fallback form): a job left `awarded` (award seen, delivery not started) or
/// `executing` (interrupted mid-run) is re-executed, so a process that dies mid-job resumes rather
/// than losing the award. `delivered` jobs are left for the pay path; terminal (`paid`/`failed`)
/// never re-run. Pure so the selection is unit-testable. Re-execution is idempotent: the delivery
/// snapshot is deterministic (stored award-date) and `deliver_and_enqueue` dedups, so a re-created
/// delivery lands exactly once.
fn should_resume_execution(state: super::store::JobState) -> bool {
    matches!(
        state,
        super::store::JobState::Awarded | super::store::JobState::Executing
    )
}

/// Decide whether to claim `offer`, applying the always-on money-safety gates in the legacy order:
/// a lapsed offer is refused BEFORE its deadline is re-derived (never resurrect a stale offer with a
/// fresh `now + timeout`), then the targeting/rate gate. Pure over (offer, config, now).
fn classify_offer(
    offer: &ParsedOffer,
    seller: &crate::home::SellerConfig,
    seller_pubkey: &str,
    now_unix: u64,
) -> ClaimDecision {
    // Offer-freshness (money-safety): an offer whose own absolute deadline already passed is dead,
    // refused here before `job_deadline_unix` could hand it a fresh window.
    if offer.deadline_unix <= now_unix {
        return ClaimDecision::Skip("offer deadline already passed (lapsed; never resurrected)");
    }
    if rate_gate_allows(offer, seller_pubkey, seller.rate_sats, seller.claim_open_pool).is_err() {
        return ClaimDecision::Skip("rate-gate refused (untargeted without opt-in / below rate)");
    }
    ClaimDecision::Claim {
        deadline_unix: crate::seller::job_deadline_unix(offer, seller, now_unix),
    }
}

/// How long boot waits for the relay connection and the NIP-42 challenge.
const CONNECT_WAIT: Duration = Duration::from_secs(20);
/// Cadence of the outbox drain / housekeeping tick.
const DRAIN_INTERVAL: Duration = Duration::from_secs(5);

/// A booted seller node with its live relay surface.
pub struct SellerNodeRunner {
    node: SellerNode,
    client: Client,
    publisher: RelayPublisher,
    relay_url: String,
    seller_pubkey: nostr_sdk::PublicKey,
}

impl SellerNodeRunner {
    /// Boot the node and connect its authenticated relay client.
    ///
    /// Custody rule: the seller key lives in exactly two places — the signer actor (opened by
    /// [`SellerNode::open`]) and THIS authenticated relay client, constructed once below. It is never
    /// exposed by an accessor, logged, or serialized. The client holds it because mobee-relay
    /// authenticates the seller via NIP-42 (signing the challenge) before it will deliver the
    /// p-gated kind-1059 payment wraps.
    pub async fn boot(home: MobeeHome) -> Result<Self, NodeError> {
        let relay_url = home.config.relay_url.clone();

        // Read the seller secret ONCE, here, to build the authenticated client (single construction
        // site — see the custody rule above). Dropped as soon as the client owns the keys.
        let secret = home::read_secret_key_hex(&home)?;
        let keys = Keys::parse(&secret)
            .map_err(|error| NodeError::Relay(format!("seller key parse: {error}")))?;
        drop(secret);

        let node = SellerNode::open(home).await?;

        // Reconcile durable state before serving anything live: expire stale outbox rows, report the
        // non-terminal jobs that resume. Reconcile must NOT release parked claims (invariant 5).
        match node.reconcile_on_start(now_unix()) {
            Ok(report) => eprintln!(
                "seller node reconcile: resumed_jobs={} expired_outbox={} pending_outbox={}",
                report.resumed_jobs.len(),
                report.expired_outbox,
                report.pending_outbox
            ),
            Err(error) => eprintln!("seller node reconcile failed on startup (continuing): {error}"),
        }

        let seller_pubkey = keys.public_key();
        let client = Client::new(keys);
        // Seller receive depends on NIP-42; keep auto-auth ON so a relay that challenges on the REQ
        // (not just connect) still authenticates.
        client.automatic_authentication(true);
        client
            .pool()
            .add_relay(&relay_url, RelayOptions::default().reconnect(true))
            .await
            .map_err(|error| NodeError::Relay(format!("add relay: {error}")))?;

        // Subscribe the relay's notification stream BEFORE connect — `Authenticated` is emitted once
        // and never re-emitted, so a receiver created after connect could miss it.
        let parsed_relay = RelayUrl::parse(&relay_url)
            .map_err(|error| NodeError::Relay(format!("parse relay url: {error}")))?;
        let relay = client
            .relays()
            .await
            .get(&parsed_relay)
            .cloned()
            .ok_or_else(|| NodeError::Relay("relay missing after add_relay".into()))?;
        let mut relay_notifications = relay.notifications();
        client.connect().await;
        client.wait_for_connection(CONNECT_WAIT).await;
        match relay_auth::wait_for_nip42_auth(&mut relay_notifications, CONNECT_WAIT).await {
            Ok(AuthWait::Authenticated) => eprintln!("seller node relay authenticated (NIP-42)"),
            Ok(AuthWait::NoChallenge) => eprintln!(
                "seller node WARN: no NIP-42 challenge within {CONNECT_WAIT:?}; proceeding \
                 (auto-auth stays ON — a challenge on the REQ still authenticates). p-gated kind-1059 \
                 receive may be degraded until auth completes."
            ),
            Err(error) => return Err(NodeError::Relay(format!("NIP-42 auth: {error}"))),
        }

        let publisher = RelayPublisher::new(node.signer().clone(), client.clone(), &relay_url);

        Ok(Self {
            node,
            client,
            publisher,
            relay_url,
            seller_pubkey,
        })
    }

    /// The seller public key (hex).
    pub fn seller_pubkey(&self) -> String {
        self.seller_pubkey.to_hex()
    }

    /// Subscribe the marketplace filters (offers/awards/gift-wraps) plus, when heartbeat is enabled,
    /// the own-heartbeat watchdog subscription. `since` is `Some(overlap)` on a post-stall resubscribe
    /// so events published during the stall backfill; `None` at boot. Reused by boot and by the
    /// watchdog's reconnect so both paths subscribe the SAME set.
    async fn subscribe_all(
        &self,
        since: Option<nostr_sdk::Timestamp>,
        heartbeat_enabled: bool,
    ) -> Result<(), NodeError> {
        let apply = |filter: Filter| match since {
            Some(cursor) => filter.since(cursor),
            None => filter,
        };
        let offer_filter = apply(
            Filter::new()
                .kind(Kind::Custom(JOB_OFFER_KIND))
                .pubkey(self.seller_pubkey),
        );
        let award_filter = apply(
            Filter::new()
                .kind(Kind::Custom(JOB_AWARD_KIND))
                .hashtag(crate::gateway::MOBEE_TAG)
                .pubkey(self.seller_pubkey),
        );
        let wrap_filter = apply(Filter::new().kind(Kind::GiftWrap).pubkey(self.seller_pubkey));
        for filter in [offer_filter, award_filter, wrap_filter] {
            self.client
                .subscribe(filter, None)
                .await
                .map_err(|error| NodeError::Relay(format!("subscribe: {error}")))?;
        }
        if heartbeat_enabled {
            self.client
                .subscribe(self_heartbeat_subscription_filter(self.seller_pubkey), None)
                .await
                .map_err(|error| NodeError::Relay(format!("subscribe self-heartbeat: {error}")))?;
        }
        Ok(())
    }

    /// Run the live loop until the relay pool closes: ingests offers/awards/gift-wraps, drains the
    /// outbox on a periodic tick, and — when heartbeat is enabled — publishes an own-heartbeat each
    /// heartbeat tick and runs the #150 relay-stall watchdog (reconnect + resubscribe-with-overlap if
    /// no own heartbeat has round-tripped within the stall threshold), with #162 bounded recovery
    /// retries.
    pub async fn run(self) -> Result<(), NodeError> {
        // Heartbeat + relay-stall watchdog config. Disabled ⇒ no heartbeat publish and the watchdog
        // branch is inert (the loop only waits on the drain tick + relay stream).
        let hb = &self.node.home().config.seller_heartbeat;
        let heartbeat_enabled = crate::heartbeat::resolve_enabled(hb);
        let heartbeat_interval_secs = crate::heartbeat::resolve_interval_secs(hb);
        let stall_missed_intervals = crate::heartbeat::resolve_stall_missed_intervals(hb);
        let stall_threshold = stall_threshold_secs(heartbeat_interval_secs, stall_missed_intervals);

        // The relay handle for the watchdog's reconnect (fresh notification receiver + NIP-42 re-auth).
        let parsed_relay = RelayUrl::parse(&self.relay_url)
            .map_err(|error| NodeError::Relay(format!("parse relay url: {error}")))?;
        let relay = self
            .client
            .relays()
            .await
            .get(&parsed_relay)
            .cloned()
            .ok_or_else(|| NodeError::Relay("relay missing in run loop".into()))?;

        let mut notifications = self.client.notifications();
        self.subscribe_all(None, heartbeat_enabled).await?;
        eprintln!(
            "seller node live: pubkey={} relay={}",
            self.seller_pubkey.to_hex(),
            self.relay_url
        );
        if heartbeat_enabled {
            eprintln!(
                "seller node heartbeat+watchdog enabled: kind-30340 every {heartbeat_interval_secs}s; \
                 reconnect if no own heartbeat round-trips within {stall_threshold}s \
                 ({stall_missed_intervals} missed intervals)"
            );
        }

        // Drain anything reconcile left pending before the first tick.
        self.drain().await;

        // Resume execution for jobs a process restart left mid-flight (invariant 4, fallback form):
        // an `awarded`/`executing` job is re-driven through execute_job so a crash mid-job resumes
        // instead of losing the award. Idempotent — a re-created delivery lands exactly once
        // (deterministic snapshot + deliver_and_enqueue dedup). Runs once at boot, before the loop.
        match self.node.store().resumable_jobs() {
            Ok(jobs) => {
                for (job_id, state) in jobs {
                    if should_resume_execution(state) {
                        eprintln!(
                            "seller node resume: re-driving execution for job_id={job_id} (state={state:?})"
                        );
                        self.execute_job(&job_id).await;
                    }
                }
            }
            Err(error) => {
                eprintln!("seller node resume: resumable_jobs read failed (continuing): {error}")
            }
        }

        let mut drain_tick = tokio::time::interval(DRAIN_INTERVAL);
        let mut heartbeat_tick =
            tokio::time::interval(Duration::from_secs(heartbeat_interval_secs.max(1)));
        // Watchdog liveness clocks: monotonic instant (staleness measure, robust to wall-clock jumps)
        // + unix stamp (resubscribe `since` cursor). Seeded to "now" so a healthy node never trips
        // before its first heartbeat round-trips.
        let mut last_self_heartbeat_seen = tokio::time::Instant::now();
        let mut last_self_heartbeat_seen_unix = now_unix();
        loop {
            tokio::select! {
                _ = drain_tick.tick() => {
                    self.drain().await;
                    continue;
                }
                // The heartbeat tick rides the SAME loop (never a blocking side-thread). The watchdog
                // check runs BEFORE publishing (it evaluates whether PRIOR heartbeats round-tripped).
                _ = heartbeat_tick.tick(), if heartbeat_enabled => {
                    let stall_elapsed = last_self_heartbeat_seen.elapsed().as_secs();
                    if subscription_stalled(stall_elapsed, stall_threshold) {
                        let overlap_since = nostr_sdk::Timestamp::from(
                            last_self_heartbeat_seen_unix
                                .saturating_sub(STALL_OVERLAP_MARGIN_SECS as i64)
                                .max(0) as u64,
                        );
                        eprintln!(
                            "seller node RELAY-STALL detected: no own heartbeat round-tripped in \
                             {stall_elapsed}s (threshold {stall_threshold}s); reconnecting + \
                             resubscribing with since={} overlap",
                            overlap_since.as_secs()
                        );
                        match self.recover_stall(&relay, overlap_since, heartbeat_enabled).await {
                            Ok(attempts) => {
                                let outage = now_unix().saturating_sub(last_self_heartbeat_seen_unix);
                                // Grace: reset the watchdog clock so it does not immediately re-fire
                                // before the first post-reconnect heartbeat round-trips.
                                last_self_heartbeat_seen = tokio::time::Instant::now();
                                last_self_heartbeat_seen_unix = now_unix();
                                eprintln!(
                                    "seller node RELAY-STALL recovery SUCCEEDED (attempts={attempts}, \
                                     outage={outage}s): reconnected + resubscribed \
                                     (offers+awards+1059+heartbeat, since={} overlap)",
                                    overlap_since.as_secs()
                                );
                            }
                            Err(error) => {
                                // Leave the clocks untouched so the next heartbeat tick retries.
                                eprintln!(
                                    "seller node RELAY-STALL recovery FAILED (will retry next heartbeat tick): {error}"
                                );
                            }
                        }
                    }
                    self.publish_heartbeat().await;
                    continue;
                }
                recv = notifications.recv() => {
                    match recv {
                        Ok(RelayPoolNotification::Event { event, .. }) => {
                            // Own-heartbeat round-trip: our kind-30340 coming back proves the RECEIVE
                            // path is alive. Refresh the liveness clocks and drop it — heartbeats are
                            // diagnostic and never route to offer/award/payment handling.
                            if event.kind.as_u16() == crate::heartbeat::SELLER_HEARTBEAT_KIND
                                && event.pubkey == self.seller_pubkey
                            {
                                last_self_heartbeat_seen = tokio::time::Instant::now();
                                last_self_heartbeat_seen_unix = now_unix();
                                continue;
                            }
                            match event.kind {
                                k if k.as_u16() == JOB_OFFER_KIND => self.on_offer(&event).await,
                                k if k.as_u16() == JOB_AWARD_KIND => self.on_award(&event).await,
                                Kind::GiftWrap => self.on_gift_wrap(&event).await,
                                _ => {}
                            }
                            self.drain().await;
                        }
                        Ok(RelayPoolNotification::Shutdown) => {
                            eprintln!("seller node: relay pool shutdown; loop ending");
                            break;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            // A broadcast lag is recoverable — never go permanently deaf.
                            eprintln!("seller node WARN: notification stream {error}; continuing");
                            continue;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Publish one own-heartbeat (kind-30340) — best-effort liveness/discovery + the watchdog's
    /// round-trip probe. Signed through the signer actor and sent on the shared client; a failure is
    /// logged and never wedges the loop. No-op without `[seller]` config.
    async fn publish_heartbeat(&self) {
        let Some(seller) = self.node.home().config.seller.clone() else {
            return;
        };
        let job_in_flight = self.node.store().health().map(|h| h.jobs > 0).unwrap_or(false);
        let draft =
            crate::heartbeat::heartbeat_for_state(job_in_flight, seller.rate_sats).to_event_draft();
        match self.node.signer().sign(draft, now_unix()).await {
            Ok(Ok(signed)) => {
                use nostr_sdk::JsonUtil as _;
                match nostr_sdk::Event::from_json(&signed.json) {
                    Ok(event) => {
                        if let Err(error) = self.client.send_event_to([&self.relay_url], &event).await {
                            eprintln!("seller node heartbeat publish failed (continuing): {error}");
                        }
                    }
                    Err(error) => {
                        eprintln!("seller node heartbeat encode failed (continuing): {error}")
                    }
                }
            }
            Ok(Err(error)) => eprintln!("seller node heartbeat sign failed (continuing): {error}"),
            Err(error) => {
                eprintln!("seller node signer actor gone at heartbeat (continuing): {error}")
            }
        }
    }

    /// One stall recovery, with #162 bounded retries: a connect-phase failure (relay drops the socket
    /// before NIP-42 completes) is retried up to [`RECOVERY_MAX_ATTEMPTS`] with a short backoff WITHIN
    /// this recovery before yielding to the next heartbeat tick. Returns the attempt count on success.
    async fn recover_stall(
        &self,
        relay: &nostr_sdk::prelude::Relay,
        overlap_since: nostr_sdk::Timestamp,
        heartbeat_enabled: bool,
    ) -> Result<u32, NodeError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self
                .reconnect_and_resubscribe(relay, overlap_since, heartbeat_enabled)
                .await
            {
                Ok(()) => return Ok(attempt),
                Err(error) if attempt < RECOVERY_MAX_ATTEMPTS => {
                    eprintln!(
                        "seller node RELAY-STALL recovery attempt {attempt} failed ({error}); \
                         retrying in {}s",
                        RECOVERY_BACKOFF.as_secs()
                    );
                    tokio::time::sleep(RECOVERY_BACKOFF).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Tear down the silently-dead connection and rebuild it: reconnect, re-run NIP-42 (the p-gated
    /// kind-1059 resubscribe depends on it, same as boot), then resubscribe ALL filters with
    /// `since = overlap`. Clears the stale subscriptions only AFTER a successful reconnect+auth so a
    /// failed recovery never leaves the node reconnected-but-deaf.
    async fn reconnect_and_resubscribe(
        &self,
        relay: &nostr_sdk::prelude::Relay,
        overlap_since: nostr_sdk::Timestamp,
        heartbeat_enabled: bool,
    ) -> Result<(), NodeError> {
        // Fresh receiver BEFORE the reconnect so the `Authenticated` notification can't be missed.
        let mut relay_notifications = relay.notifications();
        self.client.disconnect().await;
        self.client.connect().await;
        self.client.wait_for_connection(CONNECT_WAIT).await;
        relay_auth::wait_for_nip42_auth(&mut relay_notifications, CONNECT_WAIT)
            .await
            .map_err(|error| NodeError::Relay(format!("reconnect NIP-42 auth: {error}")))?;
        self.client.unsubscribe_all().await;
        self.subscribe_all(Some(overlap_since), heartbeat_enabled).await
    }

    /// Consider one offer event: parse it, apply the money-safety gates, and — if admitted — journal
    /// the claim (creq + claim event) into the store in one transaction, then drain so the claim is
    /// published. Every non-claim path logs a named reason; there is no silent drop.
    ///
    /// The claim-time creq is authored here from the seller's OWN config (accepted mints + rate) and
    /// journaled via `claim_and_enqueue`, so delivery later signs the STORED creq's hash (invariant
    /// 8) and the restart redeem-guard reads its mints (Fix Q) — never a rebuild from live config.
    async fn on_offer(&self, event: &nostr_sdk::Event) {
        let Some(seller) = self.node.home().config.seller.clone() else {
            eprintln!("seller node offer skipped: no [seller] config");
            return;
        };
        let draft = event_to_draft(event);
        let offer = match parse_offer(&draft) {
            Ok(offer) => offer,
            Err(error) => {
                eprintln!("seller node offer skip id={}: unparseable ({error})", event.id);
                return;
            }
        };
        // Contribution offers are a later slice: refuse (never run a contribution as from-scratch),
        // matching the legacy fail-closed posture. A malformed contribution is likewise refused.
        match crate::contribution::parse_contribution_offer(&draft.tags) {
            Ok(None) => {}
            Ok(Some(_)) => {
                eprintln!(
                    "seller node offer skip id={}: contribution offers not served by the node yet",
                    event.id
                );
                return;
            }
            Err(error) => {
                eprintln!("seller node offer skip id={}: malformed contribution ({error})", event.id);
                return;
            }
        }

        let seller_pubkey = self.seller_pubkey.to_hex();
        let now = now_unix();
        let deadline_unix = match classify_offer(&offer, &seller, &seller_pubkey, now as u64) {
            ClaimDecision::Claim { deadline_unix } => deadline_unix,
            ClaimDecision::Skip(reason) => {
                eprintln!("seller node offer skip id={}: {reason}", event.id);
                return;
            }
        };

        // Capacity back-pressure: never hold unbounded parked claims.
        match self.node.store().health() {
            Ok(health) if health.open_claims >= AWAITING_AWARD_CAP => {
                eprintln!(
                    "seller node offer skip id={}: awaiting-award backlog full (cap {AWAITING_AWARD_CAP})",
                    event.id
                );
                return;
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("seller node offer skip id={}: store health read failed ({error})", event.id);
                return;
            }
        }

        // The job id IS the offer event id (as on the legacy path); the buyer is its author.
        let job_id = event.id.to_hex();
        let buyer_pubkey = event.pubkey.to_hex();

        // Journal the offer facts BEFORE claiming: the award arm reads the buyer to authorize an
        // award (author MUST be the offer's buyer), and the pay path reads amount/unit as the redeem
        // terms. Idempotent — a re-seen offer is a no-op.
        if let Err(error) = self.node.store().record_offer(
            &super::store::Offer {
                offer_id: job_id.clone(),
                buyer_pubkey: buyer_pubkey.clone(),
                amount_sats: offer.amount,
                unit: offer.unit.clone(),
                task: offer.task.clone(),
                deadline_unix: offer.deadline_unix as i64,
                targeted: offer.is_targeted(),
            },
            now,
        ) {
            eprintln!("seller node offer skip id={job_id}: record offer failed ({error})");
            return;
        }

        let creq = match gateway::creq::build_seller_creq(
            &job_id,
            offer.amount,
            &offer.unit,
            &self.node.home().config.accepted_mints,
            &seller_pubkey,
        ) {
            Ok(creq) => creq,
            Err(error) => {
                eprintln!("seller node offer skip id={job_id}: creq build failed ({error})");
                return;
            }
        };
        let claim = claim_draft(&job_id, &buyer_pubkey, &seller_pubkey, &creq);
        match self.node.store().claim_and_enqueue(
            &job_id,
            &job_id,
            &creq,
            &claim,
            now,
            now + CLAIM_PUBLISH_WINDOW_SECS,
            now,
        ) {
            Ok(super::store::Claimed::New) => {
                eprintln!(
                    "seller node claimed job_id={job_id} buyer={buyer_pubkey} amount={} deadline={deadline_unix} (awaiting award)",
                    offer.amount
                );
                // The caller drains after dispatch, publishing the just-enqueued claim.
            }
            Ok(super::store::Claimed::Idempotent) => {
                eprintln!("seller node offer id={job_id}: already claimed (dedup no-op)");
            }
            Err(error) => eprintln!("seller node claim failed job_id={job_id}: {error}"),
        }
    }

    /// Handle one award event: authorize it (author must be the offer's buyer), decide whether it
    /// names OUR claim, and bind or release accordingly. Binding records the award (which moves the
    /// claim → awarded and creates the job row); execution of the awarded job is the next port step.
    async fn on_award(&self, event: &nostr_sdk::Event) {
        let draft = event_to_draft(event);
        let Some(award) = parse_award(&draft) else {
            return;
        };
        let job_id = award.offer_id.clone();

        // Only an offer we recorded can be awarded to us; its buyer is the sole authorized awarder.
        let buyer = match self.node.store().offer_facts(&job_id) {
            Ok(Some((buyer, _, _))) => buyer,
            Ok(None) => {
                eprintln!("seller node award ignore job_id={job_id}: no offer of ours recorded");
                return;
            }
            Err(error) => {
                eprintln!("seller node award ignore job_id={job_id}: offer read failed ({error})");
                return;
            }
        };
        // We must hold a parked claim for this job (journaled creq present ⇒ we claimed).
        match self.node.store().job_creq(&job_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                eprintln!("seller node award ignore job_id={job_id}: no claim of ours");
                return;
            }
            Err(error) => {
                eprintln!("seller node award ignore job_id={job_id}: claim read failed ({error})");
                return;
            }
        }

        // Ensure our claim is on the wire, then read its published id for the win check.
        self.drain().await;
        let our_claim_id = match self.node.store().outbox_row(&format!("claim:{job_id}")) {
            Ok(Some((_, _, published))) => published,
            _ => None,
        };
        let award_author = event.pubkey.to_hex();
        match match_award(&award.claim_id, our_claim_id.as_deref(), &award_author, &buyer) {
            AwardMatch::Execute => {
                match self
                    .node
                    .store()
                    .record_award(&event.id.to_hex(), &job_id, &buyer, now_unix())
                {
                    Ok(super::store::Awarded::New) => {
                        eprintln!("seller node awarded job_id={job_id} buyer={buyer} — executing");
                        self.execute_job(&job_id).await;
                    }
                    Ok(super::store::Awarded::Duplicate) => {
                        eprintln!("seller node award dedup job_id={job_id} (already recorded)")
                    }
                    Ok(super::store::Awarded::NoClaim) => {
                        eprintln!("seller node award job_id={job_id}: no claim to bind")
                    }
                    Err(error) => eprintln!("seller node award record failed job_id={job_id}: {error}"),
                }
            }
            AwardMatch::Release => {
                match self.node.store().release_claim(&job_id, now_unix()) {
                    Ok(()) => eprintln!(
                        "seller node released claim job_id={job_id}: buyer picked another seller's claim"
                    ),
                    Err(error) => eprintln!("seller node release failed job_id={job_id}: {error}"),
                }
            }
            AwardMatch::Ignore => eprintln!(
                "seller node award ignore job_id={job_id}: author not the offer buyer, or our claim not yet published"
            ),
        }
    }

    /// Execute an awarded job end to end: run the agent in a fresh empty-base workdir, snapshot its
    /// output into ONE delivery commit dated at the STORED award time (so a re-created commit after a
    /// restart keeps the same oid — invariant 2), push it under the seller's NIP-98 auth, then bind
    /// the trade + the delivered commit + the STORED claim-time creq's hash (audit N-4 / invariant 8)
    /// into a co-signature the seller signs through its actor, and journal + enqueue the result event
    /// in one transaction. Every failure path fails the job with a named reason and publishes nothing
    /// partial; the delivery journal is idempotent, so a resumed job never double-publishes.
    async fn execute_job(&self, job_id: &str) {
        let Some(seller) = self.node.home().config.seller.clone() else {
            eprintln!("seller node execute skip job_id={job_id}: no [seller] config");
            self.fail_job(job_id).await;
            return;
        };
        // Offer facts (task + amount + buyer + absolute deadline) were journaled at claim time.
        let offer = match self.node.store().offer_row(job_id) {
            Ok(Some(offer)) => offer,
            Ok(None) => {
                eprintln!("seller node execute fail job_id={job_id}: offer facts missing");
                self.fail_job(job_id).await;
                return;
            }
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: offer read failed ({error})");
                self.fail_job(job_id).await;
                return;
            }
        };
        // The claim-time creq is the single source of truth for the payment terms (audit N-4): the
        // delivery cosignature signs ITS hash, never a rebuild from live config.
        let stored_creq = match self.node.store().job_creq(job_id) {
            Ok(Some(creq)) => creq,
            _ => {
                eprintln!("seller node execute fail job_id={job_id}: stored creq missing");
                self.fail_job(job_id).await;
                return;
            }
        };
        // The delivery commit's author date is the STORED award time (stable across restarts), so a
        // re-created delivery commit is byte-identical and the re-push is a no-op (invariant 2).
        let author_date = match self.node.store().job_award_time(job_id) {
            Ok(Some(award_time)) => award_time,
            _ => now_unix(),
        };

        // Move awarded -> executing (idempotent). A failed mark is logged, never fatal.
        if let Err(error) = self.node.store().mark_executing(job_id, now_unix()) {
            eprintln!("seller node execute job_id={job_id}: mark_executing failed (continuing): {error}");
        }

        let seller_pubkey = self.seller_pubkey.to_hex();
        let identity = DeliveryAgentIdentity::for_seller(&seller_pubkey);
        let workdir = job_workdir(self.node.home(), job_id);
        if let Err(error) = seller_git::init_empty_delivery_workdir(&workdir, &identity) {
            eprintln!("seller node execute fail job_id={job_id}: workdir init failed ({error})");
            self.fail_job(job_id).await;
            return;
        }

        // Run the agent under the job's remaining deadline, retrying a transient error while the
        // deadline has room. The agent edits files in `workdir`; the node owns commit + push.
        let deadline = offer.deadline_unix.max(0) as u64;
        let prompt = compose_agent_prompt(&offer.task, &seller.git_remote, None);
        let run_started = std::time::Instant::now();
        let run_result = run_agent_with_retry(
            deadline,
            MAX_AGENT_ATTEMPTS,
            || now_unix() as u64,
            |_attempt| {
                let job_timeout = unified_job_timeout(deadline, now_unix() as u64);
                run_agent_job(&seller.agent_command, &prompt, &workdir, &identity, job_timeout)
            },
        )
        .await;
        let wall_time_ms = run_started.elapsed().as_millis() as u64;
        let usage = match run_result {
            Ok(usage) => usage,
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: agent run failed ({error})");
                self.fail_job(job_id).await;
                return;
            }
        };

        // Snapshot the agent's final workdir tree into ONE delivery commit at the stored author date.
        // An empty / no-op tree is refused with a precise reason (nothing to deliver).
        let branch = format!("mobee/{}", &job_id[..8.min(job_id.len())]);
        let message = delivery_message(&offer.task);
        if let Err(error) = seller_git::snapshot_delivery_at(
            &workdir,
            &identity,
            None,
            &branch,
            &message,
            author_date,
        ) {
            eprintln!("seller node execute fail job_id={job_id}: delivery snapshot refused ({error})");
            self.fail_job(job_id).await;
            return;
        }

        // Push under the seller's NIP-98 auth. The push authorization is signed THROUGH the signer
        // actor (which owns the seller key), so the push path is NOT a third custody site — the key
        // stays confined to the actor + the authenticated relay client, never re-read here. A
        // public/anonymous https remote takes no header (auth applies to relay-git remotes only).
        let push_header = if crate::delivery_transport::is_relay_git_locator(&seller.git_remote) {
            match self.node.signer().http_auth_header(seller.git_remote.clone()).await {
                Ok(Ok(header)) => Some(header),
                Ok(Err(error)) => {
                    eprintln!("seller node execute fail job_id={job_id}: push auth sign failed ({error})");
                    self.fail_job(job_id).await;
                    return;
                }
                Err(error) => {
                    eprintln!("seller node execute fail job_id={job_id}: signer actor gone ({error})");
                    self.fail_job(job_id).await;
                    return;
                }
            }
        } else {
            None
        };
        let commit = match seller_git::push_branch_with_header(
            &workdir,
            &seller.git_remote,
            &branch,
            push_header,
        ) {
            Ok(oid) => oid,
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: git push failed ({error})");
                self.fail_job(job_id).await;
                return;
            }
        };

        // Bind the trade + delivered commit + STORED creq hash into the co-signature preimage and
        // sign it through the signer actor (the seller key never leaves the actor).
        let delivery_kind = match seller_delivery_kind(&seller.git_remote, &branch, &commit) {
            Ok(kind) => kind,
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: delivery kind typing failed ({error})");
                self.fail_job(job_id).await;
                return;
            }
        };
        let preimage = delivery_receipt_preimage(
            job_id,
            &offer.task,
            offer.amount_sats,
            &offer.buyer_pubkey,
            &seller_pubkey,
            &commit,
            delivery_kind.as_str(),
            &stored_creq,
        );
        let seller_sig = match self.node.signer().sign_receipt_hash(preimage.digest_hex()).await {
            Ok(Ok(sig)) => sig,
            Ok(Err(error)) => {
                eprintln!("seller node execute fail job_id={job_id}: receipt sign refused ({error})");
                self.fail_job(job_id).await;
                return;
            }
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: signer actor gone ({error})");
                self.fail_job(job_id).await;
                return;
            }
        };

        // Harness-generic PUBLIC seller-claimed usage block (opportunistic; absent fields stay
        // absent). `usage` carries what the ACP driver surfaced this run — `None` when it exposed none.
        let exec_metadata = seller_exec_metadata(
            &seller.agent_command,
            seller.agent.as_deref(),
            wall_time_ms,
            usage.as_ref(),
        );
        let draft = git_result_draft(
            job_id,
            &offer.buyer_pubkey,
            &seller.git_remote,
            &branch,
            &commit,
            offer.amount_sats,
            &preimage.job_hash,
            &seller_sig,
            format!("delivery commit {commit}"),
            &exec_metadata,
        );
        // Journal the delivery + enqueue the result in one transaction. Idempotent: a resumed job
        // that already delivered re-enqueues nothing (invariant 2 — no divergent double-publish).
        let now = now_unix();
        match self.node.store().deliver_and_enqueue(
            job_id,
            &commit,
            &draft,
            now,
            now + RESULT_PUBLISH_WINDOW_SECS,
            now,
        ) {
            Ok(true) => {
                eprintln!("seller node delivered job_id={job_id} commit={commit} result enqueued")
            }
            Ok(false) => eprintln!(
                "seller node execute job_id={job_id}: delivery already journaled (dedup no-op)"
            ),
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: deliver journal failed ({error})");
                self.fail_job(job_id).await;
                return;
            }
        }
        self.drain().await;
    }

    /// Settle one gift-wrapped payment: decode it (through the signer actor — the NIP-44 decrypt
    /// needs the seller key, which never leaves the actor), authenticate the buyer by the seal,
    /// enforce the money-safety guards (seal sender == bound buyer, realized mint ∈ the STORED
    /// claim-time creq per Fix Q, `allow_real_mints` fence), then — in the invariant-3 order — write
    /// the intent breadcrumb BEFORE swapping at the mint, classify the swap FAIL-CLOSED (never infer
    /// collection from the breadcrumb), and only then record the receipt (deduped by the wrap id, so
    /// a replayed wrap credits the job at most once). Every refusal is logged with a named reason.
    async fn on_gift_wrap(&self, event: &nostr_sdk::Event) {
        let event_id = event.id.to_hex();
        // Log EVERY wrap seen — silence must mean "no wraps", never "lost money".
        eprintln!("seller node wrap seen event={event_id}");

        let received = match self.node.signer().unwrap_payment_wrap(event.clone()).await {
            Ok(Ok(Some(received))) => received,
            Ok(Ok(None)) => {
                eprintln!("seller node wrap event={event_id}: not a decodable own-payment wrap (skipped)");
                return;
            }
            Ok(Err(error)) => {
                eprintln!("seller node wrap event={event_id}: decode failed ({error})");
                return;
            }
            Err(error) => {
                eprintln!("seller node wrap event={event_id}: signer actor gone ({error})");
                return;
            }
        };
        let job_id = received.payload.job_id().to_owned();
        if job_id.is_empty() {
            eprintln!("seller node wrap event={event_id}: payment carries no job id (skipped)");
            return;
        }

        // Already-paid job: a re-see of consumed money — skip (do not re-redeem). Fail closed on a
        // read error (never read an unreadable journal as "not paid ⇒ safe to redeem again").
        match self.node.store().has_receipt(&job_id) {
            Ok(true) => {
                eprintln!("seller node wrap event={event_id}: job {job_id} already receipted, skipping");
                return;
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("seller node wrap event={event_id}: has_receipt read failed for {job_id} (fail-closed, skipping): {error}");
                return;
            }
        }

        // Bind to a job we recorded (offer facts). No offer ⇒ early pay for a still-unknown job or
        // not ours — leave it (buffered by re-delivery), never misattribute.
        let offer = match self.node.store().offer_row(&job_id) {
            Ok(Some(offer)) => offer,
            Ok(None) => {
                eprintln!("seller node wrap event={event_id}: no offer recorded for job {job_id} (skipped)");
                return;
            }
            Err(error) => {
                eprintln!("seller node wrap event={event_id}: offer read failed for {job_id} ({error})");
                return;
            }
        };

        // Seal-sender guard: the authenticated buyer MUST be the bound offer buyer.
        let buyer = received.buyer_pubkey.to_hex();
        if !seal_sender_is_bound_buyer(&buyer, &offer.buyer_pubkey) {
            eprintln!(
                "seller node wrap event={event_id}: payment sender {buyer} is not the bound offer buyer {} for job {job_id} — refused",
                offer.buyer_pubkey
            );
            return;
        }

        // Fix Q — settle against the mints the seller ORIGINALLY advertised (the STORED claim-time
        // creq), never live config: a config change across the trade can neither strand this payment
        // nor let a newly-added mint settle it.
        let stored_creq = match self.node.store().job_creq(&job_id) {
            Ok(Some(creq)) => creq,
            _ => {
                eprintln!("seller node wrap event={event_id}: no stored creq for job {job_id} (skipped)");
                return;
            }
        };
        let request = match crate::gateway::creq::parse_creq(&stored_creq) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("seller node wrap event={event_id}: stored creq unparseable for job {job_id} ({error})");
                return;
            }
        };
        let payload_mint = received.payload.payload.mint.clone();
        let mint_str = payload_mint.to_string();
        // Redeem guard: the realized mint MUST be one the STORED creq advertised.
        if !request.mints.contains(&payload_mint) {
            eprintln!(
                "seller node wrap event={event_id}: realized mint {mint_str} outside the stored creq's accepted mints for job {job_id} — refused"
            );
            return;
        }
        // Real-mint fence: a real mint can never settle unless the operator opted in.
        if !crate::home::mint_allowed(&mint_str, self.node.home().config.allow_real_mints) {
            eprintln!(
                "seller node wrap event={event_id}: mint {mint_str} not allowed (allow_real_mints={}) for job {job_id} — refused",
                self.node.home().config.allow_real_mints
            );
            return;
        }

        // Payment terms over the stored-creq accepted set (amount == offer.amount, unit == sat). The
        // ParsedOffer is reconstructed from the stored offer; a targeted offer we hold was targeted to
        // US, so its seller target is our own pubkey (open-pool = untargeted).
        let seller_pubkey = self.seller_pubkey.to_hex();
        let parsed_offer = ParsedOffer {
            task: offer.task.clone(),
            output: String::new(),
            amount: offer.amount_sats,
            unit: offer.unit.clone(),
            deadline_unix: offer.deadline_unix.max(0) as u64,
            seller_pubkey: offer.targeted.then(|| seller_pubkey.clone()),
        };
        let accepted_mints: std::collections::HashSet<cashu::MintUrl> =
            request.mints.iter().cloned().collect();
        let policy = crate::payment_wallet::PaymentPolicy::new(accepted_mints.iter().cloned());
        let terms = match policy.terms_for_offer(payload_mint.clone(), &parsed_offer, &seller_pubkey) {
            Ok(terms) => terms,
            Err(error) => {
                eprintln!("seller node wrap event={event_id}: payment terms refused for job {job_id} ({error})");
                return;
            }
        };

        // Derive the cashu P2PK key through the actor and open a wallet at the REALIZED mint (the
        // buyer paid seller-locked ecash there; the wallet must be bound to that same mint).
        let cashu_key = match self.node.signer().cashu_p2pk_secret().await {
            Ok(Ok(key)) => key,
            Ok(Err(error)) => {
                eprintln!("seller node wrap event={event_id}: cashu key derive failed for job {job_id} ({error})");
                return;
            }
            Err(error) => {
                eprintln!("seller node wrap event={event_id}: signer actor gone ({error})");
                return;
            }
        };
        let wallet = match crate::buyer_fund::open_wallet_at_mint_async(self.node.home(), &mint_str).await {
            Ok(wallet) => wallet,
            Err(error) => {
                eprintln!("seller node wrap event={event_id}: open wallet at {mint_str} failed for job {job_id} ({error})");
                return;
            }
        };
        let adapter = crate::payment_wallet::CdkSellerReceive::new(&wallet, cashu_key);
        let token = received.payload.to_token();
        let expected = offer.amount_sats;

        // Intent-to-receive breadcrumb BEFORE the swap (invariant 3). token_hash is SHA-256 of the
        // token string — no proof/secret material is stored.
        let token_hash = {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(token.to_string().as_bytes());
            hex::encode(hasher.finalize())
        };
        if let Err(error) =
            self.node
                .store()
                .append_pending_receive(&job_id, &token_hash, &buyer, &mint_str, expected, now_unix())
        {
            eprintln!("seller node wrap event={event_id}: breadcrumb write failed for job {job_id} ({error}) — refusing to receive");
            return;
        }

        // Swap at the mint, then classify FAIL-CLOSED (never infer prior collection from the
        // breadcrumb — the only proof is a COMPLETED receipt read fail-closed).
        let receive_result = adapter
            .receive(&token, &terms, &accepted_mints, &payload_mint)
            .await
            .map(|amount| amount.to_u64())
            .map_err(|error| error.to_string());
        let amount_received = match classify_redeem_outcome(receive_result, || {
            self.node.store().has_receipt(&job_id).map_err(|error| error.to_string())
        }) {
            RedeemDecision::Finalize(amount) => amount,
            RedeemDecision::IdempotentNoOp => {
                eprintln!("seller node wrap event={event_id}: idempotent no-op (already spent AND a completed receipt exists) for job {job_id}");
                return;
            }
            RedeemDecision::Refuse(reason) => {
                eprintln!("seller node wrap event={event_id}: receive refused for job {job_id} ({reason}) — buffered for reconcile");
                return;
            }
        };
        eprintln!(
            "seller node collect ok: job_id={job_id} amount_received={amount_received} expected={expected} mint={mint_str}"
        );

        // Record the receipt AFTER the money landed (invariant 3 order) — deduped on the wrap id, so a
        // replayed wrap marks the job paid at most once.
        match self
            .node
            .store()
            .collect_receipt(&event_id, &job_id, amount_received, now_unix())
        {
            Ok(super::store::Collected::New) => {
                eprintln!("seller node paid job_id={job_id} amount={amount_received} receipt={event_id}")
            }
            Ok(super::store::Collected::Duplicate) => eprintln!(
                "seller node wrap event={event_id}: receipt already collected for job {job_id} (dedup no-op)"
            ),
            Err(error) => {
                eprintln!("seller node wrap event={event_id}: receipt write failed for job {job_id} ({error})")
            }
        }
    }

    /// Mark a job failed (best-effort; a fail-mark that itself errors is logged, never propagated —
    /// the loop keeps serving).
    async fn fail_job(&self, job_id: &str) {
        if let Err(error) = self.node.store().fail_job(job_id, now_unix()) {
            eprintln!("seller node job_id={job_id}: fail_job write error (continuing): {error}");
        }
    }

    /// One outbox drain pass over the shared authenticated client. Log-and-continue: a publish
    /// failure leaves the row pending for the next tick (never wedges the loop).
    async fn drain(&self) {
        let now = now_unix();
        match drain_once(self.node.store(), &self.publisher, now).await {
            Ok(report) if report.confirmed > 0 || report.failed > 0 => eprintln!(
                "seller node outbox drain: confirmed={} failed={} expired={}",
                report.confirmed, report.failed, report.expired
            ),
            Ok(_) => {}
            Err(error) => eprintln!("seller node outbox drain error (continuing): {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELLER: &str = "aa";
    const NOW: u64 = 10_000;

    fn seller_cfg(rate_sats: u64, claim_open_pool: bool) -> crate::home::SellerConfig {
        crate::home::SellerConfig {
            agent_command: vec!["claude".to_owned()],
            rate_sats,
            git_remote: "https://example.invalid/repo.git".to_owned(),
            job_timeout_secs: None,
            agent: Some("claude".to_owned()),
            claim_open_pool,
            offer_backfill_secs: 0,
            contribution_enabled: true,
        }
    }

    fn offer(amount: u64, targeted_to: Option<&str>, deadline_unix: u64) -> ParsedOffer {
        ParsedOffer {
            task: "do the thing".to_owned(),
            output: String::new(),
            amount,
            unit: "sat".to_owned(),
            deadline_unix,
            seller_pubkey: targeted_to.map(str::to_owned),
        }
    }

    // A fresh, in-rate, targeted offer is claimed and carries the resolved deadline.
    #[test]
    fn claims_fresh_targeted_offer_at_rate() {
        let decision = classify_offer(&offer(5, Some(SELLER), NOW + 600), &seller_cfg(2, false), SELLER, NOW);
        assert_eq!(decision, ClaimDecision::Claim { deadline_unix: NOW + 600 });
    }

    // MONEY-SAFETY ORDER: a lapsed offer (deadline already passed) is refused BEFORE the rate gate —
    // it is never resurrected with a fresh window, even though it clears the rate floor.
    #[test]
    fn refuses_lapsed_offer_before_rate() {
        let decision = classify_offer(&offer(100, Some(SELLER), NOW), &seller_cfg(2, false), SELLER, NOW);
        assert!(matches!(decision, ClaimDecision::Skip(reason) if reason.contains("lapsed")));
    }

    // Below the rate floor ⇒ skip (never claim work priced under the seller's floor).
    #[test]
    fn refuses_below_rate() {
        let decision = classify_offer(&offer(1, Some(SELLER), NOW + 600), &seller_cfg(5, false), SELLER, NOW);
        assert!(matches!(decision, ClaimDecision::Skip(_)));
    }

    // TOOTH (invariant 8 / audit N-4) — the delivery cosignature signs the hash of the STORED
    // claim-time creq, never a rebuild from live config. Author a creq under one accepted-mint set
    // (what the buyer read off the claim), then "drift" the config to a different mint set: the
    // stored creq's hash is unchanged, and the drifted config would produce a DIFFERENT hash that
    // delivery must NOT use. Signing the stored value is what keeps buyer/seller cosigs agreeing.
    #[test]
    fn delivery_hash_binds_stored_creq_not_drifted_config() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let mints_claim = vec!["https://testnut.cashudevkit.org".to_owned()];
        let stored_creq =
            gateway::creq::build_seller_creq("job-1", 21, "sat", &mints_claim, &seller).expect("creq");
        let signed_hash = gateway::creq_hash_hex(&stored_creq);

        // Config drifts to a different accepted-mint set after the claim.
        let mints_drifted = vec![
            "https://testnut.cashudevkit.org".to_owned(),
            "https://mint.example.invalid".to_owned(),
        ];
        let drifted_creq =
            gateway::creq::build_seller_creq("job-1", 21, "sat", &mints_drifted, &seller).expect("creq2");

        // The stored creq's hash is stable; the drifted config yields a DIFFERENT hash — which the
        // delivery path must never sign (it reads store.job_creq, not live config).
        assert_eq!(gateway::creq_hash_hex(&stored_creq), signed_hash);
        assert_ne!(
            gateway::creq_hash_hex(&drifted_creq),
            signed_hash,
            "a config-drifted creq hashes differently; delivery must sign the STORED creq's hash"
        );
    }

    // AWARD AUTHORIZATION (security-critical): only the offer's buyer may drive execute or release.
    #[test]
    fn award_from_non_buyer_is_ignored_even_when_claim_matches() {
        // Author != buyer ⇒ Ignore, regardless of a matching claim id — a third party can neither
        // execute nor release our claim.
        assert_eq!(
            match_award("claim1", Some("claim1"), "attacker", "buyer"),
            AwardMatch::Ignore
        );
    }

    #[test]
    fn award_binds_our_claim_and_releases_on_a_different_one() {
        // Buyer awards OUR published claim ⇒ Execute.
        assert_eq!(match_award("claim1", Some("claim1"), "buyer", "buyer"), AwardMatch::Execute);
        // Buyer awards a DIFFERENT claim ⇒ Release ours (another seller won).
        assert_eq!(match_award("claim2", Some("claim1"), "buyer", "buyer"), AwardMatch::Release);
        // Our claim not yet on the wire ⇒ Ignore (never act on an unpublished claim).
        assert_eq!(match_award("claim1", None, "buyer", "buyer"), AwardMatch::Ignore);
    }

    // Untargeted offers are refused unless open-pool is opted in; with it, they claim.
    #[test]
    fn untargeted_needs_open_pool_opt_in() {
        assert!(matches!(
            classify_offer(&offer(5, None, NOW + 600), &seller_cfg(2, false), SELLER, NOW),
            ClaimDecision::Skip(_)
        ));
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &seller_cfg(2, true), SELLER, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 }
        );
    }

    // ── Execute-body delivery contract (invariants 2 & 8), no network ────────────────────────────

    use crate::seller_node::store::{Offer, SellerStore};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-run-{label}-{}-{id}", std::process::id()))
    }

    // A store with a full offer → claim(creq) → award already journaled, so the execute-body readers
    // (offer_row / job_creq / job_award_time) have real rows to answer from.
    fn store_with_awarded_job(
        creq: &str,
        job: &str,
        buyer: &str,
        award_time: i64,
    ) -> (SellerStore, std::path::PathBuf) {
        let root = temp_dir("store");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mk root");
        let store = SellerStore::open(root.join("seller.sqlite")).expect("open store");
        store
            .record_offer(
                &Offer {
                    offer_id: job.to_owned(),
                    buyer_pubkey: buyer.to_owned(),
                    amount_sats: 21,
                    unit: "sat".to_owned(),
                    task: "build a widget".to_owned(),
                    deadline_unix: 2_000_000_000,
                    targeted: true,
                },
                1,
            )
            .expect("record offer");
        let draft = claim_draft(job, buyer, &"s".repeat(64), creq);
        store
            .claim_and_enqueue(job, job, creq, &draft, 1, 9_999_999_999, 1)
            .expect("claim");
        store
            .record_award(&"w".repeat(64), job, buyer, award_time)
            .expect("award");
        (store, root)
    }

    // TOOTH (invariant 8 / audit N-4), NODE-level: the delivery cosignature the execute body signs
    // binds the hash of the STORED claim-time creq read from the store — never a rebuild from live
    // config. Author a creq under one accepted-mint set, journal it, then build the preimage the exec
    // body builds (from `store.job_creq`): its creq_hash equals the STORED creq's hash and differs
    // from the hash a drifted mint set would produce. Bite: if the exec body sourced the creq from
    // live config instead of the store, the bound hash would be the drifted one and this goes red.
    #[test]
    fn delivery_preimage_binds_stored_creq_not_drifted_config() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let buyer = "b".repeat(64);
        let job = "a".repeat(64);
        let mints_claim = vec!["https://testnut.cashudevkit.org".to_owned()];
        let creq_a =
            gateway::creq::build_seller_creq(&job, 21, "sat", &mints_claim, &seller).expect("creq A");

        let (store, root) = store_with_awarded_job(&creq_a, &job, &buyer, 4242);
        let stored = store.job_creq(&job).expect("read").expect("present");
        assert_eq!(stored, creq_a, "the stored creq is the claim-time creq");

        let preimage = delivery_receipt_preimage(
            &job,
            "build a widget",
            21,
            &buyer,
            &seller,
            &"c".repeat(40),
            "fork",
            &stored,
        );
        assert_eq!(
            preimage.creq_hash,
            Some(gateway::creq_hash_hex(&creq_a)),
            "delivery signs the STORED creq's hash"
        );

        // Config drifts to a different accepted-mint set after the claim: its creq hashes differently
        // and the delivery must NOT bind it.
        let mints_drifted = vec![
            "https://testnut.cashudevkit.org".to_owned(),
            "https://mint.example.invalid".to_owned(),
        ];
        let creq_b =
            gateway::creq::build_seller_creq(&job, 21, "sat", &mints_drifted, &seller).expect("creq B");
        assert_ne!(
            preimage.creq_hash,
            Some(gateway::creq_hash_hex(&creq_b)),
            "a config-drifted creq hashes differently; the delivery must not sign it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (invariant 2), NODE-level: a re-created delivery commit is deterministic (identical tree
    // + the STORED award-time author date ⇒ identical oid), and the durable delivery journal adopts
    // the existing tip instead of double-publishing. Bite: if the snapshot used wall-clock now()
    // instead of the journaled date, the two commits differ and the equality assert goes red; if
    // `deliver_and_enqueue` did not dedup, the second call returns true and a SECOND result outbox
    // row appears — the count assert goes red.
    #[test]
    fn resume_redelivery_is_deterministic_and_never_double_publishes() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let buyer = "b".repeat(64);
        let job = "a".repeat(64);
        let author_date = 4242_i64;
        let branch = "mobee/aaaaaaaa";
        let identity = DeliveryAgentIdentity::for_seller(&seller);

        // Two independent workdirs with byte-identical trees, each snapshotted at the SAME journaled
        // author date — the exact "crashed, re-created the commit on resume" shape.
        let make_commit = |label: &str| -> String {
            let wd = temp_dir(label);
            let _ = std::fs::remove_dir_all(&wd);
            seller_git::init_empty_delivery_workdir(&wd, &identity).expect("init workdir");
            std::fs::write(wd.join("deliverable.txt"), b"the widget\n").expect("write file");
            let commit =
                seller_git::snapshot_delivery_at(&wd, &identity, None, branch, "mobee delivery: build a widget", author_date)
                    .expect("snapshot");
            let _ = std::fs::remove_dir_all(&wd);
            commit
        };
        let commit_first = make_commit("wd1");
        let commit_resume = make_commit("wd2");
        assert_eq!(
            commit_first, commit_resume,
            "identical tree + stored author date ⇒ identical delivery oid (deterministic re-push)"
        );

        // The durable delivery journal: first delivery lands, a resumed re-delivery is a dedup no-op.
        let creq = gateway::creq::build_seller_creq(
            &job,
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, author_date);
        let draft = claim_draft(&job, &buyer, &seller, &creq);
        let now = 5000;
        assert!(
            store
                .deliver_and_enqueue(&job, &commit_first, &draft, now, now + RESULT_PUBLISH_WINDOW_SECS, now)
                .expect("deliver"),
            "first delivery journals + enqueues the result"
        );
        assert!(
            !store
                .deliver_and_enqueue(&job, &commit_resume, &draft, now, now + RESULT_PUBLISH_WINDOW_SECS, now)
                .expect("re-deliver"),
            "resume adopts the existing tip: a second delivery re-enqueues nothing"
        );
        let result_rows = store
            .pending_outbox(now)
            .expect("pending")
            .into_iter()
            .filter(|item| item.dedup_key == format!("result:{job}"))
            .count();
        assert_eq!(result_rows, 1, "exactly one result event enqueued across the resume");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Pay arm money-safety (invariant 3), no mint ─────────────────────────────────────────────

    // TOOTH (invariant 3 / finding S) — the redeem classification never forges a receipt from a
    // pending-receive breadcrumb; the ONLY positive proof of our prior collection is a COMPLETED
    // receipt, read fail-closed. Covers the replay-of-collected case (already-spent + has_receipt=true
    // ⇒ no-op) and the crash-between-import-and-receipt-row case (already-spent + has_receipt=false ⇒
    // refuse, never a forged receipt).
    #[test]
    fn redeem_classification_finalizes_and_never_forges_from_a_breadcrumb() {
        // A clean successful receive finalizes exactly its amount; has_receipt is never consulted.
        assert!(matches!(
            classify_redeem_outcome(Ok(21), || panic!("has_receipt must not be read on success")),
            RedeemDecision::Finalize(21)
        ));
        // Already-spent + a COMPLETED receipt ⇒ idempotent no-op (legit backfill/restart re-see).
        assert!(matches!(
            classify_redeem_outcome(Err("Token already spent".into()), || Ok(true)),
            RedeemDecision::IdempotentNoOp
        ));
        // Already-spent + NO receipt (crash-between, or a replay/theft — indistinguishable) ⇒ refuse.
        assert!(matches!(
            classify_redeem_outcome(Err("Token already spent".into()), || Ok(false)),
            RedeemDecision::Refuse(_)
        ));
        // has_receipt READ ERROR ⇒ refuse, FAIL CLOSED (never read unreadable as "no receipt ⇒ safe").
        assert!(matches!(
            classify_redeem_outcome(Err("already redeemed".into()), || Err("corrupt".into())),
            RedeemDecision::Refuse(_)
        ));
        // A non-already-spent receive error refuses without consulting has_receipt.
        assert!(matches!(
            classify_redeem_outcome(Err("mint offline".into()), || panic!("must not read has_receipt")),
            RedeemDecision::Refuse(_)
        ));
    }

    // TOOTH (#150 relay-stall watchdog) — the stall threshold is interval*missed with each factor
    // clamped ≥1 (never 0, so the watchdog can never trip on the first tick), and staleness trips
    // only AT/after the threshold.
    #[test]
    fn watchdog_stall_math_clamps_and_trips_only_at_threshold() {
        assert_eq!(stall_threshold_secs(300, 3), 900);
        assert_eq!(stall_threshold_secs(0, 0), 1, "each factor clamped ≥1 so the product is never 0");
        assert!(!subscription_stalled(899, 900), "below threshold ⇒ live");
        assert!(subscription_stalled(900, 900), "at threshold ⇒ stalled");
        assert!(subscription_stalled(901, 900));
    }

    // TOOTH (invariant 3, security) — a payment settles a job ONLY when the authenticated seal sender
    // is the bound offer buyer; a third party can never pay-once and close someone else's job.
    #[test]
    fn seal_sender_must_be_the_bound_offer_buyer() {
        assert!(seal_sender_is_bound_buyer("buyerpk", "buyerpk"));
        assert!(!seal_sender_is_bound_buyer("attackerpk", "buyerpk"));
    }

    // TOOTH (invariant 3 / Fix Q) — the redeem guard settles only at a mint the STORED claim-time creq
    // advertised; a realized mint outside that set is refused, so a config change across the trade can
    // neither strand this payment nor introduce a settling mint.
    #[test]
    fn realized_mint_must_be_in_the_stored_creq() {
        use std::str::FromStr as _;
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let advertised = "https://testnut.cashudevkit.org";
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &[advertised.to_owned()],
            &seller,
        )
        .expect("creq");
        let request = gateway::creq::parse_creq(&creq).expect("parse");
        let advertised_mint = cashu::MintUrl::from_str(advertised).expect("mint");
        let foreign_mint = cashu::MintUrl::from_str("https://mint.example.invalid").expect("mint");
        assert!(request.mints.contains(&advertised_mint), "the advertised mint settles");
        assert!(
            !request.mints.contains(&foreign_mint),
            "a mint outside the stored creq is refused"
        );
    }

    // TOOTH (invariant 3) — the receipt row dedups a replayed payment on the wrap id: a job is marked
    // paid at most once, so a re-delivered gift-wrap never double-credits.
    #[test]
    fn receipt_collect_dedups_a_replayed_payment() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let job = "a".repeat(64);
        let buyer = "b".repeat(64);
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, 4242);
        let wrap_id = "e".repeat(64);
        assert!(!store.has_receipt(&job).expect("read"), "not paid before collect");
        assert_eq!(
            store.collect_receipt(&wrap_id, &job, 21, 5000).expect("collect"),
            crate::seller_node::store::Collected::New
        );
        assert_eq!(
            store.collect_receipt(&wrap_id, &job, 21, 5001).expect("replay"),
            crate::seller_node::store::Collected::Duplicate,
            "a replayed wrap id never credits the job twice"
        );
        assert!(store.has_receipt(&job).expect("read"), "paid after the first collect");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Resume execution across a process restart (invariant 4, fallback form) ───────────────────

    // TOOTH (invariant 4) — the resume selection re-drives only jobs left mid-flight (awarded /
    // executing); a delivered job is left for the pay path and terminal jobs never re-run.
    #[test]
    fn resume_selects_awarded_or_executing_not_delivered_or_terminal() {
        use crate::seller_node::store::JobState;
        assert!(should_resume_execution(JobState::Awarded));
        assert!(should_resume_execution(JobState::Executing));
        assert!(!should_resume_execution(JobState::Delivered));
        assert!(!should_resume_execution(JobState::Paid));
        assert!(!should_resume_execution(JobState::Failed));
    }

    // TOOTH (invariant 4) — boot with a journaled awarded-but-undelivered job: it is resume-eligible
    // (the field-test promise — nous's Mac kills processes mid-job), and the re-execution's delivery
    // lands EXACTLY ONCE (deliver_and_enqueue is idempotent on the job), so a resumed job never
    // double-publishes.
    #[test]
    fn boot_resume_re_drives_awarded_undelivered_job_delivery_lands_once() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let creq = gateway::creq::build_seller_creq(
            &"a".repeat(64),
            21,
            "sat",
            &["https://testnut.cashudevkit.org".to_owned()],
            &seller,
        )
        .expect("creq");
        let job = "a".repeat(64);
        let buyer = "b".repeat(64);
        let (store, root) = store_with_awarded_job(&creq, &job, &buyer, 4242);

        // Awarded + undelivered ⇒ the boot resume pass selects it.
        let resumable = store.resumable_jobs().expect("resumable");
        assert!(
            resumable
                .iter()
                .any(|(id, state)| id == &job && should_resume_execution(*state)),
            "the awarded, undelivered job is resume-eligible: {resumable:?}"
        );
        assert_eq!(
            store.job_state(&job).expect("state"),
            Some(crate::seller_node::store::JobState::Awarded)
        );

        // Re-execution delivers exactly once: deliver_and_enqueue is idempotent on the job.
        let draft = claim_draft(&job, &buyer, &seller, &creq);
        let now = 5000;
        assert!(
            store
                .deliver_and_enqueue(&job, &"c".repeat(40), &draft, now, now + RESULT_PUBLISH_WINDOW_SECS, now)
                .expect("deliver"),
            "first (resumed) delivery lands"
        );
        assert!(
            !store
                .deliver_and_enqueue(&job, &"c".repeat(40), &draft, now, now + RESULT_PUBLISH_WINDOW_SECS, now)
                .expect("re-deliver"),
            "a resumed re-execution delivers at most once"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
