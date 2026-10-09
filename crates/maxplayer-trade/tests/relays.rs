use maxplayer_trade::{Asset, Leg, Status, lot_event, market::Market, status_event};
use nostr_relay_builder::prelude::*;
#[tokio::test]
async fn discovery_unions_lot_and_status_from_different_relays() {
    let a = LocalRelay::new(RelayBuilder::default());
    a.run().await.unwrap();
    let b = LocalRelay::new(RelayBuilder::default());
    b.run().await.unwrap();
    let ka = Keys::generate();
    let writer_a = Market::connect(ka.clone(), &[a.url().await.to_string()])
        .await
        .unwrap();
    let writer_b = Market::connect(ka.clone(), &[b.url().await.to_string()])
        .await
        .unwrap();
    let lot = lot_event(
        &ka,
        Leg {
            asset: Asset::new("http://127.0.0.1:1").unwrap(),
            net: 32,
        },
        Leg {
            asset: Asset::new("http://127.0.0.1:2").unwrap(),
            net: 24,
        },
    )
    .unwrap();
    let status = status_event(&ka, lot.id, 1, lot.id, Status::Available).unwrap();
    writer_a.publish(&lot).await.unwrap();
    writer_b.publish(&status).await.unwrap();
    let reader = Market::connect(
        Keys::generate(),
        &[a.url().await.to_string(), b.url().await.to_string()],
    )
    .await
    .unwrap();
    let found = reader.discover(Some(lot.id)).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, lot.id);
    let cancelled = status_event(&ka, lot.id, 2, status.id, Status::Cancelled).unwrap();
    writer_b.publish(&cancelled).await.unwrap();
    assert!(reader.discover(Some(lot.id)).await.unwrap().is_empty());
}
#[tokio::test]
async fn unreachable_is_not_empty_market() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    let empty = Market::connect(Keys::generate(), &[relay.url().await.to_string()])
        .await
        .unwrap();
    assert!(empty.discover(None).await.unwrap().is_empty());
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", socket.local_addr().unwrap());
    drop(socket);
    let dead = Market::connect(Keys::generate(), &[url]).await.unwrap();
    let error = dead.discover(None).await.unwrap_err().to_string();
    assert!(error.contains("relays unreachable"), "{error}");
}
/// Bob, 2026-10-09: relay.maxplayer.ai carries mint traffic (23410/23411) only. Through the real
/// CLI, the same host is accepted as `--mint-relay` and refused as `--relay`. Both checks run
/// before any socket opens, so this never contacts the real relay.
#[test]
fn production_relay_carries_mint_traffic_but_never_market_traffic() {
    for url in ["wss://relay.maxplayer.ai", "wss://RELAY.MAXPLAYER.AI/"] {
        let home = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
            .arg("--home")
            .arg(home.path())
            .args(["--relay", url, "--mint-relay", url, "discover"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("production relay forbidden"),
            "SAFETY: market traffic to {url} refused: {stderr}"
        );
        assert!(
            !stderr.contains("mint relay"),
            "mint traffic to {url} allowed: {stderr}"
        );
        let mint_only = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
            .arg("--home")
            .arg(home.path())
            .args(["--mint-relay", url, "status"])
            .output()
            .unwrap();
        assert!(mint_only.status.success(), "mint relay {url} accepted");
    }
}
#[tokio::test]
async fn production_relay_is_forbidden() {
    assert!(
        Market::connect(Keys::generate(), &["wss://relay.maxplayer.ai".into()])
            .await
            .is_err()
    );
}

use maxplayer_trade::{journal::Journal, market::Publication};
fn diagnostic(keys: &Keys, kind: u16) -> Event {
    EventBuilder::new(Kind::Custom(kind), uuid::Uuid::new_v4().to_string())
        .sign_with_keys(keys)
        .unwrap()
}
#[tokio::test]
async fn positive_ack_is_deduplicated_across_restart() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    let home = tempfile::tempdir().unwrap();
    let j = Journal::open(home.path()).await.unwrap();
    let keys = Keys::generate();
    let m = Market::connect(keys.clone(), &[relay.url().await.to_string()])
        .await
        .unwrap();
    let e = diagnostic(&keys, 3410);
    m.publish_durable(&j, &e).await.unwrap();
    for _ in 0..20 {
        m.publish_durable(&j, &e).await.unwrap();
    }
    drop(j);
    let j = Journal::open(home.path()).await.unwrap();
    m.retry_publications(&j).await.unwrap();
    let row = j
        .get::<Publication>("publication", &e.id.to_hex())
        .await
        .unwrap()
        .unwrap();
    assert!(
        row.relays.values().all(|d| d.ack && d.attempts == 1),
        "unchanged events must not be republished, even after reopen"
    );
}
#[tokio::test]
async fn rate_limited_relay_is_not_hammered_and_missing_ack_fails_closed() {
    let relay = LocalRelay::new(RelayBuilder::default().rate_limit(RateLimit {
        notes_per_minute: 1,
        ..Default::default()
    }));
    relay.run().await.unwrap();
    let home = tempfile::tempdir().unwrap();
    let j = Journal::open(home.path()).await.unwrap();
    let keys = Keys::generate();
    let m = Market::connect(keys.clone(), &[relay.url().await.to_string()])
        .await
        .unwrap();
    m.publish_durable(&j, &diagnostic(&keys, 3410))
        .await
        .unwrap();
    let refused = diagnostic(&keys, 3411);
    assert!(
        m.publish_durable(&j, &refused).await.is_err(),
        "SAFETY: missing ACK must not count as publication success"
    );
    for _ in 0..20 {
        assert!(m.publish_durable(&j, &refused).await.is_err());
    }
    let row = j
        .get::<Publication>("publication", &refused.id.to_hex())
        .await
        .unwrap()
        .unwrap();
    assert!(row.relays.values().all(|d| !d.ack && d.attempts == 1));
    let gate = j
        .get::<serde_json::Value>("relay_gate", &hex::encode(relay.url().await.to_string()))
        .await
        .unwrap()
        .unwrap();
    assert!(
        gate["next"].as_u64().unwrap() >= maxplayer_trade::coordinator::now() + 290,
        "rate-limited OK must impose the five-minute relay-wide cooldown, not ordinary retry delay"
    );
    let another = diagnostic(&keys, 3411);
    assert!(m.publish_durable(&j, &another).await.is_err());
    let row = j
        .get::<Publication>("publication", &another.id.to_hex())
        .await
        .unwrap()
        .unwrap();
    assert!(
        row.relays.values().all(|d| d.attempts == 0),
        "rate limit applies across new events too"
    );
}
#[tokio::test]
async fn ephemeral_ack_does_not_prove_stored_readback() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    let keys = Keys::generate();
    let m = Market::connect(keys.clone(), &[relay.url().await.to_string()])
        .await
        .unwrap();
    let e = diagnostic(&keys, 23412);
    m.publish(&e).await.unwrap();
    let fresh = Market::connect(Keys::generate(), &[relay.url().await.to_string()])
        .await
        .unwrap();
    assert!(
        fresh
            .query(Filter::new().id(e.id))
            .await
            .unwrap()
            .0
            .is_empty(),
        "ACK is not storage evidence"
    );
}

#[derive(Debug, Clone)]
struct BlockWrites(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl WritePolicy for BlockWrites {
    fn admit_event<'a>(
        &'a self,
        _: &'a Event,
        _: &'a std::net::SocketAddr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PolicyResult> + Send + 'a>> {
        Box::pin(async move {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            PolicyResult::Reject("test block".into())
        })
    }
}
#[tokio::test]
async fn blocked_relay_stays_blocked_across_events_and_restart_healthy_ack_suffices() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };
    let count = Arc::new(AtomicUsize::new(0));
    let blocked = LocalRelay::new(RelayBuilder::default().write_policy(BlockWrites(count.clone())));
    blocked.run().await.unwrap();
    let good = LocalRelay::new(RelayBuilder::default());
    good.run().await.unwrap();
    let keys = Keys::generate();
    let urls = [
        blocked.url().await.to_string(),
        good.url().await.to_string(),
    ];
    let m = Market::connect(keys.clone(), &urls).await.unwrap();
    let h = tempfile::tempdir().unwrap();
    let j = Journal::open(h.path()).await.unwrap();
    assert_eq!(
        m.publish_durable(&j, &diagnostic(&keys, 3410))
            .await
            .unwrap()
            .len(),
        1
    );
    drop(j);
    let j = Journal::open(h.path()).await.unwrap();
    for _ in 0..3 {
        assert_eq!(
            m.publish_durable(&j, &diagnostic(&keys, 3411))
                .await
                .unwrap()
                .len(),
            1
        );
    }
    assert_eq!(
        count.load(SeqCst),
        1,
        "blocked relay must not be hammered by other events or a restart"
    );
}
