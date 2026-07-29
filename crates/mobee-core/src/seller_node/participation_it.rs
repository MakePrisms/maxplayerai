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
    EventId, Filter,
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

// ── Leg 1b: the reader can actually authenticate ──────────────────────────────────────────────────

/// ★ The regression tooth for the defect the live run caught, which every other leg here passed over.
///
/// The read client was built as `Client::default()` — no signer. It set `automatic_authentication`,
/// but auto-auth has nothing to sign a NIP-42 challenge with, so it stayed **anonymous**. Against the
/// deployed relay that means `auth-required:` on every REQ: no carrier echo, no 44100, no mention,
/// nothing — for the whole life of the process.
///
/// Every fixture leg above passed anyway, because `LocalRelay` does not require NIP-42 and therefore
/// **cannot distinguish an anonymous reader from an authenticated one**. That is the shape to guard
/// against, so this asserts the property directly instead of hoping a fixture happens to demand it:
/// the reader must hold the node's identity, since admission is granted per-pubkey and an anonymous
/// or differently-keyed reader is unadmitted no matter how healthy the socket looks.
///
/// It is asserted on identity rather than on the fixture's per-REQ `authenticated` flag on purpose:
/// the probe's read REQ is not `#p`-pinned, so it may legitimately race the AUTH handshake. REQ/auth
/// ORDERING is a separate question with its own fixture (`p_gate_relay_fixture`, issue #189); this
/// leg is about whether an identity exists at all.
#[tokio::test]
async fn leg1b_the_reader_authenticates_as_the_node_not_as_nobody() {
    let (_relay, url) = start_relay().await;
    let node = Keys::generate();
    let store = SellerStore::open(temp_db("leg1b")).expect("open store");

    let publisher = connect(&url, node.clone()).await;
    let participation = Participation::start(
        &config(&url),
        store.clone(),
        node.public_key(),
        &publisher,
        &carrier(&node),
    )
    .await
    .expect("start participation");

    let identities = participation.reader_identities().await;
    assert_eq!(identities.len(), 1, "one admitted relay ⇒ one reader");
    let (seen_url, identity) = &identities[0];
    assert_eq!(seen_url, &url);
    assert_eq!(
        identity.as_ref().expect("the reader MUST have a signer — a signer-less reader is anonymous \
                                  and reads nothing on any relay that requires NIP-42"),
        &node.public_key(),
        "the reader must authenticate as the ADMITTED key; admission is per-pubkey, so any other \
         identity reads nothing even when the handshake succeeds"
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

// ── LIVE leg: the half no fixture can prove — the RELAY producing the 44100 ───────────────────────

/// Drive the participation surface against the **deployed** buzz relay, human-in-the-loop.
///
/// Everything above proves how the node RESPONDS to a membership notification. Nothing above proves
/// the relay PRODUCES one: in every fixture a member key publishes the 44100 directly. Only the
/// deployed relay turns a member's `kind-9000` into a relay-signed 44100, so only this leg closes
/// that gap.
///
/// Ignored by default and env-gated, following the existing live buzz-persona tests. It needs a
/// THROWAWAY key that the relay has admitted — never the real seller identity, which carries
/// reputation and money.
///
/// ```text
/// BUZZ_PERSONA_SECRET=<64-hex throwaway>  \
/// BUZZ_LIVE_CHANNEL=<channel uuid>        \
/// BUZZ_LIVE_RELAY=wss://buzzrelay.orveth.dev  \
/// PARTICIPATION_LIVE_LOG=/path/to/live.log    \
///   cargo test -p mobee-core --features acp,gateway,git-delivery,wallet --lib \
///     -- live_participation_against_deployed_relay --ignored --nocapture
/// ```
///
/// It waits for a human to fire the `9000` after it reports READY, so the window is generous.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a relay-admitted throwaway key and a human to fire the kind-9000; run explicitly with env"]
async fn live_participation_against_deployed_relay() {
    let secret = match std::env::var("BUZZ_PERSONA_SECRET") {
        Ok(value) if value.trim().len() == 64 => value.trim().to_string(),
        _ => panic!("set BUZZ_PERSONA_SECRET to a 64-hex throwaway secret (never the seller key)"),
    };
    // OPTIONAL, and a prefix is enough. The node is not supposed to know the channel in advance —
    // it learns it from the invite's `h` tag, which is the whole point of the 44100. Requiring an
    // exact uuid here would also turn a truncated id (`00fe7a48` for a full uuid) into a spurious
    // failure that looks like the invite never landed.
    let expect_channel = std::env::var("BUZZ_LIVE_CHANNEL").ok().filter(|v| !v.is_empty());
    let url = std::env::var("BUZZ_LIVE_RELAY")
        .unwrap_or_else(|_| "wss://buzzrelay.orveth.dev".to_string());
    let log = std::env::var("PARTICIPATION_LIVE_LOG")
        .expect("set PARTICIPATION_LIVE_LOG — evidence is asserted from a file, never from a pane");

    let node = Keys::parse(&secret).expect("parse throwaway secret");
    let me = node.public_key();
    let store = SellerStore::open(temp_db("live")).expect("open store");
    let mut evidence = Evidence::new(&log);
    evidence.note("L0 identity", &format!("pubkey={} relay={url} expect_channel={expect_channel:?}", me.to_hex()));

    // ── L2: access classified by positive probe against a real access-scoped relay ──────────────
    let publisher = connect(&url, node.clone()).await;
    let mut participation = Participation::start(
        &ParticipationConfig {
            relays: vec![url.clone()],
            probe_timeout_secs: 20,
        },
        store.clone(),
        me,
        &publisher,
        &carrier(&node),
    )
    .await
    .expect("start participation");

    let states = participation.access_states();
    evidence.note("L2 access states", &format!("{states:?}"));
    assert_eq!(
        states,
        vec![(url.clone(), AccessState::Admitted)],
        "the relay must serve our carrier back. Denied here means the admission has not landed — \
         which is a staging problem, not a code one, so stop rather than reinterpret it"
    );

    // ★ L3's BASELINE. Auto-subscribe is measured against this being empty: a pre-existing joined
    // row would make the 44100 look effective when nothing happened.
    let baseline = store.joined_channels(&url).expect("channels");
    evidence.note("L3 baseline (must be empty)", &format!("{baseline:?}"));
    assert!(
        baseline.is_empty(),
        "the node must hold no channel before the 9000 fires, or L3 proves nothing"
    );

    evidence.note(
        "READY",
        "subscribed and idle; membership filter live. Fire the kind-9000 add now.",
    );
    eprintln!("=== READY — fire the kind-9000 add for {} ===", me.to_hex());

    // ── L1 + L3: the relay-signed invite arrives and we auto-subscribe ──────────────────────────
    let joined = wait_for(Duration::from_secs(300), &mut participation, || {
        let channels = store.joined_channels(&url).expect("channels");
        (!channels.is_empty()).then_some(channels)
    })
    .await
    .expect("no 44100 arrived within the window — check the 9000 actually fired");
    evidence.note("L3 joined", &format!("{joined:?}"));
    let channel = joined.first().expect("a joined channel").clone();
    if let Some(expected) = &expect_channel {
        assert!(
            channel.starts_with(expected) || expected.starts_with(&channel),
            "the invite named channel {channel}, which does not match the expected {expected}"
        );
    }

    // Pull the 44100 back off the relay by the id we recorded, so the artifact is the relay's own
    // event rather than our belief about it.
    let source_id = store
        .joined_channel_source(&url, &channel)
        .expect("source id")
        .expect("the joined row must carry the event that admitted us");
    let reader = connect(&url, Keys::generate()).await;
    let invite = reader
        .fetch_events(
            Filter::new().id(EventId::from_hex(&source_id).expect("event id")),
            Duration::from_secs(15),
        )
        .await
        .expect("fetch the 44100")
        .first()
        .cloned()
        .expect("the 44100 must be fetchable from the relay by id");
    evidence.note("L1 the 44100, verbatim", &serde_json::to_string(&invite).expect("serialize the invite"));

    assert_eq!(invite.kind.as_u16(), 44100);
    // ★ The one thing no fixture can show: the invite was authored by a THIRD PARTY, not by us.
    // Confirming that author is specifically the RELAY's signing key needs that key from
    // keeper:buzz — so it is RECORDED here for cross-check, not asserted into a proof we cannot make.
    assert_ne!(
        invite.pubkey, me,
        "a self-authored 44100 would mean we invited ourselves — exactly the false pass this leg exists to rule out"
    );
    evidence.note(
        "L1 invite author (cross-check against the relay's signing key)",
        &invite.pubkey.to_hex(),
    );

    // ── L4: a mention lands as an owed debt ────────────────────────────────────────────────────
    eprintln!("=== joined {channel} — now post a kind-9 mentioning {} ===", me.to_hex());
    let owed = wait_for(Duration::from_secs(300), &mut participation, || {
        let owed = store.owed_responses().expect("owed");
        (!owed.is_empty()).then_some(owed)
    })
    .await
    .expect("no mention landed within the window");
    evidence.note("L4 owed ledger", &format!("{owed:?}"));
    assert_eq!(owed[0].channel_id, channel);
    assert_eq!(owed[0].relay_url, url);
    assert_ne!(owed[0].counterparty, me.to_hex(), "we must not be our own counterparty");

    evidence.note(
        "LIMITS",
        "one relay, one throwaway key, one junk channel. Human rate tier (60/min) — a \
         'rate-limited:' CLOSED here is throttle, not a defect. No money, no 340x signing, no real \
         identity touched. Restart/exactly-once (L5) and the denied-relay frame count (L6) are \
         driven separately per LIVE-RUN-PLAN.md.",
    );
    participation.shutdown().await;
}

/// Append-only evidence file. Every predicate's artifact is written here and asserted from here —
/// rendered terminal text cannot distinguish a line that was printed from one that merely looks it.
struct Evidence {
    path: std::path::PathBuf,
}

impl Evidence {
    fn new(path: &str) -> Self {
        let path = std::path::PathBuf::from(path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self { path }
    }

    fn note(&mut self, label: &str, body: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .expect("open evidence log");
        writeln!(file, "=== {label} ===\n{body}\n").expect("write evidence");
        eprintln!("=== {label} ===\n{body}");
    }
}

/// Pump until `probe` yields a value or the window closes. Returns `None` on timeout rather than
/// panicking, so the caller names what was expected.
async fn wait_for<T>(
    window: Duration,
    participation: &mut Participation,
    mut probe: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        participation
            .pump(Duration::from_secs(2))
            .await
            .expect("pump");
        if let Some(value) = probe() {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
    }
}
