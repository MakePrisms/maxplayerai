//! The market's TRADE inbox on a relay that requires NIP-42 sign-in and gives no live events to a
//! subscription opened before sign-in (relay.maxplayer.ai behaviour).

#[path = "../../maxplayer-core/tests/support/auth_gated_relay.rs"]
mod auth_gated_relay;

use std::time::Duration;

use auth_gated_relay::{AuthRelay, AuthRelayOptions, Counters};
use maxplayer_trade::journal::Journal;
use maxplayer_trade::market::Market;
use maxplayer_trade::{LOT, TRADE};
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::{Event, EventBuilder, Keys, Kind, Tag};

/// `from` publishes a TRADE message to `to`; true once `to`'s inbox receives it.
async fn delivered(from: &Market, to: &mut Market, text: &str) -> bool {
    let event = EventBuilder::new(Kind::Custom(TRADE), text)
        .tag(Tag::public_key(to.keys.public_key()))
        .sign_with_keys(&from.keys)
        .unwrap();
    from.publish(&event)
        .await
        .expect("relay accepts the message");
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(got) = to.inbox.recv().await {
            if got.id == event.id {
                return;
            }
        }
    })
    .await
    .is_ok()
}

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

async fn markets(relay: &AuthRelay) -> (Market, Market) {
    let urls = [relay.url.clone()];
    let a = Market::connect(Keys::generate(), &urls).await.unwrap();
    let b = Market::connect(Keys::generate(), &urls).await.unwrap();
    // Both inboxes re-subscribed after sign-in.
    all_hear(relay, 2).await;
    (a, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_inbox_receives_on_a_sign_in_required_relay() {
    let relay = AuthRelay::start(AuthRelayOptions::default()).await;
    let (a, mut b) = markets(&relay).await;
    assert!(
        delivered(&a, &mut b, "hello").await,
        "the inbox subscription went out before sign-in and was never re-sent: {:?}",
        relay.counters
    );
    assert!(
        Counters::get(&relay.counters.pre_auth_reqs) >= 1,
        "{:?}",
        relay.counters
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_inbox_receives_again_after_a_reconnect() {
    let relay = AuthRelay::start(AuthRelayOptions::default()).await;
    let (a, mut b) = markets(&relay).await;
    assert!(
        delivered(&a, &mut b, "before").await,
        "{:?}",
        relay.counters
    );
    let connections = Counters::get(&relay.counters.connections);
    relay.drop_connections();
    // nostr-sdk reconnects after 10 s +/- 3 s and re-sends the REQ before the new sign-in.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    while Counters::get(&relay.counters.connections) < connections + 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "markets never reconnected"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    all_hear(&relay, connections + 2).await;
    assert!(delivered(&a, &mut b, "after").await, "{:?}", relay.counters);
}

fn lot(keys: &Keys) -> Event {
    EventBuilder::new(Kind::Custom(LOT), uuid::Uuid::new_v4().to_string())
        .sign_with_keys(keys)
        .unwrap()
}

/// relay.maxplayer.ai as the first market relay (relay-v5, #1246): a member's lot is stored there
/// (the EVENT refused before sign-in is re-sent after it). A non-member's sign-in is refused the
/// way buzz refuses it (#1228), and its lot still lands on the public fallback, so listing never
/// depends on relay membership.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lots_reach_a_members_only_relay_and_non_members_fall_back() {
    let member = Keys::generate();
    let gated = AuthRelay::start(AuthRelayOptions {
        members: Some([member.public_key(); 2]),
        buzz_refusal: true,
        ..Default::default()
    })
    .await;
    let open = LocalRelay::new(RelayBuilder::default());
    open.run().await.unwrap();
    let open_url = open.url().await.to_string();
    let home = tempfile::tempdir().unwrap();
    let j = Journal::open(home.path()).await.unwrap();

    let m = Market::connect(member.clone(), &[gated.url.clone()])
        .await
        .unwrap();
    let stored = m.publish_durable(&j, &lot(&member)).await.unwrap();
    assert_eq!(stored, vec![gated.url.clone()], "{:?}", gated.counters);

    let stranger = Keys::generate();
    let s = Market::connect(stranger.clone(), &[gated.url.clone(), open_url.clone()])
        .await
        .unwrap();
    let stored = s.publish_durable(&j, &lot(&stranger)).await.unwrap();
    assert_eq!(stored, vec![open_url], "{:?}", gated.counters);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_inbox_on_an_open_relay_is_unchanged() {
    let relay = AuthRelay::start(AuthRelayOptions {
        challenge: false,
        ..Default::default()
    })
    .await;
    let urls = [relay.url.clone()];
    let a = Market::connect(Keys::generate(), &urls).await.unwrap();
    let mut b = Market::connect(Keys::generate(), &urls).await.unwrap();
    let started = std::time::Instant::now();
    assert!(delivered(&a, &mut b, "open").await);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(Counters::get(&relay.counters.auths), 0);
}
