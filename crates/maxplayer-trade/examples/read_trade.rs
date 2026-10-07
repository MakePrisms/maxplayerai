//! Independent, read-only Nostr reader. No trade coordinator, wallet or journal access.
use anyhow::{Context, Result, ensure};
use nostr_sdk::prelude::*;
use std::{collections::BTreeMap, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    let lot_id: EventId = std::env::args()
        .nth(1)
        .context("usage: read_trade LOT_ID")?
        .parse()?;
    let mut union = BTreeMap::new();
    for relay in ["wss://relay.ditto.pub", "wss://relay.damus.io"] {
        let client = Client::default();
        client.add_relay(relay).await?;
        client.connect().await;
        client.wait_for_connection(Duration::from_secs(15)).await;
        for filter in [
            Filter::new().kind(Kind::Custom(3410)).id(lot_id),
            Filter::new().kind(Kind::Custom(3411)).event(lot_id),
        ] {
            match client.fetch_events(filter, Duration::from_secs(15)).await {
                Ok(events) => {
                    for event in events {
                        event.verify()?;
                        println!(
                            "{}",
                            serde_json::json!({"relay":relay,"id":event.id,"kind":event.kind.as_u16(),"signature":"valid"})
                        );
                        union.insert(event.id, event);
                    }
                }
                Err(error) => eprintln!("{relay}: {error}"),
            }
        }
        client.disconnect().await;
    }
    let lot = union.get(&lot_id).context("lot not read back")?;
    let mut statuses = union
        .values()
        .filter(|e| e.kind == Kind::Custom(3411))
        .map(|e| Ok((e, serde_json::from_str::<serde_json::Value>(&e.content)?)))
        .collect::<Result<Vec<_>>>()?;
    statuses.sort_by_key(|(_, s)| s["seq"].as_u64().unwrap_or(0));
    let mut previous = lot_id.to_hex();
    let mut last = String::new();
    for (index, (event, body)) in statuses.iter().enumerate() {
        ensure!(event.pubkey == lot.pubkey, "foreign status author");
        ensure!(body["lot_id"] == lot_id.to_hex(), "wrong lot");
        ensure!(body["seq"].as_u64() == Some(index as u64 + 1), "gap/fork");
        ensure!(body["prev"] == previous, "broken chain");
        ensure!(last != "sold" && last != "cancelled", "terminal reopened");
        previous = event.id.to_hex();
        last = body["status"]
            .as_str()
            .context("status missing")?
            .to_owned();
        ensure!(
            ["available", "sold", "cancelled"].contains(&last.as_str()),
            "invalid status"
        );
    }
    ensure!(last == "sold", "trade not independently read back as sold");
    println!(
        "{}",
        serde_json::json!({"lot_id":lot_id,"status":"sold","status_event_ids":statuses.iter().map(|(e,_)|e.id.to_hex()).collect::<Vec<_>>(),"chain":"valid"})
    );
    Ok(())
}
