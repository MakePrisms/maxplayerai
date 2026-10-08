//! Non-tradable diagnostic events: no listing, bearer tokens, or preimages.
use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;
#[tokio::main]
async fn main() -> Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    for url in [
        "wss://relay.ditto.pub",
        "wss://nos.lol",
        "wss://relay.primal.net",
        "wss://nostr-pub.wellorder.net",
        "wss://relay.nostr.band",
        "wss://offchain.pub",
    ] {
        tasks.spawn(probe(url));
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result? {
            eprintln!("probe error: {error}");
        }
    }
    Ok(())
}
async fn probe(url: &str) -> Result<()> {
    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.add_relay(url).await?;
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(15)).await;
    let reader = Client::new(Keys::generate());
    reader.add_relay(url).await?;
    reader.connect().await;
    reader.wait_for_connection(Duration::from_secs(15)).await;
    reader
        .subscribe(
            Filter::new().author(keys.public_key()).kinds([
                Kind::Custom(3410),
                Kind::Custom(3411),
                Kind::Custom(23412),
            ]),
            None,
        )
        .await?;
    let mut rx = reader.notifications();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut rows = Vec::new();
    for kind in [3410, 3411, 23412] {
        let body=serde_json::json!({"diagnostic":"maxplayer-trade relay capability probe; NOT a listing","trade_v":0}).to_string();
        let content = if kind == 23412 {
            nip44::encrypt(
                keys.secret_key(),
                &keys.public_key(),
                body,
                nip44::Version::V2,
            )?
        } else {
            body
        };
        let event = EventBuilder::new(Kind::Custom(kind), content)
            .tags([
                Tag::public_key(keys.public_key()),
                Tag::hashtag("maxplayer-trade-probe"),
            ])
            .sign_with_keys(&keys)?;
        let publication =
            tokio::time::timeout(Duration::from_secs(20), client.send_event(&event)).await;
        let observed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(RelayPoolNotification::Event { event: e, .. }) = rx.recv().await {
                    if e.id == event.id {
                        break;
                    }
                }
            }
        })
        .await
        .is_ok();
        let ack = match publication {
            Ok(Ok(ref output)) => !output.success.is_empty(),
            _ => false,
        };
        rows.push((kind, event.id, ack, observed, std::time::Instant::now()));
    }
    // Independent reader, no local publisher cache; each event ages at least 60 seconds.
    tokio::time::sleep(Duration::from_secs(60)).await;
    reader.disconnect().await;
    let stored_reader = Client::new(Keys::generate());
    stored_reader.add_relay(url).await?;
    stored_reader.connect().await;
    stored_reader
        .wait_for_connection(Duration::from_secs(10))
        .await;
    for (kind, id, ack, live, sent) in rows {
        let stored = stored_reader
            .fetch_events(Filter::new().id(id), Duration::from_secs(8))
            .await;
        println!(
            "{}",
            serde_json::json!({"relay":url,"kind":kind,"event_id":id,"ack":ack,"live":live,"stored":stored.as_ref().is_ok_and(|es|es.iter().any(|e|e.id==id)),"age_seconds":sent.elapsed().as_secs()})
        );
    }
    reader.disconnect().await;
    stored_reader.disconnect().await;
    client.disconnect().await;
    Ok(())
}
