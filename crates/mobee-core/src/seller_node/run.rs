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

use crate::gateway::{
    self, claim_draft, error_draft, git_result_draft, parse_award, parse_offer, OfferParseError,
    ParsedOffer,
};
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
/// Buyer-facing reason on any post-award execution failure: generic (never leaks internal paths or
/// error detail — the operator log carries the specifics) but enough that the buyer learns the job
/// failed instead of waiting on a delivery that will never come.
const EXEC_FAILURE_FEEDBACK: &str = "seller could not complete the job (execution failed before delivery)";

/// The pure claim/skip decision over a parsed offer — no I/O, so the money-safety ordering
/// (targeting, deadline-expiry, rate floor) is unit-testable. Mirrors the legacy `classify_offer`
/// gates that do not need durable state; the store-backed dedup + capacity checks ride on top in
/// [`SellerNodeRunner::on_offer`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaimDecision {
    /// Claim it — carries the job deadline resolved for execution.
    Claim { deadline_unix: u64 },
    /// Skip it, with a named reason (never a silent drop).
    Skip(SkipReason),
}

/// Why an offer was skipped. A typed reason (not a bare string) so the caller can act on the
/// *kind* of refusal — specifically, only a rate-gate refusal (never a lapsed offer) is eligible for
/// the targeted under-rate buyer feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// The offer's own absolute deadline already passed — dead, never resurrected.
    Lapsed,
    /// Rate-gate refused: untargeted without open-pool opt-in, or below the seller's rate floor.
    RateGate,
}

impl SkipReason {
    /// The machine-readable log/feedback reason (same string the legacy path logged).
    fn reason(self) -> &'static str {
        match self {
            Self::Lapsed => "offer deadline already passed (lapsed; never resurrected)",
            Self::RateGate => "rate-gate refused (untargeted without opt-in / below rate)",
        }
    }
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
/// Base backoff between the bounded recovery attempts (#162), doubled per attempt by
/// [`recovery_backoff`].
const RECOVERY_BACKOFF: Duration = Duration::from_secs(2);
/// Ceiling on the per-attempt backoff, so one bounded recovery still fits inside a single heartbeat
/// interval and the watchdog stays on cadence.
const RECOVERY_BACKOFF_MAX: Duration = Duration::from_secs(8);

/// Backoff to wait after a failed recovery `attempt` before the next one: exponential from
/// [`RECOVERY_BACKOFF`], capped at [`RECOVERY_BACKOFF_MAX`].
///
/// A flat retry interval re-dials the relay as fast as the socket can be torn down — with #171 in
/// the field that was every wedged node re-dialing shared infrastructure three times a minute,
/// indefinitely. Backing off spaces the attempts; capping them keeps a whole recovery bounded.
fn recovery_backoff(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.saturating_sub(1).min(16);
    RECOVERY_BACKOFF
        .saturating_mul(factor)
        .min(RECOVERY_BACKOFF_MAX)
}

/// Cadence of the periodic payment-wrap backfill.
///
/// A live kind-1059 subscription is not sufficient on its own. Field-observed on the in-memory
/// daemon this node replaces: a fresh subscription delivers a wrap within ~1 min, but a subscription
/// ~10+ minutes old was seen to go deaf and never deliver again — and a payment then sat unredeemed
/// until the process was manually restarted, because the restart re-ran the boot backfill. Re-asking
/// the relay for stored wraps on a timer is what makes that recover WITHOUT a restart.
///
/// Note this is a failure the liveness probe cannot see: the session still answers our REQs, so the
/// relay is "alive" by every measure the watchdog has. The three layers are deliberately distinct —
/// probe = session liveness, this backfill = money-leg recovery, and a subscription-map reconciler
/// (#172) = registration integrity. None of them subsumes another.
const WRAP_BACKFILL_INTERVAL_SECS: u64 = 300;
/// Skew margin subtracted from the oldest delivered-but-unpaid job when clamping the backfill cursor.
const WRAP_BACKFILL_MARGIN_SECS: i64 = 3600;
/// Test-only override of [`WRAP_BACKFILL_INTERVAL_SECS`]. NOT a user config knob; no production path
/// sets it (mirrors the heartbeat cadence seam).
const WRAP_BACKFILL_INTERVAL_ENV: &str = "MOBEE_WRAP_BACKFILL_INTERVAL_SECS";
/// Hard cap on one backfill fetch, so an auth-gated relay that never EOSEs cannot wedge the tick.
const WRAP_BACKFILL_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Effective backfill cadence: the env test seam wins over [`WRAP_BACKFILL_INTERVAL_SECS`]; a `0` or
/// unparseable value is ignored.
fn resolve_wrap_backfill_interval_secs() -> u64 {
    match std::env::var(WRAP_BACKFILL_INTERVAL_ENV) {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|secs| *secs > 0)
            .unwrap_or(WRAP_BACKFILL_INTERVAL_SECS),
        Err(_) => WRAP_BACKFILL_INTERVAL_SECS,
    }
}

/// The `since` cursor for a wrap backfill: the last collected receipt, but never later than the
/// oldest delivered-but-unpaid job (minus a skew margin).
///
/// The last-receipt timestamp alone is wrong: a receipt for a NEWER job would advance the cursor past
/// an OLDER unsettled job and skip its still-uncollected payment wrap forever. Clamping keeps that
/// wrap in range; the per-job idempotency guards (`has_receipt` skip, mint already-spent refuse) make
/// the wider re-scan safe.
///
/// A journal/store READ ERROR must abort the cycle, never fall back to `since = 0` — that would turn a
/// transient read failure into a full-history backfill. Absent data (nothing collected, nothing
/// unsettled) is legitimately `0`; a failure to read is not.
fn resolve_backfill_since(
    last_receipt: Result<Option<i64>, super::store::StoreError>,
    oldest_unsettled: Result<Option<i64>, super::store::StoreError>,
) -> Result<u64, super::store::StoreError> {
    let last_receipt = last_receipt?.unwrap_or(0);
    let cursor = match oldest_unsettled? {
        Some(oldest) => last_receipt.min(oldest.saturating_sub(WRAP_BACKFILL_MARGIN_SECS)),
        None => last_receipt,
    };
    Ok(cursor.max(0) as u64)
}

/// Upper bound on stored open-pool offers a backfilling REQ may return.
const OFFER_BACKFILL_LIMIT: usize = 500;

/// Stable per-role subscription ids. Named rather than generated so a relay `CLOSED` says WHICH
/// subscription died — with anonymous ids a closed subscription is indistinguishable in the log,
/// which is how a node could go silently deaf on one leg while heartbeating happily on another.
const OFFER_SUB_ID: &str = "mobee-offers";
const AWARD_SUB_ID: &str = "mobee-awards";
const WRAP_SUB_ID: &str = "mobee-wraps";
/// The liveness probe's subscription (see [`probe_relay_serves_our_reqs`]).
const LIVENESS_PROBE_SUB_ID: &str = "mobee-liveness-probe";

/// How long the liveness probe waits for its `EOSE`. A `limit(0)` REQ is answered in milliseconds by
/// a healthy relay, so this is generous — it bounds the tick, and a single slow answer is not a stall
/// on its own (it takes `stall_missed_intervals` consecutive failures to trip the watchdog).
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The human label for one of our subscription ids, for logging a relay `CLOSED`.
fn subscription_label(id: &str) -> &'static str {
    match id {
        OFFER_SUB_ID => "offers",
        AWARD_SUB_ID => "awards",
        WRAP_SUB_ID => "payment gift-wraps (kind-1059)",
        LIVENESS_PROBE_SUB_ID => "liveness probe",
        _ => "unknown (not one of ours)",
    }
}

/// Ask the relay to serve one trivial REQ on the CURRENT session and wait for its `EOSE`. True means
/// the relay is answering OUR subscriptions on THIS authenticated connection — the exact property the
/// #150 watchdog needs, and the one thing a heartbeat cannot demonstrate.
///
/// WHY NOT the own-heartbeat round-trip this replaced: a client cannot observe its own published
/// event coming back. `RelayPool::send_event_to` saves every event it publishes into the client's own
/// database (`pool/mod.rs:767`); when the relay echoes it, the inbound handler sees
/// `DatabaseEventStatus::Saved` and returns without emitting a notification (`relay/inner.rs:1215`,
/// notification only in the `NotExistent` arm). So the old probe could never succeed — the watchdog
/// declared a stall every `stall_threshold` on every node, healthy or not, and then drove a recovery
/// that could not succeed either (#171). A `limit(0)` REQ needs no cooperating publisher and no
/// stored events: the `EOSE` alone carries the proof.
async fn probe_relay_serves_our_reqs(
    client: &Client,
    seller_pubkey: nostr_sdk::PublicKey,
    timeout: Duration,
) -> bool {
    // Receiver BEFORE the REQ — an EOSE that lands first would otherwise be missed.
    let mut notifications = client.notifications();
    let probe_id = nostr_sdk::SubscriptionId::new(LIVENESS_PROBE_SUB_ID);
    // `limit(0)` asks for zero stored events, so the relay's only work is the EOSE. Scoped to our own
    // heartbeat address so the filter is narrow and unambiguous even if it ever did match.
    let probe = Filter::new()
        .kind(Kind::Custom(crate::heartbeat::SELLER_HEARTBEAT_KIND))
        .author(seller_pubkey)
        .identifier(crate::heartbeat::SELLER_HEARTBEAT_D)
        .limit(0);
    if let Err(error) = client.subscribe_with_id(probe_id, probe, None).await {
        eprintln!("seller node liveness probe: REQ could not be sent ({error})");
        return false;
    }
    tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayPoolNotification::Message {
                    message: nostr_sdk::RelayMessage::EndOfStoredEvents(id),
                    ..
                }) if id.to_string() == LIVENESS_PROBE_SUB_ID => return true,
                Ok(_) => continue,
                // The stream ending is itself a loss of liveness.
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// The seller's LIVE offer filters: the TARGETED (`#p == self`) filter always, plus — under
/// `open_pool` — the un-pinned open-pool filter. BOTH carry the `#t=mobee` namespace guard, so a
/// foreign event squatting the offer kind is never even delivered.
///
/// The two ride ONE subscription (a single REQ, OR-matched per NIP-01). Registered as a separate
/// second subscription the un-pinned filter delivers stored events but never LIVE offers, so a
/// running open-pool seller would ignore fresh untargeted offers — grouping them is load-bearing,
/// not tidiness.
///
/// `since` bounds: on a post-stall resubscribe BOTH filters carry the overlap cursor (only the stall
/// gap is missing). At boot the targeted filter is unbounded — stored offers addressed to this
/// seller are always wanted — while the open-pool filter is bounded by `offer_backfill_secs`: `0` is
/// live-only (`since(now)` + `limit(0)`), otherwise `since(now - window)` capped at
/// [`OFFER_BACKFILL_LIMIT`]. The classify-level deadline-expiry refusal is the staleness guard on
/// both paths, so a backfilled offer is never claimed just because it was returned.
fn offer_subscription_filters(
    seller_pubkey: nostr_sdk::PublicKey,
    open_pool: bool,
    offer_backfill_secs: u64,
    since: Option<nostr_sdk::Timestamp>,
    now: nostr_sdk::Timestamp,
) -> Vec<Filter> {
    let targeted = Filter::new()
        .kind(Kind::Custom(JOB_OFFER_KIND))
        .hashtag(crate::gateway::MOBEE_TAG)
        .pubkey(seller_pubkey);
    let mut filters = vec![match since {
        Some(cursor) => targeted.since(cursor),
        None => targeted,
    }];
    if open_pool {
        let untargeted = Filter::new()
            .kind(Kind::Custom(JOB_OFFER_KIND))
            .hashtag(crate::gateway::MOBEE_TAG);
        filters.push(match since {
            Some(cursor) => untargeted.since(cursor).limit(OFFER_BACKFILL_LIMIT),
            None if offer_backfill_secs > 0 => untargeted
                .since(nostr_sdk::Timestamp::from(
                    now.as_secs().saturating_sub(offer_backfill_secs),
                ))
                .limit(OFFER_BACKFILL_LIMIT),
            None => untargeted.since(now).limit(0),
        });
    }
    filters
}

/// Drop the live socket and bring a fresh authenticated one up, returning once NIP-42 has completed
/// on the NEW connection.
///
/// ORDER IS LOAD-BEARING, and it is the whole of #171: `Relay::disconnect` emits
/// `RelayNotification::Shutdown` on the relay's own notification channel. A receiver taken BEFORE
/// the disconnect inherits that Shutdown, and [`relay_auth::wait_for_nip42_auth`] reads it as the
/// fatal "relay shutdown before NIP-42 authentication" — on a socket that in fact authenticated
/// fine. Recovery then failed 100% of the time (0 successes in 969 field attempts) while the node
/// kept heartbeating with dead subscriptions, because that Shutdown is relay-internal and never
/// reaches the pool notifications the run loop watches.
///
/// A `broadcast::Receiver` only observes sends made after it subscribes, so taking it AFTER the
/// disconnect cannot inherit our own teardown — while still taking it BEFORE `connect`, so the
/// one-shot `Authenticated` notification cannot be missed either. Both halves are required; this is
/// a free function so a test can drive exactly this sequence.
async fn reconnect_and_authenticate(
    client: &Client,
    relay: &nostr_sdk::prelude::Relay,
) -> Result<AuthWait, crate::relay_auth::RelayAuthError> {
    client.disconnect().await;
    let mut relay_notifications = relay.notifications();
    client.connect().await;
    client.wait_for_connection(CONNECT_WAIT).await;
    relay_auth::wait_for_nip42_auth(&mut relay_notifications, CONNECT_WAIT).await
}

/// The refusal reason `on_offer` logs when an offer fails to parse.
///
/// A cross-version offer is a DISTINCT refusal from a malformed one (#146 / #117 refusal taxonomy):
/// it is well-formed under another protocol version, not broken tags, and an operator triaging a
/// quiet seller has to be able to tell those apart. Routing every parse failure through this one
/// function is what makes the taxonomy testable — collapsing the version arm back into the generic
/// bucket changes what this returns, so the tooth goes red instead of quietly passing.
fn offer_parse_refusal(error: &OfferParseError) -> String {
    match error {
        OfferParseError::UnsupportedVersion(version) => {
            format!("unsupported mobee protocol version {version:?}")
        }
        other => format!("unparseable ({other})"),
    }
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

/// Whether a skipped offer earns a buyer-facing under-rate refusal (a feedback-kind `status=error`):
/// ONLY a rate-gate refusal (never a lapsed offer) that is targeted to THIS seller and priced below
/// its floor. Open-pool under-rate stays log-only (spam guard). Pure so the gate is unit-testable.
fn should_publish_under_rate_feedback(
    skip: SkipReason,
    targeted_to_self: bool,
    amount: u64,
    rate_sats: u64,
) -> bool {
    skip == SkipReason::RateGate && targeted_to_self && amount < rate_sats
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
        return ClaimDecision::Skip(SkipReason::Lapsed);
    }
    if rate_gate_allows(offer, seller_pubkey, seller.rate_sats, seller.claim_open_pool).is_err() {
        return ClaimDecision::Skip(SkipReason::RateGate);
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

    /// Subscribe (or re-subscribe) the offer REQ. `open_pool` false forces the targeted-only shape —
    /// used by the boot/recovery path when the seller has not opted into the open pool, and by the
    /// `CLOSED` degrade, which keeps targeted claiming alive after the relay refuses the grouped REQ.
    async fn subscribe_offers(
        &self,
        since: Option<nostr_sdk::Timestamp>,
        open_pool: bool,
    ) -> Result<(), NodeError> {
        let filters = offer_subscription_filters(
            self.seller_pubkey,
            open_pool,
            self.node
                .home()
                .config
                .seller
                .as_ref()
                .map(|seller| seller.offer_backfill_secs)
                .unwrap_or(0),
            since,
            nostr_sdk::Timestamp::from(now_unix().max(0) as u64),
        );
        self.client
            .pool()
            .subscribe_with_id(
                nostr_sdk::SubscriptionId::new(OFFER_SUB_ID),
                filters,
                nostr_sdk::pool::SubscribeOptions::default(),
            )
            .await
            .map_err(|error| NodeError::Relay(format!("subscribe offers: {error}")))?;
        Ok(())
    }

    /// Subscribe the marketplace filters: offers, awards, and payment gift-wraps. `since` is
    /// `Some(overlap)` on a post-stall resubscribe so events published during the stall backfill;
    /// `None` at boot. Reused by boot and by the watchdog's reconnect so both paths subscribe the SAME
    /// set — including re-arming the open-pool half after a `CLOSED` degrade.
    ///
    /// There is no own-heartbeat subscription: a client cannot be delivered its own published event
    /// (see [`probe_relay_serves_our_reqs`]), so that REQ could only ever have returned nothing.
    /// Liveness is asserted by the probe instead.
    async fn subscribe_all(&self, since: Option<nostr_sdk::Timestamp>) -> Result<(), NodeError> {
        let apply = |filter: Filter| match since {
            Some(cursor) => filter.since(cursor),
            None => filter,
        };
        self.subscribe_offers(since, self.claim_open_pool()).await?;
        let award_filter = apply(
            Filter::new()
                .kind(Kind::Custom(JOB_AWARD_KIND))
                .hashtag(crate::gateway::MOBEE_TAG)
                .pubkey(self.seller_pubkey),
        );
        let wrap_filter = apply(Filter::new().kind(Kind::GiftWrap).pubkey(self.seller_pubkey));
        for (id, filter) in [(AWARD_SUB_ID, award_filter), (WRAP_SUB_ID, wrap_filter)] {
            self.client
                .subscribe_with_id(nostr_sdk::SubscriptionId::new(id), filter, None)
                .await
                .map_err(|error| NodeError::Relay(format!("subscribe {id}: {error}")))?;
        }
        Ok(())
    }

    /// Whether this seller has opted into claiming untargeted (open-pool) offers.
    fn claim_open_pool(&self) -> bool {
        self.node
            .home()
            .config
            .seller
            .as_ref()
            .is_some_and(|seller| seller.claim_open_pool)
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
        self.subscribe_all(None).await?;
        eprintln!(
            "seller node live: pubkey={} relay={}",
            self.seller_pubkey.to_hex(),
            self.relay_url
        );
        if heartbeat_enabled {
            eprintln!(
                "seller node heartbeat+watchdog enabled: kind-30340 every {heartbeat_interval_secs}s; \
                 reconnect if the relay stops serving our REQs for {stall_threshold}s \
                 ({stall_missed_intervals} missed intervals)"
            );
        }
        eprintln!(
            "seller node wrap backfill enabled: re-fetching stored kind-1059(s) every {}s (recovers a \
             silently-deaf payment subscription without a restart; its log line is the periodic \
             liveness signal)",
            resolve_wrap_backfill_interval_secs()
        );

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
        let wrap_backfill_interval_secs = resolve_wrap_backfill_interval_secs();
        let mut wrap_backfill_tick =
            tokio::time::interval(Duration::from_secs(wrap_backfill_interval_secs));
        let mut heartbeat_tick =
            tokio::time::interval(Duration::from_secs(heartbeat_interval_secs.max(1)));
        // Watchdog liveness clocks: monotonic instant (staleness measure, robust to wall-clock jumps)
        // + unix stamp (resubscribe `since` cursor). Refreshed whenever the relay answers our liveness
        // probe. Seeded to "now" so a healthy node never trips before its first probe.
        let mut last_liveness_seen = tokio::time::Instant::now();
        let mut last_liveness_seen_unix = now_unix();
        // Whether the offer REQ is currently running in its degraded targeted-only shape after a
        // relay `CLOSED` (re-armed by the next successful recovery).
        let mut open_pool_degraded = false;
        // A repair the CLOSED arm has asked for, run on the next heartbeat tick through the ONE
        // paced recovery path rather than an off-cadence ad-hoc resubscribe.
        let mut forced_recovery: Option<String> = None;
        // Which path actually restored the receive leg. Manual recovery and the SDK's background
        // reconnect were previously indistinguishable in the log — which is how a manual path that
        // never once succeeded went unnoticed (#171). The next answered probe names it.
        let mut stalled_since_recovery = false;
        let mut manual_recovery_succeeded = false;
        loop {
            tokio::select! {
                _ = drain_tick.tick() => {
                    self.drain().await;
                    continue;
                }
                // Re-ask the relay for stored payment wraps, so a silently-deaf 1059 subscription
                // recovers without a restart. Also the node's only periodic log line, and therefore
                // the positive signal external supervision watches.
                _ = wrap_backfill_tick.tick() => {
                    self.run_wrap_backfill().await;
                    continue;
                }
                // The heartbeat tick rides the SAME loop (never a blocking side-thread). Probe first,
                // then evaluate staleness: the probe is what proves the relay is still serving our
                // REQs on this session, and it is bounded so the tick cannot hang on a dead link.
                _ = heartbeat_tick.tick(), if heartbeat_enabled => {
                    if probe_relay_serves_our_reqs(
                        &self.client,
                        self.seller_pubkey,
                        LIVENESS_PROBE_TIMEOUT,
                    )
                    .await
                    {
                        if stalled_since_recovery {
                            if manual_recovery_succeeded {
                                eprintln!(
                                    "seller node subscription RESTORED via MANUAL recovery (relay \
                                     is serving our REQs again)"
                                );
                            } else {
                                eprintln!(
                                    "seller node subscription RESTORED via SDK BACKGROUND reconnect \
                                     — no manual recovery had succeeded (relay is serving our REQs \
                                     again)"
                                );
                            }
                            stalled_since_recovery = false;
                            manual_recovery_succeeded = false;
                        }
                        last_liveness_seen = tokio::time::Instant::now();
                        last_liveness_seen_unix = now_unix();
                    }
                    let stall_elapsed = last_liveness_seen.elapsed().as_secs();
                    let stalled = subscription_stalled(stall_elapsed, stall_threshold);
                    let forced = forced_recovery.take();
                    if stalled || forced.is_some() {
                        let overlap_since = nostr_sdk::Timestamp::from(
                            last_liveness_seen_unix
                                .saturating_sub(STALL_OVERLAP_MARGIN_SECS as i64)
                                .max(0) as u64,
                        );
                        if stalled {
                            eprintln!(
                                "seller node RELAY-STALL detected: relay has not served our REQs in \
                                 {stall_elapsed}s (threshold {stall_threshold}s); reconnecting + \
                                 resubscribing with since={} overlap",
                                overlap_since.as_secs()
                            );
                        } else {
                            eprintln!(
                                "seller node RELAY-RECOVERY triggered: {}; reconnecting + \
                                 resubscribing with since={} overlap",
                                forced.unwrap_or_default(),
                                overlap_since.as_secs()
                            );
                        }
                        stalled_since_recovery = true;
                        match self.recover_stall(&relay, overlap_since).await {
                            Ok(attempts) => {
                                let outage = now_unix().saturating_sub(last_liveness_seen_unix);
                                // Grace: reset the watchdog clock so it does not immediately re-fire
                                // before the next tick's probe can answer.
                                last_liveness_seen = tokio::time::Instant::now();
                                last_liveness_seen_unix = now_unix();
                                // The full set was re-subscribed, so the open-pool half is back.
                                open_pool_degraded = false;
                                manual_recovery_succeeded = true;
                                eprintln!(
                                    "seller node RELAY-STALL recovery SUCCEEDED (attempts={attempts}, \
                                     outage={outage}s): reconnected + resubscribed \
                                     (offers+awards+1059, since={} overlap)",
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
                        // A relay `CLOSED` kills ONE subscription while the socket stays up, so the
                        // heartbeat watchdog cannot see it: close the 1059 leg and the node keeps
                        // heartbeating happily while every payment silently misses. Never fatal —
                        // always loud, always repaired.
                        Ok(RelayPoolNotification::Message {
                            message: nostr_sdk::RelayMessage::Closed { subscription_id, message: reason },
                            ..
                        }) => {
                            let id = subscription_id.to_string();
                            let label = subscription_label(&id);
                            eprintln!(
                                "seller node RELAY-CLOSED: relay closed the {label} subscription \
                                 (id={id}): {reason}"
                            );
                            // The offer REQ is the one subscription with a meaningful partial form:
                            // drop the un-pinned open-pool filter and re-subscribe targeted-only, so
                            // a relay that refuses the grouped REQ still leaves targeted claiming
                            // alive rather than taking the whole offer leg down.
                            if id == OFFER_SUB_ID && self.claim_open_pool() && !open_pool_degraded {
                                match self.subscribe_offers(None, false).await {
                                    Ok(()) => {
                                        open_pool_degraded = true;
                                        eprintln!(
                                            "seller node RELAY-CLOSED DEGRADE: offer subscription \
                                             re-armed TARGETED-ONLY (open-pool half dropped; \
                                             re-armed on the next successful recovery)"
                                        );
                                    }
                                    Err(error) => {
                                        eprintln!(
                                            "seller node RELAY-CLOSED degrade failed ({error}); \
                                             forcing full recovery on the next heartbeat tick"
                                        );
                                        forced_recovery = Some(format!(
                                            "offer subscription CLOSED and the targeted-only degrade failed: {error}"
                                        ));
                                    }
                                }
                            } else {
                                // Awards / 1059 / heartbeat have no partial form — repair them
                                // through the one paced recovery path so nothing re-dials the relay
                                // off-cadence.
                                forced_recovery =
                                    Some(format!("relay CLOSED the {label} subscription: {reason}"));
                            }
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

    /// Publish a feedback-kind (`status=error`) event to the buyer explaining why the seller will not
    /// deliver — so the buyer learns the reason instead of getting silence. Best-effort: signed
    /// through the signer actor and sent on the shared client; a failure is logged, never wedges the
    /// loop. Used for both the targeted under-rate refusal and an execution failure.
    async fn publish_buyer_feedback(&self, offer_id: &str, buyer_pubkey: &str, reason: &str) {
        let draft = error_draft(offer_id, buyer_pubkey, &self.seller_pubkey.to_hex(), reason);
        match self.node.signer().sign(draft, now_unix()).await {
            Ok(Ok(signed)) => {
                use nostr_sdk::JsonUtil as _;
                match nostr_sdk::Event::from_json(&signed.json) {
                    Ok(feedback) => match self.client.send_event_to([&self.relay_url], &feedback).await {
                        Ok(_) => eprintln!(
                            "seller node buyer feedback surfaced: offer={offer_id} reason={reason}"
                        ),
                        Err(error) => eprintln!(
                            "seller node WARN: buyer feedback publish failed offer={offer_id} ({error})"
                        ),
                    },
                    Err(error) => {
                        eprintln!("seller node buyer feedback encode failed (continuing): {error}")
                    }
                }
            }
            Ok(Err(error)) => {
                eprintln!("seller node buyer feedback sign failed (continuing): {error}")
            }
            Err(error) => {
                eprintln!("seller node signer actor gone at buyer feedback (continuing): {error}")
            }
        }
    }

    /// The targeted under-rate refusal feedback (see [`should_publish_under_rate_feedback`]).
    async fn publish_under_rate_feedback(&self, event: &nostr_sdk::Event, reason: &str) {
        self.publish_buyer_feedback(&event.id.to_hex(), &event.pubkey.to_hex(), reason)
            .await;
    }

    /// Re-ask the relay for stored payment gift-wraps and ingest whatever comes back.
    ///
    /// This is the money leg's recovery path, and it is response-based: we make a request and read
    /// what is returned, rather than waiting on a broadcast that may never come. A live kind-1059
    /// subscription that has silently gone deaf strands a payment indefinitely otherwise — see
    /// [`WRAP_BACKFILL_INTERVAL_SECS`] for the field case.
    ///
    /// Every wrap is routed through the normal `on_gift_wrap` path, so all the pay-once guards apply
    /// unchanged: a re-seen wrap hits the receipt dedup, and an already-spent token fails closed.
    /// Re-scanning a wide window is therefore safe by construction.
    ///
    /// LOAD-BEARING LOG: the "fetching" line is emitted unconditionally, BEFORE the fetch. It is the
    /// only periodic line a healthy idle node produces, which makes it the positive liveness signal
    /// external supervision has — a parked process satisfies pid-presence, so absence of failures is
    /// not evidence of health. Do not make it conditional to reduce noise.
    async fn run_wrap_backfill(&self) {
        let since = match resolve_backfill_since(
            self.node.store().last_receipt_unix(),
            self.node.store().oldest_unsettled_delivery_unix(),
        ) {
            Ok(since) => since,
            Err(error) => {
                eprintln!(
                    "seller node wrap backfill: ABORT — cursor read failed (retrying next cycle, \
                     NOT defaulting to since=0): {error}"
                );
                return;
            }
        };
        eprintln!("seller node wrap backfill (periodic): fetching stored kind-1059(s) since ts={since}");
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.seller_pubkey)
            .since(nostr_sdk::Timestamp::from(since));
        match tokio::time::timeout(
            WRAP_BACKFILL_FETCH_TIMEOUT,
            self.client
                .fetch_events(filter, WRAP_BACKFILL_FETCH_TIMEOUT / 2),
        )
        .await
        {
            Ok(Ok(events)) => {
                eprintln!(
                    "seller node wrap backfill (periodic): {} stored kind-1059(s) returned since ts={since}",
                    events.len()
                );
                for event in events {
                    self.on_gift_wrap(&event).await;
                }
                self.drain().await;
            }
            Ok(Err(error)) => eprintln!(
                "seller node WARN: wrap backfill fetch failed (continuing; live 1059 subscription \
                 active): {error}"
            ),
            Err(_) => eprintln!(
                "seller node WARN: wrap backfill fetch timed out after {}s (continuing; live 1059 \
                 subscription active)",
                WRAP_BACKFILL_FETCH_TIMEOUT.as_secs()
            ),
        }
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
    ) -> Result<u32, NodeError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self
                .reconnect_and_resubscribe(relay, overlap_since)
                .await
            {
                Ok(()) => return Ok(attempt),
                Err(error) if attempt < RECOVERY_MAX_ATTEMPTS => {
                    let backoff = recovery_backoff(attempt);
                    eprintln!(
                        "seller node RELAY-STALL recovery attempt {attempt} failed ({error}); \
                         retrying in {}s",
                        backoff.as_secs()
                    );
                    tokio::time::sleep(backoff).await;
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
    ) -> Result<(), NodeError> {
        reconnect_and_authenticate(&self.client, relay)
            .await
            .map_err(|error| NodeError::Relay(format!("reconnect NIP-42 auth: {error}")))?;
        self.client.unsubscribe_all().await;
        self.subscribe_all(Some(overlap_since)).await
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
                eprintln!(
                    "seller node offer skip id={}: {}",
                    event.id,
                    offer_parse_refusal(&error)
                );
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
            ClaimDecision::Skip(skip) => {
                eprintln!("seller node offer skip id={}: {}", event.id, skip.reason());
                // Buyer-visibility: a TARGETED-to-self under-rate refusal also emits a feedback-kind
                // `status=error` so the buyer learns WHY (distinguishes rate-refusal from a crash /
                // silence). Open-pool under-rate stays log-only (spam guard); a lapsed offer never
                // emits (only RateGate). Mirrors the legacy under-rate feedback dropped at cutover.
                let targeted_to_self = offer.seller_pubkey.as_deref() == Some(seller_pubkey.as_str());
                if should_publish_under_rate_feedback(skip, targeted_to_self, offer.amount, seller.rate_sats)
                {
                    self.publish_under_rate_feedback(event, skip.reason()).await;
                }
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
        // Idempotency guard: only a job still `awarded`/`executing` runs. A REDUNDANT award (a second
        // award event with a different award_id for a job already delivered/paid — seen live in the
        // smoke) or any re-drive must NOT re-run the agent: a duplicate execute burns operator compute
        // for nothing and its push is rejected non-fast-forward. It must also never clobber a terminal
        // state (delivered/paid/failed). Delivered/paid/failed ⇒ early-return, no second execute.
        match self.node.store().job_state(job_id) {
            Ok(Some(state)) if should_resume_execution(state) => {}
            Ok(Some(state)) => {
                eprintln!(
                    "seller node execute skip job_id={job_id}: job already {state:?} (idempotent — not re-run)"
                );
                return;
            }
            Ok(None) => {
                eprintln!("seller node execute skip job_id={job_id}: no job row (idempotent)");
                return;
            }
            Err(error) => {
                eprintln!("seller node execute job_id={job_id}: job_state read failed ({error}); not executing");
                return;
            }
        }

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
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
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
        if let Err(error) = seller_git::init_empty_delivery_workdir_off_runtime(
            workdir.clone(),
            identity.clone(),
        )
        .await
        {
            eprintln!("seller node execute fail job_id={job_id}: workdir init failed ({error})");
            self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
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
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
                return;
            }
        };

        // Snapshot the agent's final workdir tree into ONE delivery commit at the stored author date.
        // An empty / no-op tree is refused with a precise reason (nothing to deliver).
        let branch = format!("mobee/{}", &job_id[..8.min(job_id.len())]);
        let message = delivery_message(&offer.task);
        if let Err(error) = seller_git::snapshot_delivery_at_off_runtime(
            workdir.clone(),
            identity.clone(),
            None,
            branch.clone(),
            message,
            author_date,
        )
        .await
        {
            eprintln!("seller node execute fail job_id={job_id}: delivery snapshot refused ({error})");
            self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
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
                    self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
                    return;
                }
                Err(error) => {
                    eprintln!("seller node execute fail job_id={job_id}: signer actor gone ({error})");
                    self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
                    return;
                }
            }
        } else {
            None
        };
        let commit = match seller_git::push_branch_with_header_off_runtime(
            workdir.clone(),
            seller.git_remote.clone(),
            branch.clone(),
            push_header,
        )
        .await
        {
            Ok(oid) => oid,
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: git push failed ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
                return;
            }
        };

        // Bind the trade + delivered commit + STORED creq hash into the co-signature preimage and
        // sign it through the signer actor (the seller key never leaves the actor).
        let delivery_kind = match seller_delivery_kind(&seller.git_remote, &branch, &commit) {
            Ok(kind) => kind,
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: delivery kind typing failed ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
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
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
                return;
            }
            Err(error) => {
                eprintln!("seller node execute fail job_id={job_id}: signer actor gone ({error})");
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
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
                self.fail_job_with_feedback(job_id, &offer.buyer_pubkey, EXEC_FAILURE_FEEDBACK).await;
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
                // `event_id` is the kind-1059 payment gift-wrap — the id this collection is
                // journaled and deduped under. It is NOT the co-signed kind-3400 receipt (the buyer
                // publishes that; the seller never sees its id on this path), so name it for what it
                // is rather than inviting an operator to grep the relay for a 3400 that will not
                // match.
                eprintln!(
                    "seller node paid job_id={job_id} amount={amount_received} payment_wrap={event_id}"
                )
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

    /// Fail the job AND tell the buyer why (a feedback-kind `status=error`), so an execution failure
    /// is not silent on the wire (the buyer waits on a delivery that will never come otherwise). Used
    /// at the post-offer execute fail points where the offer buyer is known.
    async fn fail_job_with_feedback(&self, job_id: &str, buyer_pubkey: &str, reason: &str) {
        self.fail_job(job_id).await;
        self.publish_buyer_feedback(job_id, buyer_pubkey, reason).await;
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
        assert_eq!(decision, ClaimDecision::Skip(SkipReason::Lapsed));
    }

    // Below the rate floor ⇒ skip (never claim work priced under the seller's floor).
    #[test]
    fn refuses_below_rate() {
        let decision = classify_offer(&offer(1, Some(SELLER), NOW + 600), &seller_cfg(5, false), SELLER, NOW);
        assert_eq!(decision, ClaimDecision::Skip(SkipReason::RateGate));
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
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &seller_cfg(2, false), SELLER, NOW),
            ClaimDecision::Skip(SkipReason::RateGate)
        );
        assert_eq!(
            classify_offer(&offer(5, None, NOW + 600), &seller_cfg(2, true), SELLER, NOW),
            ClaimDecision::Claim { deadline_unix: NOW + 600 }
        );
    }

    // TOOTH (#146 / #117 refusal taxonomy) — a cross-version offer is a DISTINCT refusal, not the
    // generic "unparseable" bucket. Build a well-formed offer, then swap ONLY its `v` tag so the sole
    // parse failure is version skew; the node's on_offer routes that to the unsupported-version skip.
    #[test]
    fn unsupported_version_offer_is_a_distinct_parse_refusal() {
        let offer = gateway::OfferDraft::new("do a task", "text/plain", 5, NOW + 600, &"a".repeat(64));
        let mut draft = offer.to_event_draft();
        for tag in &mut draft.tags {
            if tag.0.first().map(String::as_str) == Some("v") {
                tag.0 = vec!["v".to_owned(), "99".to_owned()];
            }
        }
        let skew = parse_offer(&draft).expect_err("version skew must not parse");
        assert!(
            matches!(&skew, OfferParseError::UnsupportedVersion(v) if v == "99"),
            "version skew must parse as a distinct UnsupportedVersion, not generic unparseable"
        );

        // The ROUTING is the thing under test: pinning `parse_offer`'s enum alone let a revert that
        // collapsed on_offer's version arm into the generic bucket pass green. Assert on the refusal
        // on_offer actually emits, and that a genuinely malformed offer emits a DIFFERENT one.
        let mut malformed = draft.clone();
        malformed.tags.clear();
        let broken = parse_offer(&malformed).expect_err("a tagless offer must not parse");

        let skew_reason = offer_parse_refusal(&skew);
        let broken_reason = offer_parse_refusal(&broken);
        assert!(
            skew_reason.contains("unsupported mobee protocol version") && skew_reason.contains("99"),
            "the version-skew refusal must say so and name the version, got {skew_reason:?}"
        );
        assert!(
            broken_reason.contains("unparseable"),
            "a malformed offer stays in the generic bucket, got {broken_reason:?}"
        );
        assert_ne!(
            skew_reason, broken_reason,
            "collapsing version skew into the generic unparseable bucket is the #146 regression"
        );
    }

    // TOOTH (#171 layer 2 / #172) — the offer REQ carries the un-pinned open-pool filter IFF the
    // seller opted in, and BOTH filters carry the `#t=mobee` namespace guard. The node subscribed
    // targeted-only unconditionally, so a `claim_open_pool = true` seller ran a claim gate over
    // offers its subscription could never deliver. Bite: drop the `claim_open_pool` branch and the
    // two-filter assertions go red; drop the hashtag and the guard assertions go red.
    #[test]
    fn open_pool_filter_rides_the_offer_req_iff_opted_in() {
        let seller = nostr_sdk::prelude::Keys::generate().public_key();
        let now = nostr_sdk::Timestamp::from(NOW);

        let targeted_only = offer_subscription_filters(seller, false, 1200, None, now);
        assert_eq!(
            targeted_only.len(),
            1,
            "a targeted-only seller subscribes exactly the pinned filter"
        );
        assert_eq!(
            targeted_only[0].generic_tags.get(&nostr_sdk::SingleLetterTag::lowercase(
                nostr_sdk::Alphabet::P
            )),
            Some(&[seller.to_hex()].into_iter().collect()),
            "the targeted filter must stay pinned to this seller"
        );

        let open_pool = offer_subscription_filters(seller, true, 1200, None, now);
        assert_eq!(
            open_pool.len(),
            2,
            "an open-pool seller must ALSO subscribe the un-pinned filter — without it the \
             claim_open_pool gate governs offers that never arrive"
        );
        assert!(
            open_pool[1]
                .generic_tags
                .get(&nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::P))
                .is_none(),
            "the open-pool filter is un-pinned by definition"
        );

        // The namespace guard rides BOTH filters: a foreign event squatting the offer kind is never
        // even delivered.
        let hashtag = nostr_sdk::SingleLetterTag::lowercase(nostr_sdk::Alphabet::T);
        for (index, filter) in open_pool.iter().enumerate() {
            assert_eq!(
                filter.generic_tags.get(&hashtag),
                Some(&[crate::gateway::MOBEE_TAG.to_owned()].into_iter().collect()),
                "offer filter {index} must carry the #t=mobee namespace guard"
            );
        }

        // `offer_backfill_secs = 0` is live-only: `since(now)` + `limit(0)` requests zero stored
        // offers. A window asks for a bounded stored burst instead.
        let live_only = offer_subscription_filters(seller, true, 0, None, now);
        assert_eq!(live_only[1].limit, Some(0), "live-only requests no stored offers");
        assert_eq!(live_only[1].since, Some(now));
        let windowed = offer_subscription_filters(seller, true, 1200, None, now);
        assert_eq!(windowed[1].limit, Some(OFFER_BACKFILL_LIMIT));
        assert_eq!(windowed[1].since, Some(nostr_sdk::Timestamp::from(NOW - 1200)));

        // On a post-stall resubscribe BOTH filters carry the overlap cursor — only the stall gap is
        // missing, and the classify-level deadline refusal is the staleness guard.
        let overlap = nostr_sdk::Timestamp::from(NOW - 60);
        let resubscribed = offer_subscription_filters(seller, true, 1200, Some(overlap), now);
        for filter in &resubscribed {
            assert_eq!(filter.since, Some(overlap));
        }
    }

    // TOOTH (#171 layer 1, THE fix) — an in-process reconnect re-authenticates and the receive path
    // comes BACK, against a fixture that enforces NIP-42 before it will serve a REQ.
    //
    // The fixture parity matters: the previous watchdog teeth ran against a LocalRelay that served
    // reads unauthenticated, so the auth step was decorative and the ordering bug shipped green. Here
    // `RelayBuilderNip42Mode::Both` refuses a REQ from an unauthenticated session, so an event can
    // only arrive if auth genuinely completed on the new socket.
    //
    // The assertion is DELIVERY, not a return code: a live socket happily coexists with dead
    // subscriptions (that is exactly what wedged the field nodes — heartbeating, deaf), so a tooth
    // that only checked `Ok(..)` would be the same false green.
    //
    // BITE: swap the two lines in `reconnect_and_authenticate` so `relay.notifications()` is taken
    // before `client.disconnect()`, and this goes red — the auth wait reads our own Shutdown and
    // returns "relay shutdown before NIP-42 authentication".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_reauthenticates_and_delivery_resumes_in_process() {
        use nostr_relay_builder::prelude::{
            LocalRelay, RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode,
        };
        use nostr_sdk::prelude::{Client, EventBuilder, Keys, RelayOptions, RelayUrl};

        let wait = std::time::Duration::from_secs(10);

        // Auth-enforcing fixture: it will not serve a REQ (nor accept an EVENT) until the session
        // has completed NIP-42.
        let relay_fixture = LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42 {
            mode: RelayBuilderNip42Mode::Both,
        }));
        relay_fixture.run().await.expect("fixture relay run");
        let relay_url = relay_fixture.url().await.to_string();

        let seller = Keys::generate();
        let client = Client::new(seller.clone());
        client.automatic_authentication(true);
        client
            .pool()
            .add_relay(&relay_url, RelayOptions::default().reconnect(true))
            .await
            .expect("add relay");
        let relay = client
            .relays()
            .await
            .get(&RelayUrl::parse(&relay_url).expect("relay url"))
            .cloned()
            .expect("relay handle");

        // The beacon is published by a SEPARATE client, which is both the faithful shape (the node's
        // receive path carries events from OTHERS — offers, awards, payment wraps) and a hard
        // requirement: `RelayPool::send_event_to` saves every published event into the publishing
        // client's own database (pool/mod.rs:767), and the inbound handler drops any event already
        // present there without notifying (relay/inner.rs:1215-1218). A client therefore cannot
        // observe its own event coming back — using this client to publish would test nothing.
        let author = Keys::generate();
        let author_pubkey = author.public_key();
        let publisher = Client::new(author.clone());
        publisher.automatic_authentication(true);
        publisher
            .pool()
            .add_relay(&relay_url, RelayOptions::default())
            .await
            .expect("add relay (publisher)");
        let publisher_relay = publisher
            .relays()
            .await
            .get(&RelayUrl::parse(&relay_url).expect("relay url"))
            .cloned()
            .expect("publisher relay handle");
        let mut publisher_notifications = publisher_relay.notifications();
        publisher.connect().await;
        publisher.wait_for_connection(wait).await;
        // Writes are auth-gated too, and this fixture challenges on the first REQ — so probe once to
        // get the publisher authenticated before it tries to publish anything.
        publisher
            .subscribe(Filter::new().kind(Kind::TextNote).limit(0), None)
            .await
            .expect("publisher probe subscribe");
        relay_auth::wait_for_nip42_auth(&mut publisher_notifications, wait)
            .await
            .expect("publisher auth");

        let beacon_filter = Filter::new().kind(Kind::TextNote).author(author_pubkey);
        let beacon = |content: &str| {
            EventBuilder::new(Kind::TextNote, content)
                .sign_with_keys(&author)
                .expect("sign beacon")
        };
        // Await one specific event on a pool receiver. Returns false on timeout — the failure this
        // whole tooth exists to catch is silence.
        async fn arrives(
            notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
            id: nostr_sdk::EventId,
            wait: std::time::Duration,
        ) -> bool {
            tokio::time::timeout(wait, async {
                loop {
                    match notifications.recv().await {
                        Ok(RelayPoolNotification::Event { event, .. }) if event.id == id => {
                            return true
                        }
                        Ok(_) => continue,
                        Err(_) => return false,
                    }
                }
            })
            .await
            .unwrap_or(false)
        }
        // EOSE is the relay confirming our REQ is registered. Waiting for it makes the test
        // deterministic: publishing before the subscription lands would race, and a race here would
        // read as the very silence the tooth is meant to detect.
        async fn subscription_live(
            notifications: &mut tokio::sync::broadcast::Receiver<RelayPoolNotification>,
            wait: std::time::Duration,
        ) -> bool {
            tokio::time::timeout(wait, async {
                loop {
                    match notifications.recv().await {
                        Ok(RelayPoolNotification::Message {
                            message: nostr_sdk::RelayMessage::EndOfStoredEvents(_),
                            ..
                        }) => return true,
                        Ok(_) => continue,
                        Err(_) => return false,
                    }
                }
            })
            .await
            .unwrap_or(false)
        }

        // Boot the way the node does: receiver before connect, then subscribe. This fixture
        // challenges lazily (on the first REQ) where the deployed relay challenges on connect;
        // either way auto-auth answers it and `Authenticated` is what the node waits for.
        let mut boot_notifications = relay.notifications();
        client.connect().await;
        client.wait_for_connection(wait).await;
        client
            .subscribe(beacon_filter.clone(), None)
            .await
            .expect("boot subscribe");
        assert_eq!(
            relay_auth::wait_for_nip42_auth(&mut boot_notifications, wait)
                .await
                .expect("boot auth"),
            AuthWait::Authenticated,
            "the fixture must actually enforce NIP-42 — if it never challenges, this whole tooth \
             is decorative (which is how the ordering bug shipped green)"
        );

        // Baseline: delivery works BEFORE the reconnect, so a post-reconnect silence is the code's
        // fault and not the harness's. Re-subscribe post-auth exactly as the recovery path does —
        // the boot REQ was refused pre-auth — and wait for the relay to confirm it.
        let mut notifications = client.notifications();
        client.unsubscribe_all().await;
        client
            .subscribe(beacon_filter.clone(), None)
            .await
            .expect("post-auth subscribe");
        assert!(
            subscription_live(&mut notifications, wait).await,
            "harness check: the relay must confirm (EOSE) the post-auth subscription"
        );
        let before = beacon("pre-reconnect baseline");
        publisher
            .send_event(&before)
            .await
            .expect("publish baseline");
        assert!(
            arrives(&mut notifications, before.id, wait).await,
            "harness check: the subscription must deliver before we induce the reconnect"
        );

        // THE PRODUCTION PATH under test: an in-process reconnect, no process restart.
        let outcome = reconnect_and_authenticate(&client, &relay)
            .await
            .expect("in-process reconnect must re-authenticate — this is #171");
        assert_eq!(
            outcome,
            AuthWait::Authenticated,
            "the reconnect must complete NIP-42 on the NEW socket, not report a shutdown it caused"
        );

        // What the recovery path does next: replace the stale subscriptions AFTER auth.
        let mut post = client.notifications();
        client.unsubscribe_all().await;
        client
            .subscribe(beacon_filter, None)
            .await
            .expect("post-reconnect subscribe");
        assert!(
            subscription_live(&mut post, wait).await,
            "the relay must serve the post-reconnect REQ — on this fixture that is only possible \
             on an authenticated session"
        );

        let after = beacon("post-reconnect liveness beacon");
        publisher.send_event(&after).await.expect("publish beacon");
        assert!(
            arrives(&mut post, after.id, wait).await,
            "the receive path must be ALIVE after an in-process reconnect — a recovery that \
             returns Ok while nothing is delivered is the silent wedge this fixes"
        );

        client.disconnect().await;
        publisher.disconnect().await;
    }

    // TOOTH (#171 TRIGGER) — the liveness probe asserts the property the watchdog actually needs:
    // "the relay is serving MY REQs on THIS authenticated session". Both halves, because a probe that
    // can only ever answer one way is exactly the bug being fixed — the own-heartbeat round-trip it
    // replaced could NEVER succeed (nostr-sdk saves published events into the client's own database
    // and then swallows the relay's echo of them), so every node declared a stall every
    // `stall_threshold` forever, healthy or not, and drove a recovery that could not succeed either.
    //
    // BITE (positive half): break the probe — wrong sub id in the EOSE match, or drop the `limit(0)`
    // REQ — and the authenticated case goes red.
    // BITE (negative half): make the probe return true on timeout, or accept any EOSE regardless of
    // session, and the unauthenticated case goes red. That half is what pins "on THIS session":
    // against this fixture an unauthenticated REQ is answered with CLOSED and never an EOSE.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn liveness_probe_answers_only_on_an_authenticated_session() {
        use nostr_relay_builder::prelude::{
            LocalRelay, RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode,
        };
        use nostr_sdk::prelude::{Client, Keys, RelayOptions};

        let wait = std::time::Duration::from_secs(10);
        let relay_fixture = LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42 {
            mode: RelayBuilderNip42Mode::Both,
        }));
        relay_fixture.run().await.expect("fixture relay run");
        let relay_url = relay_fixture.url().await.to_string();

        // POSITIVE: an authenticated session. Auto-auth answers the fixture's challenge (raised on the
        // probe's own REQ), the relay serves it, and the EOSE comes back.
        let seller = Keys::generate();
        let authed = Client::new(seller.clone());
        authed.automatic_authentication(true);
        authed
            .pool()
            .add_relay(&relay_url, RelayOptions::default())
            .await
            .expect("add relay (authed)");
        authed.connect().await;
        authed.wait_for_connection(wait).await;
        assert!(
            probe_relay_serves_our_reqs(&authed, seller.public_key(), wait).await,
            "the probe must be answerable on a healthy authenticated session — otherwise the \
             watchdog is back to a signal that can never arrive"
        );

        // NEGATIVE: same relay, same probe, but auto-auth OFF so the session never authenticates. The
        // fixture answers the REQ with CLOSED instead of serving it, so no EOSE ever arrives and the
        // probe must report the loss of liveness rather than assuming it.
        let stranger = Keys::generate();
        let unauthed = Client::new(stranger.clone());
        unauthed.automatic_authentication(false);
        unauthed
            .pool()
            .add_relay(&relay_url, RelayOptions::default())
            .await
            .expect("add relay (unauthed)");
        unauthed.connect().await;
        unauthed.wait_for_connection(wait).await;
        assert!(
            !probe_relay_serves_our_reqs(
                &unauthed,
                stranger.public_key(),
                std::time::Duration::from_secs(2),
            )
            .await,
            "a session the relay refuses to serve is NOT alive — reporting it alive is how the \
             watchdog would go blind in the other direction"
        );

        authed.disconnect().await;
        unauthed.disconnect().await;
    }

    // TOOTH (wrap backfill) — the cursor keeps an OLDER delivered-but-unpaid job's payment window in
    // range, and a read failure ABORTS rather than silently becoming a full-history rescan.
    //
    // The clamp is the whole point: a receipt collected for a NEWER job must not advance the cursor
    // past an OLDER unsettled delivery, or that job's payment wrap falls out of every future backfill
    // and the payment is stranded forever — which is the failure this backfill exists to recover.
    //
    // BITE: drop the `.min(oldest - margin)` clamp and `cursor_stays_behind_an_unsettled_delivery`
    // goes red; make the error arm fall back to 0 and the abort assertion goes red.
    #[test]
    fn wrap_backfill_cursor_clamps_to_the_oldest_unsettled_delivery_and_fails_closed() {
        use super::super::store::StoreError;

        // Nothing collected, nothing unsettled ⇒ 0 is legitimate (first boot), not an error.
        assert_eq!(resolve_backfill_since(Ok(None), Ok(None)).expect("fresh"), 0);

        // A receipt at t=10_000 with NO unsettled delivery ⇒ cursor is the receipt.
        assert_eq!(
            resolve_backfill_since(Ok(Some(10_000)), Ok(None)).expect("settled"),
            10_000
        );

        // cursor_stays_behind_an_unsettled_delivery: a NEWER receipt must not step over an OLDER
        // delivered-but-unpaid job — the cursor clamps to that job's delivery minus the skew margin.
        let cursor = resolve_backfill_since(Ok(Some(10_000)), Ok(Some(6_000))).expect("clamped");
        assert_eq!(cursor, (6_000 - WRAP_BACKFILL_MARGIN_SECS) as u64);
        assert!(
            cursor < 6_000,
            "the cursor must sit BEFORE the unsettled delivery or its wrap is never re-fetched"
        );

        // Fail-closed: a store READ ERROR aborts the cycle. Falling back to 0 would turn a transient
        // failure into a full-history rescan.
        assert!(
            resolve_backfill_since(Err(StoreError("boom".into())), Ok(None)).is_err(),
            "a cursor read failure must abort, never default to since=0"
        );
        assert!(
            resolve_backfill_since(Ok(Some(1)), Err(StoreError("boom".into()))).is_err(),
            "an unsettled-delivery read failure must abort too"
        );
    }

    // TOOTH (wrap backfill, store) — the two cursor readers answer over real rows: a delivered job
    // with no receipt is "unsettled" and pins the cursor; collecting its receipt releases it.
    #[test]
    fn unsettled_delivery_pins_the_cursor_until_its_receipt_lands() {
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

        assert_eq!(store.last_receipt_unix().expect("receipts"), None);
        assert_eq!(
            store.oldest_unsettled_delivery_unix().expect("unsettled"),
            None,
            "nothing delivered yet"
        );

        let draft = claim_draft(&job, &buyer, &seller, &creq);
        let delivered_at = 6_000;
        assert!(store
            .deliver_and_enqueue(
                &job,
                &"c".repeat(40),
                &draft,
                delivered_at,
                delivered_at + RESULT_PUBLISH_WINDOW_SECS,
                delivered_at
            )
            .expect("deliver"));
        assert_eq!(
            store.oldest_unsettled_delivery_unix().expect("unsettled"),
            Some(delivered_at),
            "a delivered job with no receipt is unsettled and must pin the backfill cursor"
        );

        // The payment lands ⇒ no longer unsettled, and the receipt time becomes the cursor.
        store
            .collect_receipt(&"d".repeat(64), &job, 21, 10_000)
            .expect("collect");
        assert_eq!(store.last_receipt_unix().expect("receipts"), Some(10_000));
        assert_eq!(
            store.oldest_unsettled_delivery_unix().expect("unsettled"),
            None,
            "a settled delivery must stop pinning the cursor"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (#171 layer 3) — recovery attempts back off instead of re-dialing at a flat interval,
    // and the backoff is capped so one bounded recovery still fits inside a heartbeat interval.
    #[test]
    fn recovery_backoff_grows_and_stays_capped() {
        assert_eq!(recovery_backoff(1), RECOVERY_BACKOFF);
        assert!(
            recovery_backoff(2) > recovery_backoff(1),
            "a flat retry interval is what hammered shared relay infrastructure"
        );
        for attempt in 1..=64 {
            assert!(
                recovery_backoff(attempt) <= RECOVERY_BACKOFF_MAX,
                "attempt {attempt} exceeded the cap"
            );
        }
    }

    // TOOTH (buyer-facing feedback) — a TARGETED under-rate refusal surfaces a 3404 to the buyer;
    // open-pool under-rate, an at/above-rate offer, and a lapsed skip never do.
    #[test]
    fn under_rate_feedback_only_for_targeted_under_rate_rate_gate() {
        // Targeted-to-self + under-rate + RateGate ⇒ publish (buyer learns why).
        assert!(should_publish_under_rate_feedback(SkipReason::RateGate, true, 1, 5));
        // Open-pool (not targeted-to-self) under-rate ⇒ log-only (spam guard).
        assert!(!should_publish_under_rate_feedback(SkipReason::RateGate, false, 1, 5));
        // Targeted but at/above rate ⇒ no refusal feedback.
        assert!(!should_publish_under_rate_feedback(SkipReason::RateGate, true, 5, 5));
        // A lapsed skip never emits under-rate feedback, even if targeted + under-rate.
        assert!(!should_publish_under_rate_feedback(SkipReason::Lapsed, true, 1, 5));
    }

    // TOOTH (idempotency, live-caught) — the execute guard keys on job_state: a job already DELIVERED
    // or PAID is not re-execute-eligible, so a DUPLICATE award (a second award_id for the same job —
    // seen live in the smoke) does no second agent run (no wasted operator compute) and never clobbers
    // the terminal state. Bite: were should_resume_execution to admit Delivered/Paid, execute_job
    // would re-run the agent — the assertions here go red (and so does the resume-selection tooth).
    #[test]
    fn delivered_or_paid_job_is_not_re_executed() {
        use crate::seller_node::store::{Collected, JobState};
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
        let draft = claim_draft(&job, &buyer, &seller, &creq);

        // Deliver ⇒ state Delivered ⇒ NOT re-execute-eligible (the guard early-returns).
        assert!(store
            .deliver_and_enqueue(&job, &"c".repeat(40), &draft, 5000, 5000 + RESULT_PUBLISH_WINDOW_SECS, 5000)
            .expect("deliver"));
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Delivered));
        assert!(
            !should_resume_execution(store.job_state(&job).expect("s").expect("s")),
            "a delivered job must not re-execute on a duplicate award"
        );

        // Pay ⇒ state Paid ⇒ likewise not re-execute-eligible (terminal never clobbered).
        assert_eq!(
            store.collect_receipt(&"e".repeat(64), &job, 21, 6000).expect("collect"),
            Collected::New
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Paid));
        assert!(
            !should_resume_execution(store.job_state(&job).expect("s").expect("s")),
            "a paid job must not re-execute"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // TOOTH (buyer-facing feedback) — an execution failure produces a buyer-addressed feedback-kind
    // `status=error` carrying the (path-free) reason, so the buyer learns the job failed instead of
    // waiting on a delivery that never comes (the silence the live smoke's first attempt exposed).
    #[test]
    fn execution_failure_feedback_is_a_buyer_addressed_error() {
        let draft = error_draft("offer1", "buyerpk", &"s".repeat(64), EXEC_FAILURE_FEEDBACK);
        assert_eq!(draft.kind, crate::kinds::JOB_FEEDBACK_KIND);
        assert_eq!(draft.content, EXEC_FAILURE_FEEDBACK);
        let has = |name: &str, val: &str| {
            draft.tags.iter().any(|tag| {
                tag.0.first().map(String::as_str) == Some(name)
                    && tag.0.get(1).map(String::as_str) == Some(val)
            })
        };
        assert!(has("status", "error"), "feedback carries status=error");
        assert!(has("p", "buyerpk"), "addressed to the buyer");
        assert!(has("e", "offer1"), "references the offer");
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
