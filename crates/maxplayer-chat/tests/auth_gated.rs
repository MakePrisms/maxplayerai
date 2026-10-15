//! Chat on a relay that requires NIP-42 sign-in and gives no live events to a subscription opened
//! before sign-in (relay.maxplayer.ai: buzz with `BUZZ_OPEN_READ`), the failure mode of #1156.
//!
//! The acceptance fixture models a relay that CLOSEs a pre-sign-in REQ with `restricted:`, which a
//! client notices. This one gives that REQ stored history and EOSE and then silence, which a client
//! does not notice. For chat's kind-1059 `#p` filter buzz itself CLOSEs `restricted:` (its p-gate
//! runs before the open-read history path), so this fixture is the stricter guard: any REQ sent
//! before sign-in shows up as a missed live message. Each test separates the two deliveries:
//! history arrives through the paged catch-up that runs before `healthy`, live delivery is a
//! message published after `healthy` that arrives with the checkpoint unchanged (the 1-hour probe
//! never runs, so no re-read can carry it).

#[path = "../../maxplayer-core/tests/support/auth_gated_relay.rs"]
mod auth_gated_relay;

use auth_gated_relay::{AuthRelay, AuthRelayOptions, Counters};
use maxplayer_chat::{Home, WatchOptions, envelope, watch_with_options};
use maxplayer_core::private_content::transport;
use maxplayer_core::relay_auth::{AuthWait, wait_for_nip42_auth};
use nostr_sdk::prelude::*;
use std::time::Duration;
use tokio::task::JoinHandle;

fn home() -> (tempfile::TempDir, Home) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::open(dir.path().join("home")).unwrap();
    (dir, home)
}

/// `a` and `b` approve each other; `a` calls `b` "B".
fn pair() -> ((tempfile::TempDir, Home), (tempfile::TempDir, Home)) {
    let a = home();
    let b = home();
    a.1.add_peer(&b.1.keys.public_key().to_hex(), "B").unwrap();
    b.1.add_peer(&a.1.keys.public_key().to_hex(), "A").unwrap();
    (a, b)
}

/// Only setup (connect, sign-in, subscribe, catch-up) and reconnects can read history: the
/// liveness probe, the only other re-read, is an hour away.
fn options(backoff_unit: Duration) -> WatchOptions {
    WatchOptions {
        max_failures: 0,
        cycle_timeout: Duration::from_secs(10),
        liveness_interval: Duration::from_secs(3600),
        liveness_timeout: Duration::from_secs(15),
        refresh_interval: Duration::from_millis(100),
        backoff_unit,
    }
}

fn watcher(h: &Home, url: &str, options: WatchOptions) -> JoinHandle<()> {
    let path = h.root.clone();
    let url = url.to_string();
    tokio::spawn(async move {
        let _ = watch_with_options(&Home::open(path).unwrap(), &url, vec![], options).await;
    })
}

async fn wait_state(h: &Home, state: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !h.status().is_ok_and(|s| s.state == state) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("watch never became {state}: {:?}", h.status()));
}

async fn wait_entries(h: &Home, n: usize, relay: &AuthRelay, what: &str) {
    let arrived = tokio::time::timeout(Duration::from_secs(8), async {
        while h.entries().unwrap().len() < n {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        arrived.is_ok(),
        "{what}: never delivered; relay counters {:?}",
        relay.counters
    );
}

/// Wait until a re-run of setup would checkpoint a later second than the current cursor, so an
/// unchanged cursor afterwards proves no setup ran.
async fn past_checkpoint(h: &Home) -> u64 {
    let cursor = h.cursor().unwrap();
    while Timestamp::now().as_secs() <= cursor {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cursor
}

/// Publish from a client that signs in before it publishes, so only the watcher is under test.
async fn publish(relay: &str, from: &Keys, to: &Home, text: &str) {
    let event = transport::wrap(from, to.keys.public_key(), envelope(text).unwrap())
        .await
        .unwrap();
    let client = Client::new(from.clone());
    client.automatic_authentication(true);
    client.add_relay(relay).await.unwrap();
    let mut auth = client.relay(relay).await.unwrap().notifications();
    client.connect().await;
    assert_eq!(
        wait_for_nip42_auth(&mut auth, Duration::from_secs(5)).await,
        Ok(AuthWait::Authenticated)
    );
    let output = client.send_event(&event).await.unwrap();
    assert!(output.failed.is_empty(), "publish refused: {output:?}");
    client.shutdown().await;
}

/// The status file as the stopped watcher left it (`status()` reports any stopped watcher as
/// "watch not running", hiding its last error).
fn stored(h: &Home) -> maxplayer_chat::WatchStatus {
    serde_json::from_str(&std::fs::read_to_string(h.root.join("chat/status.json")).unwrap())
        .unwrap()
}

fn assert_never_deaf(relay: &AuthRelay) {
    let c = &relay.counters;
    assert_eq!(
        Counters::get(&c.pre_auth_reqs),
        0,
        "REQ before sign-in: {c:?}"
    );
    assert_eq!(
        Counters::get(&c.dropped_live),
        0,
        "live event withheld: {c:?}"
    );
    assert_eq!(Counters::get(&c.deaf), 0, "deaf subscription open: {c:?}");
}

#[tokio::test]
async fn first_sign_in_catches_up_history_then_receives_live() {
    let relay = AuthRelay::start(AuthRelayOptions::default()).await;
    let ((_a, a), (_b, b)) = pair();
    publish(&relay.url, &a.keys, &b, "stored before watch").await;

    let watch = watcher(&b, &relay.url, options(Duration::from_millis(200)));
    wait_state(&b, "healthy").await;
    // History: complete before `healthy` is written.
    assert_eq!(b.entries().unwrap().len(), 1, "history catch-up");
    assert_eq!(b.entries().unwrap()[0].text, "stored before watch");

    // Live: published after setup, delivered with no setup re-run (cursor unchanged).
    let cursor = past_checkpoint(&b).await;
    publish(&relay.url, &a.keys, &b, "live after sign-in").await;
    wait_entries(&b, 2, &relay, "live message after first sign-in").await;
    assert_eq!(b.entries().unwrap()[1].text, "live after sign-in");
    assert_eq!(
        b.cursor().unwrap(),
        cursor,
        "delivered by a re-read, not live"
    );
    assert_eq!(b.status().unwrap().consecutive_failures, 0);
    assert!(Counters::get(&relay.counters.post_auth_reqs) >= 1);
    assert_never_deaf(&relay);
    watch.abort();
}

#[tokio::test]
async fn reconnect_signs_in_again_before_subscribing() {
    let relay = AuthRelay::start(AuthRelayOptions::default()).await;
    let ((_a, a), (_b, b)) = pair();
    // ~3 s backoff: the outage message is accepted well before the watcher reconnects.
    let watch = watcher(&b, &relay.url, options(Duration::from_secs(3)));
    wait_state(&b, "healthy").await;
    let connections = Counters::get(&relay.counters.connections);

    relay.drop_connections();
    wait_state(&b, "reconnecting").await;
    let sent = a
        .send_outcome(&relay.url, "B", "sent during outage")
        .await
        .unwrap();
    assert_eq!(sent.status, "accepted", "{sent:?}");
    assert_eq!(sent.attempts, 1, "{sent:?}");
    assert_eq!(
        b.status().unwrap().state,
        "reconnecting",
        "watch reconnected before the outage message was stored; test timing is invalid"
    );

    // History after the new sign-in: caught up before `healthy`.
    wait_state(&b, "healthy").await;
    assert_eq!(b.entries().unwrap().len(), 1, "outage message via catch-up");
    assert!(Counters::get(&relay.counters.connections) >= connections + 2);

    // Live on the new socket.
    let cursor = past_checkpoint(&b).await;
    let sent = a
        .send_outcome(&relay.url, "B", "live after reconnect")
        .await
        .unwrap();
    assert_eq!(sent.status, "accepted", "{sent:?}");
    wait_entries(&b, 2, &relay, "live message after reconnect").await;
    assert_eq!(b.entries().unwrap()[1].text, "live after reconnect");
    assert_eq!(
        b.cursor().unwrap(),
        cursor,
        "delivered by a re-read, not live"
    );
    // Every socket signed in: watch twice, send twice.
    assert!(
        Counters::get(&relay.counters.auths) >= 4,
        "{:?}",
        relay.counters
    );
    assert_never_deaf(&relay);
    watch.abort();
}

#[tokio::test]
async fn refused_sign_in_never_subscribes_or_publishes() {
    let relay = AuthRelay::start(AuthRelayOptions {
        refuse_auth: true,
        ..AuthRelayOptions::default()
    })
    .await;
    let ((_a, a), (_b, b)) = pair();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        watch_with_options(
            &b,
            &relay.url,
            vec![],
            WatchOptions {
                max_failures: 2,
                backoff_unit: Duration::from_millis(50),
                ..options(Duration::from_millis(50))
            },
        ),
    )
    .await
    .expect("refused sign-in must end the bounded cycles");
    assert!(result.is_err());
    let status = stored(&b);
    assert!(status.last_healthy_at.is_none(), "{status:?}");
    assert_eq!(status.consecutive_failures, 2);
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("auth: NIP-42 rejected"),
        "{status:?}"
    );

    let sent = a
        .send_outcome(&relay.url, "B", "never published")
        .await
        .unwrap();
    assert_eq!(sent.status, "failed", "{sent:?}");
    assert_eq!(sent.exit_code(), 1);
    assert_eq!(
        sent.attempts, 3,
        "connect/auth failures are retried: {sent:?}"
    );
    assert!(
        sent.error
            .as_deref()
            .unwrap_or_default()
            .contains("auth: NIP-42 rejected")
    );
    assert!(
        a.entries().unwrap().is_empty(),
        "no outgoing transcript entry"
    );

    let c = &relay.counters;
    assert!(Counters::get(&c.connections) >= 5, "{c:?}");
    assert_eq!(Counters::get(&c.auths), 0, "{c:?}");
    assert_eq!(
        Counters::get(&c.pre_auth_reqs) + Counters::get(&c.post_auth_reqs),
        0,
        "{c:?}"
    );
}

/// Unchanged: Chat requires a relay that challenges on connect, so a relay that never challenges
/// fails closed (no REQ, never healthy) instead of serving an unauthenticated subscription.
#[tokio::test]
async fn relay_without_challenge_still_fails_closed() {
    let relay = AuthRelay::start(AuthRelayOptions {
        challenge: false,
        ..AuthRelayOptions::default()
    })
    .await;
    let (_b, b) = home();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        watch_with_options(
            &b,
            &relay.url,
            vec![],
            WatchOptions {
                max_failures: 1,
                cycle_timeout: Duration::from_millis(1500),
                ..options(Duration::from_millis(50))
            },
        ),
    )
    .await
    .expect("bounded by the cycle deadline");
    assert!(result.is_err());
    let status = stored(&b);
    assert!(status.last_healthy_at.is_none(), "{status:?}");
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .starts_with("connect/auth"),
        "{status:?}"
    );
    let c = &relay.counters;
    assert_eq!(Counters::get(&c.connections), 1, "{c:?}");
    assert_eq!(
        Counters::get(&c.pre_auth_reqs) + Counters::get(&c.post_auth_reqs),
        0,
        "{c:?}"
    );
}
