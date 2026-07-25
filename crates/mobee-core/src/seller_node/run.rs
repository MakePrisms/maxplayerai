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

use crate::home::{self, MobeeHome};
use crate::kinds::{JOB_AWARD_KIND, JOB_OFFER_KIND};
use crate::relay_auth::{self, AuthWait};

use super::outbox::drain_once;
use super::publisher::RelayPublisher;
use super::{now_unix, NodeError, SellerNode};

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
                        // PORT: classify_offer (rate/expiry/contribution/dedup over the store) →
                        // build_seller_creq → claim_and_enqueue(creq) → drain.
                        k if k.as_u16() == JOB_OFFER_KIND => {
                            eprintln!("seller node offer seen id={} (claim PORT pending)", event.id);
                        }
                        // PORT: match_award over parked claims → store.record_award → execute.
                        k if k.as_u16() == JOB_AWARD_KIND => {
                            eprintln!("seller node award seen id={} (bind PORT pending)", event.id);
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
