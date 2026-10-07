use crate::{LOT, STATUS, Status, TRADE, journal::Journal, lifecycle, parse_lot};
use anyhow::{Result, bail, ensure};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};
pub const DEFAULT_RELAYS: [&str; 2] = ["wss://relay.ditto.pub", "wss://relay.damus.io"];
pub struct Market {
    pub clients: Vec<Client>,
    pub keys: Keys,
    pub inbox: tokio::sync::mpsc::Receiver<Event>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub trade_v: u8,
    pub request_id: String,
    pub swap_id: String,
    pub lot_id: String,
    pub quote_hash: String,
    pub step: String,
    pub body: serde_json::Value,
    pub exp: u64,
}
impl Market {
    pub async fn connect(keys: Keys, urls: &[String]) -> Result<Self> {
        ensure!(!urls.is_empty() && urls.len() <= 8, "relay count");
        let (tx, inbox) = tokio::sync::mpsc::channel(256);
        let mut clients = vec![];
        for url in urls {
            let u = url::Url::parse(url)?;
            ensure!(
                u.host_str() != Some("relay.maxplayer.ai"),
                "production relay forbidden"
            );
            ensure!(
                u.scheme() == "wss"
                    || u.scheme() == "ws"
                        && u.host_str()
                            .is_some_and(|h| h == "127.0.0.1" || h == "localhost"),
                "invalid relay URL"
            );
            let c = Client::new(keys.clone());
            c.automatic_authentication(true);
            c.add_relay(url).await?;
            let mut rx = c.notifications();
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Ok(n) = rx.recv().await {
                    if let RelayPoolNotification::Message {
                        message: RelayMessage::Event { event, .. },
                        ..
                    } = n
                    {
                        if event.kind == Kind::Custom(TRADE) {
                            let _ = tx.try_send(event.into_owned());
                        }
                    }
                }
            });
            c.connect().await;
            clients.push(c);
        }
        let mut tasks = tokio::task::JoinSet::new();
        for c in &clients {
            let c = c.clone();
            let pk = keys.public_key();
            tasks.spawn(async move {
                c.wait_for_connection(Duration::from_secs(10)).await;
                c.subscribe(
                    Filter::new()
                        .kind(Kind::Custom(TRADE))
                        .pubkey(pk)
                        .since(Timestamp::from(
                            crate::coordinator::now().saturating_sub(3600),
                        )),
                    None,
                )
                .await
            });
        }
        while tasks.join_next().await.is_some() {}
        Ok(Self {
            clients,
            keys,
            inbox,
        })
    }
    pub async fn publish(&self, e: &Event) -> Result<Vec<String>> {
        let mut tasks = tokio::task::JoinSet::new();
        for c in &self.clients {
            let c = c.clone();
            let e = e.clone();
            tasks.spawn(async move {
                tokio::time::timeout(Duration::from_secs(12), c.send_event(&e)).await
            });
        }
        let mut accepted = vec![];
        while let Some(r) = tasks.join_next().await {
            if let Ok(Ok(Ok(o))) = r {
                accepted.extend(o.success.iter().map(|u| u.to_string()));
            }
        }
        ensure!(
            !accepted.is_empty(),
            "relays unreachable: no publication ACK"
        );
        Ok(accepted)
    }
    pub async fn query(&self, f: Filter) -> Result<(Vec<Event>, usize)> {
        let mut tasks = tokio::task::JoinSet::new();
        for c in &self.clients {
            let c = c.clone();
            let f = f.clone();
            tasks.spawn(async move {
                let id = SubscriptionId::generate();
                let mut rx = c.notifications();
                c.subscribe_with_id(id.clone(), f, None).await?;
                let result = tokio::time::timeout(Duration::from_secs(10), async {
                    let mut events = vec![];
                    loop {
                        match rx.recv().await? {
                            RelayPoolNotification::Message {
                                message:
                                    RelayMessage::Event {
                                        subscription_id,
                                        event,
                                    },
                                ..
                            } if subscription_id.as_ref() == &id => {
                                ensure!(events.len() < 4096, "query bound exceeded");
                                events.push(event.into_owned());
                            }
                            RelayPoolNotification::Message {
                                message: RelayMessage::EndOfStoredEvents(s),
                                ..
                            } if s.as_ref() == &id => return Ok::<_, anyhow::Error>(events),
                            _ => {}
                        }
                    }
                })
                .await;
                c.unsubscribe(&id).await;
                result?
            });
        }
        let mut union = BTreeMap::new();
        let mut ok = 0;
        while let Some(r) = tasks.join_next().await {
            if let Ok(Ok(events)) = r {
                ok += 1;
                for e in events {
                    union.insert(e.id, e);
                }
            }
        }
        ensure!(ok > 0, "relays unreachable: no EOSE (timeout_or_partial)");
        Ok((union.into_values().collect(), self.clients.len() - ok))
    }
    pub async fn discover(&self, id: Option<EventId>) -> Result<Vec<Event>> {
        let mut f = Filter::new()
            .kind(Kind::Custom(LOT))
            .hashtag("maxplayer")
            .since(Timestamp::from(
                crate::coordinator::now().saturating_sub(86400),
            ))
            .limit(1024);
        if let Some(id) = id {
            f = f.id(id)
        }
        let (lots, partial) = self.query(f).await?;
        if partial > 0 {
            eprintln!(
                "timeout_or_partial: {partial} configured relay queries failed; showing verified union"
            );
        }
        let mut valid = vec![];
        for e in lots {
            if parse_lot(&e, crate::coordinator::now()).is_err() {
                continue;
            }
            let (history, _) = self
                .query(
                    Filter::new()
                        .kind(Kind::Custom(STATUS))
                        .event(e.id)
                        .author(e.pubkey)
                        .limit(257),
                )
                .await?;
            match lifecycle(&e, &history) {
                Ok(Status::Available) => valid.push(e),
                Ok(_) => {}
                Err(_) => eprintln!("invalid_or_quarantined lot {}", e.id),
            }
        }
        Ok(valid)
    }
    pub async fn send(&self, j: &Journal, peer: PublicKey, m: &Envelope) -> Result<()> {
        let id = format!("{}_{}_{}", peer, m.swap_id, m.step);
        let e = if let Some(e) = j.get::<Event>("outbox", &id).await? {
            e
        } else {
            let text = serde_json::to_string(m)?;
            ensure!(text.len() <= 48 * 1024, "encrypted payload too large");
            let content = nip44::encrypt(self.keys.secret_key(), &peer, text, nip44::Version::V2)?;
            let e = EventBuilder::new(Kind::Custom(TRADE), content)
                .tags([
                    Tag::public_key(peer),
                    Tag::hashtag("maxplayer"),
                    Tag::parse(["v", "1"])?,
                ])
                .sign_with_keys(&self.keys)?;
            j.put("outbox", &id, &e).await?;
            e
        };
        self.publish(&e).await?;
        Ok(())
    }
    pub fn decode(&self, e: &Event) -> Result<Envelope> {
        e.verify()?;
        ensure!(
            e.kind == Kind::Custom(TRADE) && e.content.len() <= 70 * 1024,
            "wrong/oversized message"
        );
        ensure!(
            crate::tag(e, "p")? == self.keys.public_key().to_hex()
                && crate::tag(e, "t")? == "maxplayer"
                && crate::tag(e, "v")? == "1",
            "routing mismatch"
        );
        let text = nip44::decrypt(self.keys.secret_key(), &e.pubkey, &e.content)?;
        ensure!(text.len() <= 48 * 1024, "oversized plaintext");
        let m: Envelope = serde_json::from_str(&text)?;
        ensure!(m.trade_v == 1, "version");
        uuid::Uuid::parse_str(&m.request_id)?;
        uuid::Uuid::parse_str(&m.swap_id)?;
        EventId::from_hex(&m.lot_id)?;
        if !["request", "quote", "first", "second", "claimed", "done"].contains(&m.step.as_str()) {
            bail!("unknown step")
        }
        Ok(m)
    }
}
