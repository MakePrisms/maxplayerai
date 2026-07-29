//! LIVE-RELAY acceptance legs for the participation surface.
//!
//! Two fixtures, because the legs ask two different kinds of question:
//!
//! - `nostr-relay-builder`'s `LocalRelay` is a real NIP-01 relay that stores and serves events, so
//!   it can answer "did the node join, ingest, and resume correctly" (legs 1–3).
//! - [`super::p_gate_relay_fixture::PGateRelay`] records every `REQ` it receives and answers `EOSE`
//!   without ever serving an event — which is exactly the shape of a healthy authenticated
//!   connection with no read admission. That makes it the only fixture that can answer "did we send
//!   this relay anything after classifying it denied" (leg 4), because the answer is a frame count,
//!   not a behaviour.
//!
//! ## What these legs cannot prove
//!
//! Neither fixture is buzz. A real buzz relay **signs** the `44100` itself in response to a member's
//! `kind-9000`; here a member key publishes the `44100` directly. So these legs prove the node's
//! response to a membership notification, not the relay's production of one — the `9000 → 44100`
//! half is the deployed relay's behaviour and belongs to a live run against it. Recorded rather than
//! glossed, per the acceptance block's own framing.

use super::participation::ParticipationConfig;
use super::participation::relays::AccessState;
use super::participation::runtime::Participation;
use super::store::SellerStore;
use crate::seller_node::p_gate_relay_fixture::PGateRelay;
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::{
    Client, Event, EventBuilder, Keys, Kind, PublicKey, Tag, TagKind, Timestamp,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static SEQ: AtomicU64 = AtomicU64::new(0);

const CHANNEL: &str = "chan-acceptance-1";

fn temp_db(label: &str) -> std::path::PathBuf {
    let id = SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "mobee-participation-it-{label}-{}-{id}.sqlite",
        std::process::id()
    ))
}

async fn start_relay() -> (LocalRelay, String) {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("relay run");
    let url = relay.url().await.to_string();
    (relay, url)
}

async fn connect(url: &str, keys: Keys) -> Client {
    let client = Client::new(keys);
    client.automatic_authentication(true);
    client.add_relay(url).await.expect("add relay");
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(5)).await;
    client
}

/// The node's persona: the access-probe carrier. A stored kind, per [`super::participation::probe`].
fn carrier(node: &Keys) -> Event {
    EventBuilder::new(Kind::Metadata, r#"{"name":"mobee-acceptance"}"#)
        .sign_with_keys(node)
        .expect("sign carrier")
}

/// A relay-signed-shaped membership notification: `h` = channel, `p` = the node.
fn membership(member: &Keys, kind: u16, node: PublicKey, at: u64) -> Event {
    EventBuilder::new(Kind::from(kind), "")
        .tags([
            Tag::custom(TagKind::custom("h"), [CHANNEL]),
            Tag::public_key(node),
        ])
        .custom_created_at(Timestamp::from_secs(at))
        .sign_with_keys(member)
        .expect("sign membership")
}

/// A channel message that `p`-tags the node — a mention, which is the only thing that addresses us.
fn mention(author: &Keys, node: PublicKey, at: u64) -> Event {
    EventBuilder::new(Kind::from(9u16), "can you take a job?")
        .tags([
            Tag::custom(TagKind::custom("h"), [CHANNEL]),
            Tag::public_key(node),
        ])
        .custom_created_at(Timestamp::from_secs(at))
        .sign_with_keys(author)
        .expect("sign mention")
}

fn config(url: &str) -> ParticipationConfig {
    ParticipationConfig {
        relays: vec![url.to_string()],
        probe_timeout_secs: 5,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

// ── Leg 1: invited ⇒ auto-subscribed; a mention lands in the inbox with an OWED entry ────────────

#[tokio::test]
async fn leg1_an_invite_admits_us_and_a_mention_becomes_an_owed_debt() {
    let (_relay, url) = start_relay().await;
    let node = Keys::generate();
    let member = Keys::generate();
    let store = SellerStore::open(temp_db("leg1")).expect("open store");

    let publisher = connect(&url, node.clone()).await;
    let mut participation = Participation::start(
        &config(&url),
        store.clone(),
        node.public_key(),
        &publisher,
        &carrier(&node),
    )
    .await
    .expect("start participation");

    assert_eq!(
        participation.access_states(),
        vec![(url.clone(), AccessState::Admitted)],
        "the relay served our carrier back, so it must classify as admitted"
    );

    // A member adds us. There is no handshake to answer — this notification IS the invite.
    let member_client = connect(&url, member.clone()).await;
    member_client
        .send_event(&membership(&member, 44100, node.public_key(), now()))
        .await
        .expect("publish 44100");

    assert!(
        participation
            .pump(Duration::from_secs(3))
            .await
            .expect("pump")
            >= 1,
        "the membership notification must reach the pump"
    );
    assert_eq!(
        store.joined_channels(&url).expect("channels"),
        [CHANNEL],
        "accepting an invite means subscribing to the channel it names"
    );

    // Now somebody talks to us in it.
    member_client
        .send_event(&mention(&member, node.public_key(), now()))
        .await
        .expect("publish mention");
    participation
        .pump(Duration::from_secs(3))
        .await
        .expect("pump");

    let owed = store.owed_responses().expect("owed");
    assert_eq!(owed.len(), 1, "the mention must land as exactly one debt");
    // The ledger row, spelled out — this is the artifact the acceptance block asks to see.
    assert_eq!(owed[0].relay_url, url);
    assert_eq!(owed[0].channel_id, CHANNEL);
    assert_eq!(owed[0].counterparty, member.public_key().to_hex());
    assert_eq!(owed[0].kind, 9);
    eprintln!(
        "OWED LEDGER ROW: event_id={} relay={} channel={} counterparty={} kind={} created_at={}",
        owed[0].event_id,
        owed[0].relay_url,
        owed[0].channel_id,
        owed[0].counterparty,
        owed[0].kind,
        owed[0].created_at_unix
    );

    participation.shutdown().await;
}

// ── Leg 2: removal ⇒ unsubscribed, and nothing re-subscribes it ──────────────────────────────────

#[tokio::test]
async fn leg2_a_removal_unsubscribes_and_does_not_loop() {
    let (_relay, url) = start_relay().await;
    let node = Keys::generate();
    let member = Keys::generate();
    let store = SellerStore::open(temp_db("leg2")).expect("open store");

    let publisher = connect(&url, node.clone()).await;
    let mut participation = Participation::start(
        &config(&url),
        store.clone(),
        node.public_key(),
        &publisher,
        &carrier(&node),
    )
    .await
    .expect("start participation");

    let member_client = connect(&url, member.clone()).await;
    member_client
        .send_event(&membership(&member, 44100, node.public_key(), now()))
        .await
        .expect("publish 44100");
    participation
        .pump(Duration::from_secs(3))
        .await
        .expect("pump");
    assert_eq!(store.joined_channels(&url).expect("channels"), [CHANNEL]);

    member_client
        .send_event(&membership(&member, 44101, node.public_key(), now() + 1))
        .await
        .expect("publish 44101");
    participation
        .pump(Duration::from_secs(3))
        .await
        .expect("pump");

    assert!(
        store.joined_channels(&url).expect("channels").is_empty(),
        "a removal must leave the resubscribe set — otherwise every restart re-asks for a channel \
         the relay has already refused, which is the loop this leg forbids"
    );

    // A message we can no longer be shown must not resurrect anything. Quiet, then re-check.
    member_client
        .send_event(&mention(&member, node.public_key(), now() + 2))
        .await
        .expect("publish post-removal mention");
    participation
        .pump(Duration::from_secs(2))
        .await
        .expect("pump");

    assert!(
        store.joined_channels(&url).expect("channels").is_empty(),
        "nothing may re-add the channel after a removal"
    );

    participation.shutdown().await;
}

// ── Leg 3: restart ⇒ cursors resume; a message sent while down arrives exactly once ──────────────

#[tokio::test]
async fn leg3_a_message_sent_while_down_is_ingested_exactly_once() {
    let (_relay, url) = start_relay().await;
    let node = Keys::generate();
    let member = Keys::generate();
    let db = temp_db("leg3");
    let store = SellerStore::open(&db).expect("open store");

    let publisher = connect(&url, node.clone()).await;
    let mut participation = Participation::start(
        &config(&url),
        store.clone(),
        node.public_key(),
        &publisher,
        &carrier(&node),
    )
    .await
    .expect("start participation");

    let member_client = connect(&url, member.clone()).await;
    member_client
        .send_event(&membership(&member, 44100, node.public_key(), now()))
        .await
        .expect("publish 44100");
    participation
        .pump(Duration::from_secs(3))
        .await
        .expect("pump");
    assert_eq!(store.joined_channels(&url).expect("channels"), [CHANNEL]);

    // Drop every socket without a clean protocol goodbye — the `kill -9` shape. All participation
    // state is already durable, which is what makes this indistinguishable from a clean stop.
    participation.shutdown().await;

    // Spoken to while we were down.
    let while_down = mention(&member, node.public_key(), now() + 1);
    member_client
        .send_event(&while_down)
        .await
        .expect("publish while down");

    // Restart against the SAME database.
    let reopened = SellerStore::open(&db).expect("reopen store");
    assert_eq!(
        reopened.joined_channels(&url).expect("channels"),
        [CHANNEL],
        "the channels we were in must survive the restart"
    );
    let mut restarted = Participation::start(
        &config(&url),
        reopened.clone(),
        node.public_key(),
        &publisher,
        &carrier(&node),
    )
    .await
    .expect("restart participation");

    restarted.pump(Duration::from_secs(3)).await.expect("pump");

    // NO LOSS: the cursor resumed from before the gap, so the message we missed arrived.
    let owed = reopened.owed_responses().expect("owed");
    assert_eq!(
        owed.len(),
        1,
        "the message sent while the node was down must be ingested — cursors resume, they do not \
         skip the gap"
    );
    assert_eq!(owed[0].event_id, while_down.id.to_hex());

    // NO DUPLICATE: pumping again re-delivers nothing new, and even if the relay replayed it, the
    // ledger is keyed on the event id.
    restarted.pump(Duration::from_secs(2)).await.expect("pump");
    assert_eq!(
        reopened.owed_responses().expect("owed").len(),
        1,
        "re-delivery after a reconnect is normal and must stay one debt"
    );

    restarted.shutdown().await;
}

// ── Leg 4: a denied relay gets ZERO frames afterwards ────────────────────────────────────────────

#[tokio::test]
async fn leg4_a_denied_relay_receives_no_wake_surface_and_is_never_polled() {
    // This fixture answers every REQ with EOSE and never serves an event: a healthy, authenticated
    // connection with no read admission. Silence here is not a bug being simulated — it is the
    // production behaviour that makes read-silence unclassifiable.
    let relay = PGateRelay::start(Duration::ZERO).await;
    let url = relay.url();
    let node = Keys::generate();
    let store = SellerStore::open(temp_db("leg4")).expect("open store");

    let publisher = connect(&url, node.clone()).await;
    let mut participation = Participation::start(
        &ParticipationConfig {
            relays: vec![url.clone()],
            probe_timeout_secs: 2,
        },
        store.clone(),
        node.public_key(),
        &publisher,
        &carrier(&node),
    )
    .await
    .expect("start participation");

    // Classified, not merely unused — the distinction the roster exists to make.
    match participation.access_states().as_slice() {
        [(seen, AccessState::Denied { .. })] => assert_eq!(seen, &url),
        other => panic!("a relay that never echoed must be classified denied, got {other:?}"),
    }
    assert!(
        !participation.is_live(),
        "a denied relay must not become a live participation surface"
    );

    let wake_reqs = |records: Vec<super::p_gate_relay_fixture::ReqRecord>| {
        records
            .into_iter()
            .filter(|record| record.subscription_id.starts_with("participation:"))
            .count()
    };

    // The probe's own read REQ is permitted — it is the one sanctioned frame to an unproven relay,
    // and it carries a generated id. What must NOT exist is any wake-surface subscription.
    assert_eq!(
        wake_reqs(relay.reqs().await),
        0,
        "the wake surface must never be opened on a denied relay"
    );

    // And nothing repeats: no timer, no heartbeat, no retry. Snapshot, pump, wait, re-count.
    //
    // The count to compare against is the one taken right after classification, not zero: the
    // publisher's socket and the probe's own read REQ are both legitimate, and both happened before
    // the relay was denied. What must not move is anything AFTER.
    let reqs_at_denial = relay.reqs().await.len();
    let sockets_at_denial = relay.connections();

    participation
        .pump(Duration::from_secs(2))
        .await
        .expect("pump");
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        wake_reqs(relay.reqs().await),
        0,
        "still no wake surface after pumping and waiting"
    );
    assert_eq!(
        relay.reqs().await.len(),
        reqs_at_denial,
        "not one further REQ may reach a denied relay — no polling, no heartbeat, no retry"
    );
    assert_eq!(
        relay.connections(),
        sockets_at_denial,
        "no reconnects to a denied relay — a reconnect is the only thing that increments this"
    );

    participation.shutdown().await;
}

// ── Leg 5: config absent ⇒ the node behaves exactly as before ────────────────────────────────────

#[tokio::test]
async fn leg5_an_absent_config_participates_nowhere_and_touches_nothing() {
    let (_relay, url) = start_relay().await;
    let node = Keys::generate();
    let store = SellerStore::open(temp_db("leg5")).expect("open store");
    let publisher = connect(&url, node.clone()).await;

    let participation = Participation::start(
        &ParticipationConfig::default(),
        store.clone(),
        node.public_key(),
        &publisher,
        &carrier(&node),
    )
    .await
    .expect("start participation");

    assert!(participation.access_states().is_empty());
    assert!(!participation.is_live());
    // No relay was probed, so the carrier was never published and no table was written.
    assert!(store.owed_responses().expect("owed").is_empty());
    assert!(store.joined_channels(&url).expect("channels").is_empty());

    participation.shutdown().await;
}
