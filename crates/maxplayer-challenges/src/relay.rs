//! Public relay transport. Read failures never masquerade as an empty membership set.
use maxplayer_core::{
    gateway::EventDraft,
    home::{self, MaxplayerHome},
};
use nostr_sdk::prelude::*;
use std::{collections::BTreeMap, time::Duration};

pub const DEFAULT_RELAYS: [&str; 3] = [
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://offchain.pub",
];
const TIMEOUT: Duration = Duration::from_secs(10);
pub struct Relays {
    urls: Vec<String>,
}
impl Relays {
    pub fn new(urls: &[String]) -> Result<Self, String> {
        let urls = if urls.is_empty() {
            DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
        } else {
            urls.to_vec()
        };
        let mut normalized = Vec::new();
        for value in urls {
            let u = url::Url::parse(&value).map_err(|_| "invalid relay URL")?;
            let host = u
                .host_str()
                .ok_or("relay URL needs host")?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if host == "relay.maxplayer.ai" {
                return Err("production relay forbidden for challenges".into());
            }
            let loopback = host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|a| a.is_loopback());
            if !(u.scheme() == "wss" || u.scheme() == "ws" && loopback)
                || !u.username().is_empty()
                || u.password().is_some()
                || u.fragment().is_some()
            {
                return Err("relay requires wss:// (ws:// only on loopback), without credentials or fragment".into());
            }
            let value = u.to_string();
            if !normalized.contains(&value) {
                normalized.push(value);
            }
        }
        Ok(Self { urls: normalized })
    }
    async fn client(home: &MaxplayerHome, url: &str) -> Result<Client, String> {
        let client = Client::new(keys(home)?);
        client.automatic_authentication(true);
        client
            .add_relay(url)
            .await
            .map_err(|_| "could not configure relay")?;
        for relay in client.relays().await.into_values() {
            maxplayer_core::relay_auth::resubscribe_on_auth(relay, "maxplayer-challenges");
        }
        client.connect().await;
        Ok(client)
    }
    pub async fn fetch(&self, home: &MaxplayerHome, filter: Filter) -> Result<Vec<Event>, String> {
        let mut union = BTreeMap::new();
        let mut failed = Vec::new();
        for url in &self.urls {
            let client = Self::client(home, url).await?;
            let result = tokio::time::timeout(TIMEOUT, async {
                let mut rx = client.notifications();
                let id = SubscriptionId::generate();
                client
                    .subscribe_with_id(id.clone(), filter.clone(), None)
                    .await
                    .map_err(|_| "subscription failed")?;
                let mut events = Vec::new();
                loop {
                    match rx.recv().await.map_err(|_| "notification stream ended")? {
                        RelayPoolNotification::Message {
                            message:
                                RelayMessage::Event {
                                    subscription_id,
                                    event,
                                },
                            ..
                        } if subscription_id.as_ref() == &id => {
                            if event.verify().is_ok()
                                && filter.match_event(&event, Default::default())
                            {
                                events.push(event.into_owned());
                            }
                        }
                        RelayPoolNotification::Message {
                            message: RelayMessage::EndOfStoredEvents(subscription_id),
                            ..
                        } if subscription_id.as_ref() == &id => return Ok::<_, &str>(events),
                        RelayPoolNotification::Message {
                            message:
                                RelayMessage::Closed {
                                    subscription_id,
                                    message,
                                },
                            ..
                        } if subscription_id.as_ref() == &id
                            && !message.starts_with("auth-required:") =>
                        {
                            return Err("subscription closed before EOSE");
                        }
                        _ => {}
                    }
                }
            })
            .await;
            client.disconnect().await;
            match result {
                Ok(Ok(events)) => {
                    eprintln!("relay {url}: read complete (EOSE, {} events)", events.len());
                    for e in events {
                        union.insert(e.id, e);
                    }
                }
                _ => {
                    eprintln!("relay {url}: read failed (no complete EOSE)");
                    failed.push(url.clone());
                }
            }
        }
        if !failed.is_empty() {
            return Err(format!("challenge relay read incomplete; failed: {}", failed.join(", ")));
        }
        // Keep all verified versions for eligibility and replacement-timestamp checks; callers
        // select latest per (kind, author, d), never collapse distinct authors sharing a d-tag.
        Ok(union.into_values().collect())
    }
    pub async fn publish(
        &self,
        home: &MaxplayerHome,
        draft: EventDraft,
        created_at: u64,
    ) -> Result<String, String> {
        let tags = draft
            .tags
            .into_iter()
            .map(|t| Tag::parse(t.0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "invalid tags")?;
        let event = EventBuilder::new(Kind::from(draft.kind), draft.content)
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(&keys(home)?)
            .map_err(|_| "challenge signing failed")?;
        let mut failed = Vec::new();
        for url in &self.urls {
            let client = Self::client(home, url).await?;
            let result = tokio::time::timeout(TIMEOUT, client.send_event(&event)).await;
            client.disconnect().await;
            let ok = matches!(result, Ok(Ok(ref output)) if !output.success.is_empty());
            if !ok { failed.push(url.clone()); }
            eprintln!(
                "relay {url}: publish {}",
                if ok {
                    "accepted"
                } else {
                    "failed or not acknowledged"
                }
            );
        }
        if !failed.is_empty() {
            return Err(format!("challenge publish incomplete; failed: {}", failed.join(", ")));
        }
        Ok(event.id.to_hex())
    }
}
pub(crate) fn keys(home: &MaxplayerHome) -> Result<Keys, String> {
    let secret = home::read_secret_key_hex(home).map_err(|_| "could not read home key")?;
    Keys::parse(&secret).map_err(|_| "invalid home key".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relay_fences_and_defaults() {
        assert_eq!(Relays::new(&[]).unwrap().urls.len(), 3);
        for bad in [
            "wss://relay.maxplayer.ai",
            "wss://RELAY.MAXPLAYER.AI./",
            "wss://relay.maxplayer.ai%2e",
            "ws://example.com",
            "https://nos.lol",
            "wss://user:password@nos.lol",
        ] {
            assert!(Relays::new(&[bad.into()]).is_err(), "{bad}");
        }
        for good in ["ws://127.0.0.1:1234", "ws://[::1]:1234", "wss://nos.lol"] {
            assert!(Relays::new(&[good.into()]).is_ok());
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::tungstenite::Message;

    struct Fixture {
        url: String,
        writes: Arc<Mutex<Vec<Event>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn fixture(events: Vec<Event>, eose: bool, accept: bool) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let writes = Arc::new(Mutex::new(Vec::new()));
        let recorded = writes.clone();
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let events = events.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    while let Some(Ok(message)) = ws.next().await {
                        let Message::Text(text) = message else {
                            continue;
                        };
                        let m: serde_json::Value = serde_json::from_str(&text).unwrap();
                        match m[0].as_str() {
                            Some("REQ") => {
                                for event in &events {
                                    ws.send(Message::text(
                                        serde_json::json!(["EVENT", m[1], event]).to_string(),
                                    ))
                                    .await
                                    .unwrap();
                                }
                                if eose {
                                    ws.send(Message::text(
                                        serde_json::json!(["EOSE", m[1]]).to_string(),
                                    ))
                                    .await
                                    .unwrap();
                                }
                            }
                            Some("EVENT") => {
                                let event: Event = serde_json::from_value(m[1].clone()).unwrap();
                                recorded.lock().unwrap().push(event.clone());
                                ws.send(Message::text(
                                    serde_json::json!(["OK", event.id.to_hex(), accept, "fixture"])
                                        .to_string(),
                                ))
                                .await
                                .unwrap();
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        Fixture { url, writes, task }
    }
    fn event(keys: &Keys, d: &str, t: u64) -> Event {
        EventBuilder::new(Kind::Custom(30341), "fixture")
            .tags([Tag::identifier(d)])
            .custom_created_at(Timestamp::from(t))
            .sign_with_keys(keys)
            .unwrap()
    }
    #[tokio::test]
    async fn union_dedup_latest_and_publish_all_with_partial_rejection() {
        let dir = std::env::temp_dir().join(format!(
            "challenges-union-{}",
            Keys::generate().public_key()
        ));
        let home = home::bootstrap(&dir).unwrap();
        let keys = keys(&home).unwrap();
        let older = event(&keys, "one", 100);
        let newer = event(&keys, "one", 101);
        let other = event(&keys, "two", 102);
        let a = fixture(vec![older.clone(), other.clone()], true, false).await;
        let b = fixture(vec![older, newer.clone()], true, true).await;
        let relays = Relays::new(&[a.url.clone(), b.url.clone()]).unwrap();
        let events = relays
            .fetch(&home, Filter::new().kind(Kind::Custom(30341)))
            .await
            .unwrap();
        assert_eq!(events.len(), 3);
        let latest = maxplayer_core::challenge::latest_per_coordinate(events.iter());
        assert_eq!(latest.len(), 2);
        assert!(latest.iter().any(|e| e.id == newer.id));
        assert!(latest.iter().any(|e| e.id == other.id));
        let draft = maxplayer_core::challenge::Challenge {
            buyer: keys.public_key().to_hex(),
            id: "publish".into(),
            title: "test".into(),
            goal: "goal".into(),
            starts: 100,
            ends: 200,
            budget_sats: 0,
            status: "scheduled".into(),
            update: None,
            result: None,
        }
        .to_event_draft()
        .unwrap();
        let error = relays
            .publish(&home, draft.clone(), Timestamp::now().as_secs())
            .await
            .unwrap_err();
        assert!(error.contains(&a.url));
        assert_eq!(a.writes.lock().unwrap().len(), 1);
        assert_eq!(b.writes.lock().unwrap().len(), 1);
        let refused = Relays::new(std::slice::from_ref(&a.url)).unwrap();
        assert!(
            refused
                .publish(&home, draft, Timestamp::now().as_secs())
                .await
                .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn missing_eose_is_error_and_partial_events_are_discarded() {
        let dir =
            std::env::temp_dir().join(format!("challenges-eose-{}", Keys::generate().public_key()));
        let home = home::bootstrap(&dir).unwrap();
        let keys = keys(&home).unwrap();
        let incomplete = fixture(vec![event(&keys, "hidden", 100)], false, true).await;
        let complete = fixture(vec![], true, true).await;
        assert!(
            Relays::new(std::slice::from_ref(&incomplete.url))
                .unwrap()
                .fetch(&home, Filter::new().kind(Kind::Custom(30341)))
                .await
                .is_err()
        );
        let partial = Relays::new(&[incomplete.url.clone(), complete.url.clone()])
            .unwrap()
            .fetch(&home, Filter::new().kind(Kind::Custom(30341)))
            .await;
        assert!(partial.unwrap_err().contains(&incomplete.url));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
