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

use crate::gateway::{self, claim_draft, parse_award, parse_offer, ParsedOffer};
use crate::home::{self, MobeeHome};
use crate::job_lifecycle::event_to_draft;
use crate::kinds::{JOB_AWARD_KIND, JOB_OFFER_KIND};
use crate::relay_auth::{self, AuthWait};
use crate::seller::rate_gate_allows;

use super::outbox::drain_once;
use super::publisher::RelayPublisher;
use super::{now_unix, NodeError, SellerNode};

/// How long (seconds) the outbox publisher keeps retrying a claim event before it expires. Matches
/// the legacy claim TTL: a claim outlives a slow relay but never lingers indefinitely.
const CLAIM_PUBLISH_WINDOW_SECS: i64 = 3600;
/// Upper bound on parked claims awaiting an award (bounded memory / back-pressure), mirroring the
/// legacy AWAITING_AWARD_CAP: a claim is cheap (no compute until the award), so several may be held.
const AWAITING_AWARD_CAP: i64 = 32;

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

    /// Run the live loop until the relay pool closes. Ingests offers/awards/gift-wraps and drains the
    /// outbox on a periodic tick.
    pub async fn run(self) -> Result<(), NodeError> {
        // Targeted subscriptions (p-tagged to the seller). The open-pool/backfill-window offer
        // filters and the self-heartbeat watchdog subscription are added in the watchdog PORT step.
        let offer_filter = Filter::new()
            .kind(Kind::Custom(JOB_OFFER_KIND))
            .pubkey(self.seller_pubkey);
        let award_filter = Filter::new()
            .kind(Kind::Custom(JOB_AWARD_KIND))
            .hashtag(crate::gateway::MOBEE_TAG)
            .pubkey(self.seller_pubkey);
        let wrap_filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.seller_pubkey);

        let mut notifications = self.client.notifications();
        for filter in [offer_filter, award_filter, wrap_filter] {
            self.client
                .subscribe(filter, None)
                .await
                .map_err(|error| NodeError::Relay(format!("subscribe: {error}")))?;
        }
        eprintln!(
            "seller node live: pubkey={} relay={}",
            self.seller_pubkey.to_hex(),
            self.relay_url
        );

        // Drain anything reconcile left pending before the first tick.
        self.drain().await;

        let mut drain_tick = tokio::time::interval(DRAIN_INTERVAL);
        loop {
            let notification = tokio::select! {
                _ = drain_tick.tick() => {
                    // PORT: award-deadline sweep + periodic wrap backfill + heartbeat/watchdog ride
                    // this tick alongside the drain.
                    self.drain().await;
                    continue;
                }
                recv = notifications.recv() => match recv {
                    Ok(notification) => notification,
                    Err(error) => {
                        // A broadcast lag is recoverable — never go permanently deaf.
                        eprintln!("seller node WARN: notification stream {error}; continuing");
                        continue;
                    }
                },
            };
            match notification {
                RelayPoolNotification::Event { event, .. } => {
                    match event.kind {
                        k if k.as_u16() == JOB_OFFER_KIND => {
                            self.on_offer(&event).await;
                        }
                        k if k.as_u16() == JOB_AWARD_KIND => {
                            self.on_award(&event).await;
                        }
                        // PORT: gift-wrap unwrap → import-before-receipt-row → store.collect_receipt.
                        Kind::GiftWrap => {
                            eprintln!("seller node gift-wrap seen id={} (pay PORT pending)", event.id);
                        }
                        _ => {}
                    }
                    self.drain().await;
                }
                RelayPoolNotification::Shutdown => {
                    eprintln!("seller node: relay pool shutdown; loop ending");
                    break;
                }
                _ => {}
            }
        }
        Ok(())
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
                        eprintln!("seller node awarded job_id={job_id} buyer={buyer} — execute PORT pending");
                        // PORT: execute the awarded job store-backed (agent run + delivery, signing
                        // the STORED creq's hash) — the next arm.
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
}
