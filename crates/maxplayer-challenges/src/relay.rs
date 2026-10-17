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
/// Events per history REQ (the common public-relay cap; same as the page's `PAGE_LIMIT`).
pub const PAGE_LIMIT: usize = 500;
struct Page {
    events: Vec<Event>,
    received: usize,
    oldest: Option<Timestamp>,
}
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
            // One canonical list shared with the browser, runner and site build.
            let source = include_str!("../../../web/app/src/challenge/shared/relay.mjs");
            let literal = source
                .lines()
                .find_map(|line| line.strip_prefix("export const CHALLENGE_RELAY_DENYLIST = "))
                .ok_or("missing challenge relay denylist")?
                .trim_end_matches(';');
            let denied: Vec<String> =
                serde_json::from_str(literal).map_err(|_| "invalid challenge relay denylist")?;
            if denied.contains(&host) {
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
        // Event size/tag limits off: a frame the SDK dropped would not count toward a full
        // page, so a short-looking page could hide older history. Message size stays capped.
        let limits = RelayLimits {
            events: nostr_sdk::pool::relay::limits::RelayEventLimits::disable(),
            ..Default::default()
        };
        let client = Client::builder()
            .signer(keys(home)?)
            .opts(ClientOptions::new().relay_limits(limits))
            .build();
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
    /// Full history for `filter` on every relay, paged like the page's trusted reader
    /// (`web/app/src/challenge/shared/relay.mjs`): each REQ carries `limit: PAGE_LIMIT`, and a
    /// full page is never treated as complete. It re-reads from the oldest in-range second
    /// (inclusive `until`) and fails the read unless that cursor moves strictly older.
    pub async fn fetch(&self, home: &MaxplayerHome, filter: Filter) -> Result<Vec<Event>, String> {
        self.read(home, filter.limit(PAGE_LIMIT), true)
            .await
            .map(|(events, _)| events)
    }
    /// One newest-first page per relay at the filter's own `limit`, never paged (untrusted,
    /// display-only reads). The flag is true when any relay filled that page.
    pub async fn fetch_sample(
        &self,
        home: &MaxplayerHome,
        filter: Filter,
    ) -> Result<(Vec<Event>, bool), String> {
        let limit = filter.limit.unwrap_or(PAGE_LIMIT);
        self.read(home, filter.limit(limit), false).await
    }
    async fn read(
        &self,
        home: &MaxplayerHome,
        filter: Filter,
        paged: bool,
    ) -> Result<(Vec<Event>, bool), String> {
        let mut union = BTreeMap::new();
        let mut failed = Vec::new();
        let mut saturated = false;
        for url in &self.urls {
            let client = Self::client(home, url).await?;
            let result = Self::read_relay(&client, &filter, paged).await;
            client.disconnect().await;
            match result {
                Ok((events, pages, full)) => {
                    eprintln!(
                        "relay {url}: read complete (EOSE, {} events, {pages} page(s))",
                        events.len()
                    );
                    saturated |= full;
                    for e in events {
                        union.insert(e.id, e);
                    }
                }
                Err(reason) => {
                    eprintln!("relay {url}: read failed ({reason})");
                    failed.push(format!("{url} ({reason})"));
                }
            }
        }
        if !failed.is_empty() {
            return Err(format!(
                "challenge relay read incomplete; failed: {}",
                failed.join(", ")
            ));
        }
        // Keep all verified versions for eligibility and replacement-timestamp checks; callers
        // select latest per (kind, author, d), never collapse distinct authors sharing a d-tag.
        Ok((union.into_values().collect(), saturated))
    }
    /// Reasons are fixed strings: no relay-supplied text reaches diagnostics.
    async fn read_relay(
        client: &Client,
        filter: &Filter,
        paged: bool,
    ) -> Result<(Vec<Event>, usize, bool), &'static str> {
        let limit = filter.limit.unwrap_or(PAGE_LIMIT);
        let mut cursor = filter.clone();
        let mut events = Vec::new();
        let mut pages = 0;
        loop {
            pages += 1;
            let page = tokio::time::timeout(TIMEOUT, Self::read_page(client, &cursor))
                .await
                .map_err(|_| "no EOSE before timeout")??;
            events.extend(page.events);
            if page.received < limit {
                return Ok((events, pages, false));
            }
            if !paged {
                return Ok((events, pages, true));
            }
            // A full page: the relay may have cut it at its limit. Re-read the oldest second
            // (inclusive) so a tie there is never skipped; the cursor must move strictly older.
            cursor = match page.oldest {
                None => return Err("history truncated: full page without an in-range event"),
                Some(oldest) if cursor.until.is_some_and(|until| oldest >= until) => {
                    return Err("history truncated: 500+ events in one second");
                }
                Some(oldest) => cursor.until(oldest),
            };
        }
    }
    async fn read_page(client: &Client, filter: &Filter) -> Result<Page, &'static str> {
        let mut rx = client.notifications();
        let id = SubscriptionId::generate();
        client
            .subscribe_with_id(id.clone(), filter.clone(), None)
            .await
            .map_err(|_| "subscription failed")?;
        let mut page = Page {
            events: Vec::new(),
            received: 0,
            oldest: None,
        };
        let result = loop {
            match rx.recv().await {
                Err(_) => break Err("notification stream ended"),
                Ok(RelayPoolNotification::Message {
                    message:
                        RelayMessage::Event {
                            subscription_id,
                            event,
                        },
                    ..
                }) if subscription_id.as_ref() == &id => {
                    // Saturation counts every frame the relay spent its limit on, including
                    // events outside the requested window or filter.
                    page.received += 1;
                    if event.verify().is_ok() && filter.match_event(&event, Default::default()) {
                        page.oldest = Some(
                            page.oldest
                                .map_or(event.created_at, |o: Timestamp| o.min(event.created_at)),
                        );
                        page.events.push(event.into_owned());
                    }
                }
                Ok(RelayPoolNotification::Message {
                    message: RelayMessage::EndOfStoredEvents(subscription_id),
                    ..
                }) if subscription_id.as_ref() == &id => break Ok(()),
                Ok(RelayPoolNotification::Message {
                    message:
                        RelayMessage::Closed {
                            subscription_id,
                            message,
                        },
                    ..
                }) if subscription_id.as_ref() == &id && !message.starts_with("auth-required:") => {
                    break Err("subscription closed before EOSE");
                }
                Ok(_) => {}
            }
        };
        client.unsubscribe(&id).await;
        result.map(|()| page)
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
            if !ok {
                failed.push(url.clone());
            }
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
            return Err(format!(
                "challenge publish incomplete; failed: {}",
                failed.join(", ")
            ));
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
pub(crate) mod transport_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::tungstenite::Message;

    pub(crate) struct Fixture {
        pub(crate) url: String,
        pub(crate) writes: Arc<Mutex<Vec<Event>>>,
        pub(crate) reqs: Arc<Mutex<Vec<serde_json::Value>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    type Answer = Arc<dyn Fn(&serde_json::Value) -> Vec<Event> + Send + Sync>;
    /// Every REQ gets every event, whatever its filter (models a relay ignoring filters).
    pub(crate) async fn fixture(events: Vec<Event>, eose: bool, accept: bool) -> Fixture {
        fixture_with(Arc::new(move |_| events.clone()), eose, accept).await
    }
    /// Answers like a public relay: honours kinds/authors/#p/since/until, newest first (ties in
    /// the given order, so junk listed first wins a tie), at most `limit` (default cap 500).
    /// `sample_only` events are served only to REQs without `authors` (the applicant sample),
    /// modelling a relay that returned an event to one query and not to the trusted one.
    pub(crate) async fn relay_like(events: Vec<Event>, sample_only: Vec<Event>) -> Fixture {
        let answer = move |raw: &serde_json::Value| {
            let filter: Filter = serde_json::from_value(raw.clone()).unwrap();
            let mut out: Vec<Event> = events
                .iter()
                .chain(sample_only.iter().filter(|_| filter.authors.is_none()))
                .filter(|e| filter.match_event(e, MatchEventOptions::new()))
                .cloned()
                .collect();
            out.sort_by_key(|a| std::cmp::Reverse(a.created_at));
            out.truncate(filter.limit.unwrap_or(PAGE_LIMIT));
            out
        };
        fixture_with(Arc::new(answer), true, true).await
    }
    async fn fixture_with(answer: Answer, eose: bool, accept: bool) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let writes = Arc::new(Mutex::new(Vec::new()));
        let recorded = writes.clone();
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let filters = reqs.clone();
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let answer = answer.clone();
                let recorded = recorded.clone();
                let filters = filters.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    while let Some(Ok(message)) = ws.next().await {
                        let Message::Text(text) = message else {
                            continue;
                        };
                        let m: serde_json::Value = serde_json::from_str(&text).unwrap();
                        match m[0].as_str() {
                            Some("REQ") => {
                                filters.lock().unwrap().push(m[2].clone());
                                for event in &answer(&m[2]) {
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
        Fixture {
            url,
            writes,
            reqs,
            task,
        }
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
    async fn partial_publish_names_failed_url_and_remains_on_live_relay() {
        use nostr_relay_builder::prelude::*;
        let live = LocalRelay::new(RelayBuilder::default());
        live.run().await.unwrap();
        let live_url = live.url().await.to_string();
        let failed = fixture(vec![], true, false).await;
        let dir = std::env::temp_dir().join(format!(
            "challenge-partial-{}",
            Keys::generate().public_key()
        ));
        let home = home::bootstrap(&dir).unwrap();
        let draft = maxplayer_core::challenge::Challenge {
            buyer: keys(&home).unwrap().public_key().to_hex(),
            id: "partial".into(),
            title: "Test".into(),
            goal: "Goal".into(),
            starts: 100,
            ends: 200,
            budget_sats: 0,
            status: "scheduled".into(),
            update: None,
            result: None,
        }
        .to_event_draft()
        .unwrap();
        let error = Relays::new(&[live_url.clone(), failed.url.clone()])
            .unwrap()
            .publish(&home, draft, Timestamp::now().as_secs())
            .await
            .unwrap_err();
        assert!(error.contains(&failed.url), "{error}");
        let stored = Relays::new(&[live_url])
            .unwrap()
            .fetch(&home, Filter::new().kind(Kind::Custom(30341)))
            .await
            .unwrap();
        assert_eq!(
            stored.len(),
            1,
            "partial publish must remain readable on live relay"
        );
        assert_eq!(stored[0].content, "Goal");
        live.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn full_page_is_never_complete_without_older_cursor() {
        let dir =
            std::env::temp_dir().join(format!("challenges-page-{}", Keys::generate().public_key()));
        let home = home::bootstrap(&dir).unwrap();
        let keys = keys(&home).unwrap();
        let filter = Filter::new().kind(Kind::Custom(30341));
        // 500 frames the filter rejects (wrong kind): a full page with no in-range event.
        let other: Vec<_> = (0..PAGE_LIMIT)
            .map(|i| {
                EventBuilder::new(Kind::TextNote, format!("{i}"))
                    .sign_with_keys(&keys)
                    .unwrap()
            })
            .collect();
        let relay = fixture(other, true, true).await;
        let error = Relays::new(std::slice::from_ref(&relay.url))
            .unwrap()
            .fetch(&home, filter.clone())
            .await
            .unwrap_err();
        assert!(
            error.contains("full page without an in-range event"),
            "{error}"
        );
        // 500 matching events in one second: re-read once with inclusive until, then fail.
        let tied: Vec<_> = (0..PAGE_LIMIT)
            .map(|i| event(&keys, &format!("d{i}"), 100))
            .collect();
        let relay = relay_like(tied, vec![]).await;
        let error = Relays::new(std::slice::from_ref(&relay.url))
            .unwrap()
            .fetch(&home, filter)
            .await
            .unwrap_err();
        assert!(error.contains("500+ events in one second"), "{error}");
        let reqs = relay.reqs.lock().unwrap();
        assert_eq!(reqs.len(), 2, "one re-read of the saturated second");
        assert_eq!(reqs[0]["limit"], PAGE_LIMIT);
        assert_eq!(reqs[1]["until"], 100);
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
