//! LOCAL-RELAY integration tests for the buzz persona: drive [`SellerNode::start_buzz`] end-to-end
//! against an in-process NIP-01 relay (`nostr-relay-builder`) and assert on RELAY TRAFFIC — the
//! kind-0 persona is published + fetchable with its rate card, the clobber guard refuses a foreign
//! kind-0, and presence heartbeats flow while up and stop on clean shutdown.
//!
//! What a plain NIP-01 fixture CAN prove here: the publish/clobber/heartbeat LOGIC. What it can NOT
//! reproduce is the deployed relay's WS + Redis-TTL presence layer (`PRESENCE_SNAPSHOT` REQ, ~90s
//! expiry) — that is a stored-nowhere runtime behaviour of buzzrelay, verified live against the
//! deployed relay (acceptance item 2). Here presence is asserted as: heartbeats are published while
//! the node is up, and stop after a clean shutdown.

use super::*;
use crate::home::{self, BuzzConfig};
use crate::seller_node::buzz::{event_has_marker, BuzzError, MOBEE_MARKER_TAG, MOBEE_MARKER_VALUE, PRESENCE_KIND};
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::{
    Client, EventBuilder, Filter, Keys, Kind, Metadata, PublicKey, RelayPoolNotification, Tag,
};
use nostr_sdk::JsonUtil;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static IT_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_root(label: &str) -> std::path::PathBuf {
    let n = IT_SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("mobee-buzz-it-{label}-{}-{n}", std::process::id()))
}

async fn start_relay() -> (LocalRelay, String) {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("relay run");
    let url = relay.url().await.to_string();
    (relay, url)
}

async fn connect_client(relay_url: &str) -> Client {
    let client = Client::new(Keys::generate());
    client.add_relay(relay_url).await.expect("add relay");
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(5)).await;
    client
}

/// A reader bound to an ADMITTED key with NIP-42 auto-auth — the deployed relay gates reads, so a
/// live observer must authenticate as an admitted member (a random unadmitted key reads nothing).
async fn connect_admitted_observer(relay_url: &str, secret_hex: &str) -> Client {
    let keys = Keys::parse(secret_hex).expect("observer keys");
    let client = Client::new(keys);
    client.automatic_authentication(true);
    client.add_relay(relay_url).await.expect("add relay");
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(10)).await;
    client
}

/// Bootstrap a seller home wired with a `[buzz]` persona bound to `relay_url` (fast 1s heartbeat).
fn buzz_home(root: &std::path::Path, relay_url: &str) -> home::MobeeHome {
    let mut h = home::bootstrap(root).expect("bootstrap home");
    h.config.buzz = Some(BuzzConfig {
        relay_url: relay_url.to_string(),
        name: "Rocky".to_string(),
        about: Some("Rust reviewer".to_string()),
        rate_sats: Some(50),
        capabilities: vec!["code".to_string(), "test".to_string()],
        mint: None,
        heartbeat_secs: 1,
    });
    h
}

async fn fetch_kind0_about(observer: &Client, pubkey: PublicKey) -> Option<String> {
    let filter = Filter::new().author(pubkey).kind(Kind::Metadata).limit(1);
    let events = observer
        .fetch_events(filter, Duration::from_secs(5))
        .await
        .expect("fetch kind-0");
    let newest = events.into_iter().max_by_key(|e| e.created_at)?;
    let metadata = Metadata::from_json(&newest.content).ok()?;
    metadata.about
}

/// Count of ephemeral presence events (kind PRESENCE_KIND) observed from `author`, collected before
/// the node starts so none is missed.
fn spawn_presence_collector(client: &Client, author: PublicKey) -> Arc<Mutex<usize>> {
    let count = Arc::new(Mutex::new(0usize));
    let sink = count.clone();
    let mut notif = client.notifications();
    tokio::spawn(async move {
        while let Ok(n) = notif.recv().await {
            if let RelayPoolNotification::Event { event, .. } = n {
                if event.kind.as_u16() == PRESENCE_KIND && event.pubkey == author {
                    *sink.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                }
            }
        }
    });
    count
}

async fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ── Item 1 (logic): node boot publishes a kind-0 fetchable by pubkey carrying the rate card. ──
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boot_publishes_kind0_rate_card() {
    let (relay, relay_url) = start_relay().await;
    let home = buzz_home(&unique_root("kind0"), &relay_url);
    let node = SellerNode::open(home).await.expect("open node");
    let seller_pk = PublicKey::parse(node.seller_pubkey()).expect("seller pubkey");

    let observer = connect_client(&relay_url).await;

    let handle = node
        .start_buzz()
        .await
        .expect("start buzz")
        .expect("buzz configured");

    // The persona kind-0 is fetchable by pubkey and its about carries the rate card.
    let about = fetch_kind0_about(&observer, seller_pk).await.expect("kind-0 present");
    assert!(about.contains("50 sat/job"), "rate missing from about: {about}");
    assert!(about.contains("code, test"), "capabilities missing: {about}");
    assert!(about.contains("testnut"), "mint missing: {about}");
    assert!(about.contains("Rust reviewer"), "blurb missing: {about}");

    handle.shutdown().await;
    relay.shutdown();
}

// ── Item 3: a pre-existing FOREIGN kind-0 on the key ⇒ start_buzz refuses (no clobber). ──
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_kind0_refuses_clobber() {
    let (relay, relay_url) = start_relay().await;
    let root = unique_root("foreign");
    let home = buzz_home(&root, &relay_url);
    // Seed a FOREIGN kind-0 (no mobee marker) on the seller's own key, as if the key were already a
    // buzz inhabitant published by something else.
    let secret = home::read_secret_key_hex(&home).expect("secret");
    let keys = Keys::parse(&secret).expect("keys");
    let seeder = connect_client(&relay_url).await;
    let foreign = EventBuilder::metadata(&Metadata::new().name("someone-else").about("not mobee"))
        .sign_with_keys(&keys)
        .expect("sign foreign kind-0");
    seeder.send_event(&foreign).await.expect("seed foreign kind-0");

    let node = SellerNode::open(home).await.expect("open node");
    let result = node.start_buzz().await;
    match result {
        Err(BuzzError::Clobber(message)) => {
            assert!(message.contains("foreign") || message.contains("did not write"), "msg: {message}");
        }
        Err(other) => panic!("expected a clobber refusal, got a different error: {other}"),
        Ok(_) => panic!("expected a clobber refusal, but start_buzz succeeded"),
    }
    relay.shutdown();
}

// ── OUR OWN prior kind-0 (carries the marker) ⇒ start_buzz replaces it, no refusal. ──
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_marked_kind0_is_replaced() {
    let (relay, relay_url) = start_relay().await;
    let root = unique_root("ours");
    let home = buzz_home(&root, &relay_url);
    let secret = home::read_secret_key_hex(&home).expect("secret");
    let keys = Keys::parse(&secret).expect("keys");
    let seeder = connect_client(&relay_url).await;
    // Seed a kind-0 WITH our marker (a prior run's persona).
    let marker = Tag::parse([MOBEE_MARKER_TAG, MOBEE_MARKER_VALUE]).expect("marker tag");
    let ours = EventBuilder::metadata(&Metadata::new().name("Rocky").about("old card"))
        .tag(marker)
        .sign_with_keys(&keys)
        .expect("sign own kind-0");
    assert!(event_has_marker(&ours), "seeded event must carry the marker");
    seeder.send_event(&ours).await.expect("seed own kind-0");

    let node = SellerNode::open(home).await.expect("open node");
    let handle = node
        .start_buzz()
        .await
        .expect("start buzz over our own prior kind-0")
        .expect("buzz configured");
    handle.shutdown().await;
    relay.shutdown();
}

// ── LIVE deployed-relay harness (item 1): kind-0 round-trips against buzzrelay. ──
//
// Ignored by default (needs relay admission for the key). Run once the throwaway pubkey is admitted:
//   BUZZ_PERSONA_SECRET=<64-hex secret> \
//   BUZZ_LIVE_RELAY=wss://buzzrelay.orveth.dev \
//   cargo test -p maxplayer-core --no-default-features --features gateway,git-delivery,wallet --release \
//     -- --ignored --nocapture live_buzz_kind0_round_trip_against_deployed_relay
//
// It boots a node whose identity IS the throwaway key, publishes the persona, and fetches the
// kind-0 back from the deployed relay by pubkey, asserting the rate card. Presence (items 2-3) is
// confirmed separately against the deployed relay's PRESENCE_SNAPSHOT once its exact wire shape is
// pinned with the buzz keeper.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs relay admission for the throwaway key; run explicitly with env"]
async fn live_buzz_kind0_round_trip_against_deployed_relay() {
    let secret = match std::env::var("BUZZ_PERSONA_SECRET") {
        Ok(value) if value.trim().len() == 64 => value.trim().to_string(),
        _ => panic!("set BUZZ_PERSONA_SECRET to a 64-hex throwaway secret"),
    };
    let relay_url = std::env::var("BUZZ_LIVE_RELAY")
        .unwrap_or_else(|_| "wss://buzzrelay.orveth.dev".to_string());

    let root = unique_root("live");
    let home = {
        let mut h = buzz_home(&root, &relay_url);
        // Adopt the throwaway identity as the node's key.
        std::fs::write(&h.key_path, &secret).expect("write throwaway key");
        h.config.buzz.as_mut().unwrap().heartbeat_secs = 30;
        h
    };
    let keys = Keys::parse(&secret).expect("keys");
    let seller_pk = keys.public_key();

    let node = SellerNode::open(home).await.expect("open node");
    assert_eq!(node.seller_pubkey(), seller_pk.to_hex(), "node identity is the throwaway key");
    let handle = node
        .start_buzz()
        .await
        .expect("start buzz against the deployed relay")
        .expect("buzz configured");
    eprintln!("published kind-0 id={} pubkey={}", handle.kind0_event_id, seller_pk.to_hex());

    // The deployed relay gates reads behind NIP-42 — the observer authenticates as an admitted key.
    let observer_secret = std::env::var("BUZZ_OBSERVER_SECRET")
        .expect("set BUZZ_OBSERVER_SECRET to a 64-hex admitted observer secret");
    let observer = connect_admitted_observer(&relay_url, &observer_secret).await;
    let about = fetch_kind0_about(&observer, seller_pk)
        .await
        .expect("kind-0 fetchable from the deployed relay by pubkey");
    eprintln!("fetched kind-0 about: {about}");
    assert!(about.contains("sat/job"), "rate card missing from live kind-0: {about}");

    handle.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

// ── Presence: heartbeats flow while up, and STOP after a clean shutdown. ──
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_heartbeats_flow_then_stop_on_shutdown() {
    let (relay, relay_url) = start_relay().await;
    let home = buzz_home(&unique_root("presence"), &relay_url);
    let node = SellerNode::open(home).await.expect("open node");
    let seller_pk = PublicKey::parse(node.seller_pubkey()).expect("seller pubkey");

    // Observer subscribes to the seller's presence BEFORE the node starts.
    let observer = connect_client(&relay_url).await;
    observer
        .subscribe(
            Filter::new().kind(Kind::Custom(PRESENCE_KIND)).author(seller_pk),
            None,
        )
        .await
        .expect("subscribe presence");
    let count = spawn_presence_collector(&observer, seller_pk);

    let handle = node
        .start_buzz()
        .await
        .expect("start buzz")
        .expect("buzz configured");

    // At least two beats arrive within a few seconds (1s cadence + the immediate first beat).
    let flowed = wait_until(Duration::from_secs(6), || {
        *count.lock().unwrap_or_else(|e| e.into_inner()) >= 2
    })
    .await;
    assert!(flowed, "presence heartbeats must flow while the node is up");

    // Clean shutdown stops the heartbeat: after disconnecting, the count stops growing.
    handle.shutdown().await;
    let after = *count.lock().unwrap_or_else(|e| e.into_inner());
    tokio::time::sleep(Duration::from_secs(3)).await;
    let later = *count.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(after, later, "no presence beats must be published after a clean shutdown");

    relay.shutdown();
}

// ── LIVE deployed-relay presence (items 2-3): presence up/clear against buzzrelay. ──
//
// Ignored by default (needs BOTH demo keys admitted). Run once admitted:
//   BUZZ_PERSONA_SECRET=<persona 64-hex> \
//   BUZZ_OBSERVER_SECRET=<admitted observer 64-hex> \
//   BUZZ_LIVE_RELAY=wss://buzzrelay.orveth.dev \
//   cargo test -p maxplayer-core --no-default-features --features gateway,git-delivery,wallet --release \
//     -- --ignored --nocapture live_buzz_presence_against_deployed_relay
//
// Deployed presence (relay source, keeper:mobee-buzz): a kind-20001 `"online"` from the AUTHED
// connection registers presence keyed on the authed pubkey; the relay ignores tags and expires it
// on a ~60s TTL. The canonical operator snapshot is the HTTP `POST /query` (NIP-98) which
// synthesizes a RELAY-SIGNED online event — a WS `REQ` for a kind-20001 hits the DB (ephemerals are
// never stored) and always returns empty. This test uses the equivalent, simpler proof the keeper
// named: a live WS SUB `{kinds:[20001], authors:[persona]}` opened BEFORE a beat receives the raw
// SELF-signed heartbeat ephemeral each cycle — so it asserts presence FLOWS while up and STOPS on a
// clean shutdown. (`authors`, not `#p`: a `#p` filter has authors=None and falls through to the
// empty DB query.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs both demo keys admitted; run explicitly with env"]
async fn live_buzz_presence_against_deployed_relay() {
    let secret = match std::env::var("BUZZ_PERSONA_SECRET") {
        Ok(value) if value.trim().len() == 64 => value.trim().to_string(),
        _ => panic!("set BUZZ_PERSONA_SECRET to a 64-hex throwaway persona secret"),
    };
    let observer_secret = match std::env::var("BUZZ_OBSERVER_SECRET") {
        Ok(value) if value.trim().len() == 64 => value.trim().to_string(),
        _ => panic!("set BUZZ_OBSERVER_SECRET to a 64-hex admitted observer secret"),
    };
    let relay_url = std::env::var("BUZZ_LIVE_RELAY")
        .unwrap_or_else(|_| "wss://buzzrelay.orveth.dev".to_string());

    let root = unique_root("live-presence");
    let home = {
        let mut h = buzz_home(&root, &relay_url);
        std::fs::write(&h.key_path, &secret).expect("write throwaway key");
        // Fast beat so a heartbeat lands inside the test window.
        h.config.buzz.as_mut().unwrap().heartbeat_secs = 3;
        h
    };
    let seller_pk = Keys::parse(&secret).expect("keys").public_key();

    let node = SellerNode::open(home).await.expect("open node");

    // Admitted observer subscribes to the persona's presence BEFORE the node starts, so it catches
    // the live self-signed heartbeat ephemerals.
    let observer = connect_admitted_observer(&relay_url, &observer_secret).await;
    observer
        .subscribe(
            Filter::new().kind(Kind::Custom(PRESENCE_KIND)).author(seller_pk),
            None,
        )
        .await
        .expect("subscribe presence");
    let beats = spawn_presence_status_collector(&observer, seller_pk);

    let handle = node.start_buzz().await.expect("start buzz").expect("buzz configured");

    // Item 2 (online while up): at least one `"online"` heartbeat ephemeral flows.
    let flowed = wait_until(Duration::from_secs(12), || {
        !beats.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    })
    .await;
    assert!(flowed, "a live presence heartbeat must flow while the node is up");
    {
        let seen = beats.lock().unwrap_or_else(|e| e.into_inner());
        eprintln!("live presence beats while up: {} (first content={:?})", seen.len(), seen.first());
        assert!(
            seen.iter().any(|c| c == "online"),
            "presence heartbeat content must be the bare \"online\" status: {seen:?}"
        );
    }

    // Item 3 (clean disconnect stops presence): after shutdown, no further beats arrive.
    handle.shutdown().await;
    let at_shutdown = beats.lock().unwrap_or_else(|e| e.into_inner()).len();
    tokio::time::sleep(Duration::from_secs(8)).await;
    let later = beats.lock().unwrap_or_else(|e| e.into_inner()).len();
    eprintln!("presence beats at shutdown={at_shutdown}, after 8s={later}");
    assert_eq!(
        at_shutdown, later,
        "no presence beats may flow after a clean shutdown (heartbeat stopped + WS disconnected)"
    );

    observer.disconnect().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// Collect the CONTENT of each kind-20001 presence event observed from `author`.
fn spawn_presence_status_collector(client: &Client, author: PublicKey) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    let mut notif = client.notifications();
    tokio::spawn(async move {
        while let Ok(n) = notif.recv().await {
            if let RelayPoolNotification::Event { event, .. } = n {
                if event.kind.as_u16() == PRESENCE_KIND && event.pubkey == author {
                    sink.lock().unwrap_or_else(|e| e.into_inner()).push(event.content.clone());
                }
            }
        }
    });
    seen
}
