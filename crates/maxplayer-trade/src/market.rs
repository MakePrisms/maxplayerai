use crate::{LOT, STATUS, Status, TRADE, journal::Journal, lifecycle, parse_lot};
use anyhow::{Context, Result, bail, ensure};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};
pub const DEFAULT_RELAYS: [&str; 3] = [
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://offchain.pub",
];
pub struct Market {
    pub clients: Vec<Client>,
    pub keys: Keys,
    urls: Vec<String>,
    pub inbox: tokio::sync::mpsc::UnboundedReceiver<Event>,
}
/// Per-event, per-relay receipts survive CLI restarts. An ACK is not a storage proof.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Delivery {
    pub ack: bool,
    pub attempts: u32,
    pub next: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Publication {
    pub event: Event,
    pub relays: BTreeMap<String, Delivery>,
}
#[derive(Default, Serialize, Deserialize)]
struct RelayGate {
    next: u64,
    blocked: bool,
}
#[derive(Debug)]
pub struct PublicationAbandoned;
impl std::fmt::Display for PublicationAbandoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "publication_abandoned: relay retry budget exhausted or blocked"
        )
    }
}
impl std::error::Error for PublicationAbandoned {}
const MAX_PUBLICATION_ATTEMPTS: u32 = 12;
fn retry_delay(attempt: u32) -> u64 {
    // Independent random jitter, without ever logging random material.
    let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]) % 6;
    (3u64.saturating_mul(1u64 << attempt.min(7))).min(300) + jitter
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
        let (tx, inbox) = tokio::sync::mpsc::unbounded_channel();
        let mut clients = vec![];
        for url in urls {
            let u = url::Url::parse(url)?;
            // `url` lowercases and percent-decodes the host but keeps a trailing dot, which
            // names the same server (`relay.maxplayer.ai.` / `relay.maxplayer.ai%2e`).
            ensure!(
                crate::transport::normalized_host(&u).as_deref() != Some("relay.maxplayer.ai"),
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
                // Bound each peer's burst without losing unrelated peers behind it.
                let mut rates = BTreeMap::new();
                while let Ok(n) = rx.recv().await {
                    if let RelayPoolNotification::Message {
                        message: RelayMessage::Event { event, .. },
                        ..
                    } = n
                    {
                        if event.kind == Kind::Custom(TRADE) {
                            let now = crate::coordinator::now();
                            rates.retain(|_, (start, _): &mut (u64, u32)| {
                                now < start.saturating_add(60)
                            });
                            let entry = rates.entry(event.pubkey).or_insert((now, 0));
                            entry.1 += 1;
                            if entry.1 <= 64 {
                                if tx.send(event.into_owned()).is_err() {
                                    break;
                                }
                            } else if entry.1 == 65 {
                                eprintln!("trade inbox: peer rate limit (64 messages/minute)");
                            }
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
            urls: urls.to_vec(),
            keys,
            inbox,
        })
    }
    /// Unjournaled one-shot for diagnostics/fixtures; trading uses publish_durable.
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
    /// Success requires at least one *recorded* positive ACK. Never infer success
    /// from timeout, live delivery, retry exhaustion, or a negative OK response.
    pub async fn publish_durable(&self, j: &Journal, e: &Event) -> Result<Vec<String>> {
        let id = e.id.to_hex();
        let mut row = j
            .get::<Publication>("publication", &id)
            .await?
            .unwrap_or(Publication {
                event: e.clone(),
                relays: BTreeMap::new(),
            });
        let now = crate::coordinator::now();
        let mut pending = Vec::new();
        for (url, client) in self.urls.iter().zip(&self.clients) {
            let gate = j
                .get::<RelayGate>("relay_gate", &hex::encode(url))
                .await?
                .unwrap_or_default();
            let d = row.relays.entry(url.clone()).or_default();
            if d.ack
                || d.attempts >= MAX_PUBLICATION_ATTEMPTS
                || d.next > now
                || gate.blocked
                || gate.next > now
            {
                continue;
            }
            // Persist the retry charge BEFORE sending; a crash cannot reset the budget.
            d.attempts += 1;
            d.next = now + retry_delay(d.attempts);
            let c = client.clone();
            let event = e.clone();
            let url = url.clone();
            pending.push((url, c, event));
        }
        j.put("publication", &id, &row).await?;
        j.put("publication_pending", &id, &id).await?;
        let mut tasks = tokio::task::JoinSet::new();
        for (url, c, event) in pending {
            tasks.spawn(async move {
                (
                    url,
                    tokio::time::timeout(Duration::from_secs(12), c.send_event(&event)).await,
                )
            });
        }
        while let Some(result) = tasks.join_next().await {
            let (url, result) = result?;
            let d = row
                .relays
                .get_mut(&url)
                .context("missing delivery intent")?;
            let (ack, reason) = match result {
                Ok(Ok(output)) => (
                    !output.success.is_empty(),
                    output
                        .failed
                        .values()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                Ok(Err(error)) => (false, error.to_string()),
                Err(_) => (false, "timeout".into()),
            };
            if ack {
                d.ack = true;
            } else {
                let lower = reason.to_ascii_lowercase();
                let mut gate = j
                    .get::<RelayGate>("relay_gate", &hex::encode(&url))
                    .await?
                    .unwrap_or_default();
                if lower.contains("blocked:") || lower.contains("banned:") {
                    gate.blocked = true;
                }
                gate.next = gate.next.max(if lower.contains("rate-limited:") {
                    now + 300 + retry_delay(d.attempts)
                } else {
                    d.next
                });
                j.put("relay_gate", &hex::encode(&url), &gate).await?;
            }
            j.put("publication", &id, &row).await?;
        }
        let accepted: Vec<String> = row
            .relays
            .iter()
            .filter(|(_, d)| d.ack)
            .map(|(u, _)| u.clone())
            .collect();
        let mut retryable = false;
        for (url, d) in &row.relays {
            let gate = j
                .get::<RelayGate>("relay_gate", &hex::encode(url))
                .await?
                .unwrap_or_default();
            retryable |= !d.ack && d.attempts < MAX_PUBLICATION_ATTEMPTS && !gate.blocked;
        }
        if !retryable {
            j.remove("publication_pending", &id).await?;
        }
        if accepted.is_empty() && !retryable {
            return Err(PublicationAbandoned.into());
        }
        ensure!(
            !accepted.is_empty(),
            "relays unreachable: no publication ACK (retained with bounded backoff)"
        );
        Ok(accepted)
    }
    /// Called by the serve/recover tick after active-swap recovery.
    pub async fn retry_publications(&self, j: &Journal) -> Result<()> {
        // One-time upgrade of old journals. Keep immutable receipts, scan only pending IDs.
        if j.get::<bool>("meta", "publication_index").await? != Some(true) {
            for row in j.all::<Publication>("publication").await? {
                j.put(
                    "publication_pending",
                    &row.event.id.to_hex(),
                    &row.event.id.to_hex(),
                )
                .await?;
            }
            j.put("meta", "publication_index", &true).await?;
        }
        for id in j.all::<String>("publication_pending").await? {
            if let Some(row) = j.get::<Publication>("publication", &id).await? {
                if row.event.created_at.as_secs().saturating_add(86400) < crate::coordinator::now()
                {
                    j.remove("publication_pending", &id).await?;
                    continue;
                }
                let _ = self.publish_durable(j, &row.event).await;
            } else {
                j.remove("publication_pending", &id).await?;
            }
        }
        Ok(())
    }
    pub async fn require_sent(
        &self,
        j: &Journal,
        peer: PublicKey,
        swap: &str,
        step: &str,
    ) -> Result<()> {
        let id = format!("{peer}_{swap}_{step}");
        let event = j
            .get::<Event>("outbox", &id)
            .await?
            .context("prerequisite publication missing")?;
        self.publish_durable(j, &event).await?;
        Ok(())
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
        self.publish_durable(j, &e).await?;
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
