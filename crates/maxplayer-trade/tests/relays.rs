use maxplayer_trade::{Asset, Leg, Status, lot_event, market::Market, status_event};
use nostr_relay_builder::prelude::*;
use nostr_sdk::prelude::*;
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
#[tokio::test]
async fn production_relay_is_forbidden() {
    assert!(
        Market::connect(Keys::generate(), &["wss://relay.maxplayer.ai".into()])
            .await
            .is_err()
    );
}
