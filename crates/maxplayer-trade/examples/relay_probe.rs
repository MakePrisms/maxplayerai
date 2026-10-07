//! Non-tradable diagnostic events: no listing, bearer tokens, or preimages.
use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;
#[tokio::main]
async fn main() -> Result<()> {
    for url in ["wss://relay.ditto.pub", "wss://relay.damus.io"] {
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
            let stored = client
                .fetch_events(Filter::new().id(event.id), Duration::from_secs(8))
                .await;
            println!(
                "relay={url} kind={kind} event={} publish={publication:?} subscribed_delivery={observed} stored={}",
                event.id,
                match stored {
                    Ok(e) => e.iter().any(|e| e.id == event.id).to_string(),
                    Err(e) => format!("ERROR:{e}"),
                }
            );
        }
        reader.disconnect().await;
        client.disconnect().await;
    }
    Ok(())
}
