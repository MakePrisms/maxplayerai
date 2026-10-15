//! The sidecar on a relay that requires NIP-42 sign-in and gives no live events to a subscription
//! opened before sign-in (relay.maxplayer.ai). The wallet side here is a raw client that signs in
//! BEFORE it subscribes and publishes, so only the server's own subscription is under test.

#[path = "../../maxplayer-core/tests/support/auth_gated_relay.rs"]
mod auth_gated_relay;

use std::time::Duration;

use auth_gated_relay::{AuthRelay, AuthRelayOptions, Counters};
use cdk::util::unix_time;
use maxplayer_core::mint_wire::{Outcome, REQUEST_KIND, RESPONSE_KIND, Request, Response, op};
use maxplayer_core::relay_auth::{AuthWait, wait_for_nip42_auth};
use maxplayer_mint::home::mint_url;
use maxplayer_mint::{backend, server};
use nostr_sdk::nips::nip44::{self, Version};
use nostr_sdk::prelude::{
    Client, EventBuilder, Filter, Keys, Kind, RelayMessage, RelayPoolNotification, Tag, Timestamp,
};
use serde_json::Value;

/// Wait until at least `connections` sockets have opened, every one of them has signed in, and no
/// subscription is still deaf (sent before sign-in, not re-sent): the barrier the fixed sleeps
/// stood in for (#1156 review M4). A client sends its REQ on connect and its AUTH only after the
/// challenge, so a socket's pre-sign-in REQ is always counted before its sign-in is.
async fn all_hear(relay: &AuthRelay, connections: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let c = &relay.counters;
        let open = Counters::get(&c.connections);
        if open >= connections && Counters::get(&c.auths) >= open && Counters::get(&c.deaf) == 0 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "subscriptions never re-sent after sign-in: {c:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn start_mint(relay: &str) -> (Keys, tokio::task::JoinHandle<()>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let keys = Keys::generate();
    let mut seed = [0u8; 64];
    getrandom::fill(&mut seed).unwrap();
    let mint = backend::open(
        &dir.path().join("mint.sqlite"),
        &seed,
        &mint_url(&keys).unwrap(),
    )
    .await
    .expect("open mint");
    let server = server::Server::connect(mint, keys.clone(), &[relay.to_owned()], 100)
        .await
        .expect("server connect");
    let task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    (keys, task, dir)
}

/// One `info` request from a client that is signed in before it subscribes or publishes.
async fn info_after_sign_in(relay: &str, mint: &Keys, wait: Duration) -> Option<Value> {
    info(relay, mint, wait, true).await
}

async fn info(relay: &str, mint: &Keys, wait: Duration, sign_in: bool) -> Option<Value> {
    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.automatic_authentication(true);
    client.add_relay(relay).await.unwrap();
    let mut auth = client.relay(relay).await.unwrap().notifications();
    client.connect().await;
    if sign_in {
        let signed_in = wait_for_nip42_auth(&mut auth, Duration::from_secs(5)).await;
        assert_eq!(signed_in, Ok(AuthWait::Authenticated), "client sign-in");
    } else {
        client.wait_for_connection(Duration::from_secs(5)).await;
    }
    let mut notifications = client.notifications();
    client
        .subscribe(
            Filter::new()
                .kind(Kind::Custom(RESPONSE_KIND))
                .pubkey(keys.public_key())
                .since(Timestamp::now() - Duration::from_secs(10)),
            None,
        )
        .await
        .unwrap();
    let request = Request {
        v: 1,
        id: "auth-gated-info".into(),
        op: op::INFO.into(),
        body: Value::Null,
        exp: unix_time() + 30,
    };
    let content = nip44::encrypt(
        keys.secret_key(),
        &mint.public_key(),
        serde_json::to_string(&request).unwrap(),
        Version::V2,
    )
    .unwrap();
    let event = EventBuilder::new(Kind::Custom(REQUEST_KIND), content)
        .tag(Tag::public_key(mint.public_key()))
        .sign_with_keys(&keys)
        .unwrap();
    client
        .send_event(&event)
        .await
        .expect("relay accepts the signed-in request");
    let reply = tokio::time::timeout(wait, async {
        loop {
            let Ok(RelayPoolNotification::Message {
                message: RelayMessage::Event { event: reply, .. },
                ..
            }) = notifications.recv().await
            else {
                continue;
            };
            if reply.kind != Kind::Custom(RESPONSE_KIND) || reply.pubkey != mint.public_key() {
                continue;
            }
            let plain =
                nip44::decrypt(keys.secret_key(), &mint.public_key(), &reply.content).unwrap();
            let response: Response = serde_json::from_str(&plain).unwrap();
            if let Outcome::Ok(value) = response.outcome {
                return value;
            }
        }
    })
    .await
    .ok();
    let _ = client.shutdown().await;
    reply
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mint_answers_on_a_sign_in_required_relay() {
    let relay = AuthRelay::start(AuthRelayOptions::default()).await;
    let (mint, task, _dir) = start_mint(&relay.url).await;
    let info = info_after_sign_in(&relay.url, &mint, Duration::from_secs(5)).await;
    assert!(
        info.is_some(),
        "the mint never received the request: its subscription was opened before sign-in and \
         not re-sent ({:?})",
        relay.counters
    );
    // The scenario is real: the server's first REQ did go out before sign-in.
    assert!(
        Counters::get(&relay.counters.pre_auth_reqs) >= 1,
        "{:?}",
        relay.counters
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mint_answers_again_after_the_relay_drops_its_connection() {
    let relay = AuthRelay::start(AuthRelayOptions::default()).await;
    let (mint, task, _dir) = start_mint(&relay.url).await;
    assert!(
        info_after_sign_in(&relay.url, &mint, Duration::from_secs(5))
            .await
            .is_some()
    );
    let connections = Counters::get(&relay.counters.connections);
    relay.drop_connections();
    // nostr-sdk reconnects after its retry interval (10 s +/- 3 s) and re-sends the REQ on the new
    // socket BEFORE the new sign-in, so only the re-send after sign-in restores live delivery.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while Counters::get(&relay.counters.connections) <= connections {
        assert!(
            tokio::time::Instant::now() < deadline,
            "mint never reconnected"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The sidecar is the only client until the next info call.
    all_hear(&relay, connections + 1).await;
    assert!(
        info_after_sign_in(&relay.url, &mint, Duration::from_secs(5))
            .await
            .is_some(),
        "no answer after reconnect: {:?}",
        relay.counters
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mint_on_an_open_relay_is_unchanged() {
    let relay = AuthRelay::start(AuthRelayOptions {
        challenge: false,
        ..Default::default()
    })
    .await;
    let (mint, task, _dir) = start_mint(&relay.url).await;
    let started = std::time::Instant::now();
    let answer = info(&relay.url, &mint, Duration::from_secs(5), false).await;
    assert!(answer.is_some(), "{:?}", relay.counters);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "no sign-in wait on an open relay"
    );
    assert_eq!(Counters::get(&relay.counters.auths), 0);
    task.abort();
}
