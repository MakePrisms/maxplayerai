//! Public relay transport. Read failures never masquerade as an empty membership set.
use maxplayer_core::{
    gateway::EventDraft,
    home::{self, MaxplayerHome},
};
use nostr_sdk::pool::transport::{
    error::TransportError,
    websocket::{DefaultWebsocketTransport, WebSocketSink, WebSocketStream, WebSocketTransport},
};
use nostr_sdk::prelude::*;
use nostr_sdk::util::BoxedFuture;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

pub const DEFAULT_RELAYS: [&str; 3] = [
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://offchain.pub",
];
const TIMEOUT: Duration = Duration::from_secs(10);
/// Default challenge relay quorum (run 2 #13): relays that must complete a read, and acknowledge
/// a publish, for it to count. Same rule as the page/runner (`parseChallengeQuorum` in
/// `web/app/src/challenge/shared/relay.mjs`): 2, raised to a strict majority for longer lists and
/// lowered to the list size for one relay. A configured quorum must be a strict majority, so every
/// quorum read shares at least one relay with every quorum publish.
pub const DEFAULT_QUORUM: usize = 2;
pub fn majority(n: usize) -> usize {
    n / 2 + 1
}
pub fn default_quorum(n: usize) -> usize {
    n.min(DEFAULT_QUORUM.max(majority(n)))
}
/// Re-send outbox bounds: entries kept, delivery attempts per missing relay, and age.
pub const OUTBOX_MAX_ENTRIES: usize = 64;
pub const OUTBOX_MAX_ATTEMPTS: u32 = 10;
pub const OUTBOX_MAX_AGE_SECS: u64 = 7 * 24 * 3600;
/// Events per history REQ (the common public-relay cap; same as the page's `PAGE_LIMIT`).
pub const PAGE_LIMIT: usize = 500;
/// Most pages one trusted `fetch` may request from one relay. Reaching it with a still-full page
/// fails the read with `history truncated` (nothing is published): 20 pages walk up to ~10,000
/// events (500 + 19 x 499 new, the oldest second is re-read) in at most 20 x `TIMEOUT` (200 s),
/// far beyond any real roster's membership history.
pub const MAX_TRUSTED_PAGES: usize = 20;
/// Raw view of one subscription: every `EVENT` frame the relay sent for it, and the oldest
/// verified, filter-matching one among them.
#[derive(Debug)]
struct RawPage {
    filter: Filter,
    frames: usize,
    oldest: Option<Timestamp>,
}
/// Counts `EVENT` frames per registered subscription id on the raw websocket, before nostr-sdk
/// parses, verifies, expires (NIP-40), size-checks or deduplicates them. Those drops never
/// reach `notifications()`, so saturation counted there would make a full page look short.
#[derive(Debug, Default)]
struct FrameCounter(Mutex<HashMap<String, RawPage>>);
impl FrameCounter {
    fn register(&self, id: &SubscriptionId, filter: &Filter) {
        let page = RawPage {
            filter: filter.clone(),
            frames: 0,
            oldest: None,
        };
        self.0.lock().unwrap().insert(id.to_string(), page);
    }
    fn take(&self, id: &SubscriptionId) -> (usize, Option<Timestamp>) {
        self.0
            .lock()
            .unwrap()
            .remove(id.as_str())
            .map_or((0, None), |p| (p.frames, p.oldest))
    }
    /// Text the SDK might handle (`as_text` also converts binary/ping frames, which only ever
    /// over-counts: fail closed). Unregistered ids are ignored, so the map stays bounded.
    fn observe(&self, text: &str) {
        if self.0.lock().unwrap().is_empty() || !text.contains("EVENT") {
            return;
        }
        let Ok(serde_json::Value::Array(mut m)) = serde_json::from_str(text) else {
            return;
        };
        if m.len() < 2 || m[0].as_str() != Some("EVENT") {
            return;
        }
        let Some(id) = m[1].as_str().map(str::to_owned) else {
            return;
        };
        // Like the web reader, the cursor may move to any verified frame that matches the
        // page's filter, even one the SDK then drops (e.g. expired): those still used up the
        // relay's limit, and paging past them is how older history is reached.
        let event = m
            .get_mut(2)
            .map(serde_json::Value::take)
            .and_then(|v| serde_json::from_value::<Event>(v).ok());
        let mut pages = self.0.lock().unwrap();
        let Some(page) = pages.get_mut(&id) else {
            return;
        };
        page.frames += 1;
        if let Some(event) = event
            && page.filter.match_event(&event, Default::default())
            && event.verify().is_ok()
        {
            page.oldest = Some(
                page.oldest
                    .map_or(event.created_at, |o| o.min(event.created_at)),
            );
        }
    }
}
/// The default transport, with every received frame shown to a [`FrameCounter`] first. The SDK
/// reads frames in order, so by the time it notifies a subscription's EOSE every `EVENT` frame
/// the relay sent before it has been counted.
#[derive(Debug)]
struct CountingTransport(Arc<FrameCounter>);
impl WebSocketTransport for CountingTransport {
    fn support_ping(&self) -> bool {
        DefaultWebsocketTransport.support_ping()
    }
    fn connect<'a>(
        &'a self,
        url: &'a Url,
        mode: &'a nostr_sdk::pool::ConnectionMode,
        timeout: Duration,
    ) -> BoxedFuture<'a, Result<(WebSocketSink, WebSocketStream), TransportError>> {
        Box::pin(async move {
            let (sink, stream) = DefaultWebsocketTransport
                .connect(url, mode, timeout)
                .await?;
            let counter = self.0.clone();
            let stream: WebSocketStream = Box::pin(stream.inspect(move |frame| {
                if let Ok(frame) = frame
                    && let Some(text) = frame.as_text()
                {
                    counter.observe(text);
                }
            }));
            Ok((sink, stream))
        })
    }
}
struct Page {
    events: Vec<Event>,
    received: usize,
    oldest: Option<Timestamp>,
}
/// What this invocation saw: relays that completed a read or accepted a publish (the only ones
/// the outbox re-sends to), degraded-quorum warnings, and events re-sent.
#[derive(Debug, Default)]
pub struct Report {
    pub answered: BTreeSet<String>,
    pub warnings: Vec<String>,
    pub missing: BTreeSet<String>,
    pub resent: Vec<String>,
}
pub struct Relays {
    urls: Vec<String>,
    quorum: usize,
    report: Mutex<Report>,
}
impl Relays {
    /// Default quorum. Production always goes through [`Self::with_quorum`] with the `--quorum`
    /// flag; this is the tests' shorthand.
    #[cfg(test)]
    pub fn new(urls: &[String]) -> Result<Self, String> {
        Self::with_quorum(urls, None)
    }
    /// `quorum` None uses [`default_quorum`]; an explicit value must be a strict majority of the
    /// (deduplicated) relays and at most their number.
    pub fn with_quorum(urls: &[String], quorum: Option<usize>) -> Result<Self, String> {
        let urls = Self::validate(urls)?;
        let n = urls.len();
        let quorum = match quorum {
            None => default_quorum(n),
            Some(q) if (majority(n)..=n).contains(&q) => q,
            Some(_) => {
                return Err(format!(
                    "invalid --quorum: must be from {} (a majority) to {n} (every relay)",
                    majority(n)
                ));
            }
        };
        Ok(Self {
            urls,
            quorum,
            report: Mutex::new(Report::default()),
        })
    }
    pub fn quorum(&self) -> (usize, usize) {
        (self.quorum, self.urls.len())
    }
    /// Relays missing from any read or publish so far in this invocation.
    pub fn missing(&self) -> BTreeSet<String> {
        self.report.lock().unwrap().missing.clone()
    }
    pub fn take_report(&self) -> Report {
        std::mem::take(&mut *self.report.lock().unwrap())
    }
    fn degraded(&self, what: &str, ok: usize, failed: &[String], missing: &[String]) {
        let warning = format!(
            "degraded {what}: {ok} of {} challenge relays (quorum {}); missing: {}",
            self.urls.len(),
            self.quorum,
            failed.join(", ")
        );
        let mut report = self.report.lock().unwrap();
        report.missing.extend(missing.iter().cloned());
        if !report.warnings.contains(&warning) {
            report.warnings.push(warning);
        }
    }
    fn validate(urls: &[String]) -> Result<Vec<String>, String> {
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
        Ok(normalized)
    }
    async fn client(
        home: &MaxplayerHome,
        url: &str,
    ) -> Result<(Client, Arc<FrameCounter>), String> {
        // Saturation is counted on raw frames (`CountingTransport`), so an SDK drop can no
        // longer shorten a page; event size/tag limits stay off so fewer valid frames are lost.
        let limits = RelayLimits {
            events: nostr_sdk::pool::relay::limits::RelayEventLimits::disable(),
            ..Default::default()
        };
        let counter = Arc::new(FrameCounter::default());
        // An id-tracker database (no event store): it never answers `Rejected(Replaced)`, so a
        // newer same-coordinate event (even an invalid membership) cannot make the SDK swallow
        // an older valid one. The caller picks the latest *valid* event per coordinate.
        let database = MemoryDatabase::with_opts(MemoryDatabaseOptions {
            events: false,
            ..Default::default()
        });
        let client = Client::builder()
            .signer(keys(home)?)
            .opts(ClientOptions::new().relay_limits(limits))
            .websocket_transport(CountingTransport(counter.clone()))
            .database(database)
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
        Ok((client, counter))
    }
    /// Full history for `filter` on every relay, paged like the page's trusted reader
    /// (`web/app/src/challenge/shared/relay.mjs`): each REQ carries `limit: PAGE_LIMIT`, and a
    /// full page (counted in raw relay frames) is never treated as complete. It re-reads from
    /// the oldest in-range second (inclusive `until`), fails the read unless that cursor moves
    /// strictly older, and fails once `MAX_TRUSTED_PAGES` pages were all full.
    ///
    /// Quorum read: every relay is asked and every complete answer is unioned (so with all relays
    /// up, one honest relay is still enough); it succeeds once at least `quorum` relays completed.
    pub async fn fetch(&self, home: &MaxplayerHome, filter: Filter) -> Result<Vec<Event>, String> {
        self.read(home, filter.limit(PAGE_LIMIT), true, self.quorum)
            .await
            .map(|(events, _)| events)
    }
    /// [`Self::fetch`], but every configured relay must complete (no quorum). For a check that
    /// must not miss an event published below quorum (the minutes-change challenge read).
    pub async fn fetch_all(
        &self,
        home: &MaxplayerHome,
        filter: Filter,
    ) -> Result<Vec<Event>, String> {
        self.read(home, filter.limit(PAGE_LIMIT), true, self.urls.len())
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
        self.read(home, filter.limit(limit), false, self.quorum)
            .await
    }
    async fn read(
        &self,
        home: &MaxplayerHome,
        filter: Filter,
        paged: bool,
        need: usize,
    ) -> Result<(Vec<Event>, bool), String> {
        let mut union = BTreeMap::new();
        let mut failed = Vec::new();
        let mut missing = Vec::new();
        let mut ok = 0;
        let mut saturated = false;
        for url in &self.urls {
            // A relay that cannot even be configured is one more missing relay, not a global error.
            let result = match Self::client(home, url).await {
                Ok((client, counter)) => {
                    let result = Self::read_relay(&client, &counter, &filter, paged).await;
                    client.disconnect().await;
                    result
                }
                Err(_) => Err("could not configure relay"),
            };
            match result {
                Ok((events, pages, full)) => {
                    eprintln!(
                        "relay {url}: read complete (EOSE, {} events, {pages} page(s))",
                        events.len()
                    );
                    saturated |= full;
                    ok += 1;
                    self.report.lock().unwrap().answered.insert(url.clone());
                    for e in events {
                        union.insert(e.id, e);
                    }
                }
                Err(reason) => {
                    eprintln!("relay {url}: read failed ({reason})");
                    failed.push(format!("{url} ({reason})"));
                    missing.push(url.clone());
                }
            }
        }
        if ok < need {
            // Below quorum (or, for a strict read, any relay missing): fail closed.
            return Err(if need == self.urls.len() {
                format!(
                    "challenge relay read incomplete; failed: {}",
                    failed.join(", ")
                )
            } else {
                format!(
                    "challenge relay read incomplete; quorum not met ({ok} of {} relays complete, need {need}); failed: {}",
                    self.urls.len(),
                    failed.join(", ")
                )
            });
        }
        if !failed.is_empty() {
            self.degraded("read", ok, &failed, &missing);
        }
        // An event this home published at a bare quorum, whose missing relay has not had it re-sent
        // yet, is still in the outbox: a quorum read that omits it (the overlapping relay lost it)
        // still sees it. Trusted reads only; outbox events are own-key and signature-checked.
        // Pruned first: an entry past its age or attempt bound no longer counts (advisor r1 nit).
        if paged {
            let mut outbox = Outbox::load(home);
            outbox.prune(now());
            for e in outbox.entries {
                if filter.match_event(&e.event, Default::default()) {
                    union.entry(e.event.id).or_insert(e.event);
                }
            }
        }
        // Keep all verified versions for eligibility and replacement-timestamp checks; callers
        // select latest per (kind, author, d), never collapse distinct authors sharing a d-tag.
        Ok((union.into_values().collect(), saturated))
    }
    /// Reasons are fixed strings: no relay-supplied text reaches diagnostics.
    async fn read_relay(
        client: &Client,
        counter: &FrameCounter,
        filter: &Filter,
        paged: bool,
    ) -> Result<(Vec<Event>, usize, bool), &'static str> {
        let limit = filter.limit.unwrap_or(PAGE_LIMIT);
        let mut cursor = filter.clone();
        let mut events = Vec::new();
        let mut pages = 0;
        loop {
            pages += 1;
            let page = tokio::time::timeout(TIMEOUT, Self::read_page(client, counter, &cursor))
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
            if pages >= MAX_TRUSTED_PAGES {
                return Err("history truncated: trusted page cap reached");
            }
        }
    }
    async fn read_page(
        client: &Client,
        counter: &FrameCounter,
        filter: &Filter,
    ) -> Result<Page, &'static str> {
        let mut rx = client.notifications();
        let id = SubscriptionId::generate();
        counter.register(&id, filter);
        if client
            .subscribe_with_id(id.clone(), filter.clone(), None)
            .await
            .is_err()
        {
            counter.take(&id);
            return Err("subscription failed");
        }
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
                    // Delivered frames; the raw count below also includes the ones the SDK
                    // dropped (expired, bad signature, oversized, replaced).
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
        // Saturation counts every EVENT frame the relay spent its limit on, including events
        // outside the requested window or filter and events the SDK never delivered.
        let (frames, raw_oldest) = counter.take(&id);
        result.map(|()| Page {
            received: page.received.max(frames),
            oldest: match (page.oldest, raw_oldest) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            ..page
        })
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
            let ok = Self::send(home, url, &event).await;
            if ok {
                self.report.lock().unwrap().answered.insert(url.clone());
            } else {
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
        let accepted = self.urls.len() - failed.len();
        if accepted < self.quorum {
            // Below quorum: the command fails and nothing is queued for re-send (the caller was
            // told it failed; a later re-send must never publish what they believe was not).
            return Err(format!(
                "challenge publish incomplete; quorum not met ({accepted} of {} relays accepted, need {}); failed: {}",
                self.urls.len(),
                self.quorum,
                failed.join(", ")
            ));
        }
        if !failed.is_empty() {
            self.degraded("publish", accepted, &failed, &failed);
        }
        // Quorum met: queue this exact signed event for the relays that missed it, to re-send once
        // they answer again (`resend`). Queueing also drops any older queued version of the same
        // coordinate (it is superseded). Persisting is best effort: a failure only loses the repair.
        let path = Outbox::path(home);
        if !failed.is_empty() || path.exists() {
            let mut outbox = Outbox::load(home);
            outbox.queue(&event, &failed, now());
            if let Err(e) = outbox.save(home) {
                eprintln!("Warning: could not save the challenge re-send outbox ({e})");
            }
        }
        Ok(event.id.to_hex())
    }
    /// Send one already-signed event to one relay; true when the relay acknowledged it.
    async fn send(home: &MaxplayerHome, url: &str, event: &Event) -> bool {
        let Ok((client, _)) = Self::client(home, url).await else {
            return false;
        };
        let result = tokio::time::timeout(TIMEOUT, client.send_event(event)).await;
        client.disconnect().await;
        matches!(result, Ok(Ok(ref output)) if !output.success.is_empty())
    }
    /// Re-send queued events (exactly as signed, never re-signed) to the relays that missed them,
    /// but only to relays that answered a read or accepted a publish in this invocation, so a still
    /// dead relay costs nothing. Bounded: at most `OUTBOX_MAX_ATTEMPTS` failed sends per relay and
    /// `OUTBOX_MAX_AGE_SECS` per entry; idempotent: a relay already holding the event just
    /// acknowledges it again, and an acknowledged relay is removed from the entry.
    pub async fn resend(&self, home: &MaxplayerHome) {
        let at = now();
        let mut outbox = Outbox::load(home);
        if outbox.entries.is_empty() {
            return;
        }
        // Prune first, with the same `at` as after the sends: an entry past its age or attempt
        // bound is never sent again, including on the command that expires it (advisor r2).
        outbox.prune(at);
        let answered = self.report.lock().unwrap().answered.clone();
        let mut resent = Vec::new();
        for entry in &mut outbox.entries {
            let targets: Vec<String> = entry
                .missing
                .keys()
                .filter(|url| answered.contains(*url))
                .cloned()
                .collect();
            for url in targets {
                let ok = Self::send(home, &url, &entry.event).await;
                eprintln!(
                    "relay {url}: re-send {} {}",
                    entry.event.id.to_hex(),
                    if ok { "accepted" } else { "failed" }
                );
                if ok {
                    entry.missing.remove(&url);
                    resent.push(format!("{} -> {url}", entry.event.id.to_hex()));
                } else if let Some(n) = entry.missing.get_mut(&url) {
                    *n += 1;
                }
            }
        }
        outbox.prune(at);
        if let Err(e) = outbox.save(home) {
            eprintln!("Warning: could not save the challenge re-send outbox ({e})");
        }
        self.report.lock().unwrap().resent.extend(resent);
    }
}
fn now() -> u64 {
    Timestamp::now().as_secs()
}
/// One event a quorum publish could not deliver everywhere: the exact signed event and, per
/// relay that missed it, the failed re-send attempts so far.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxEntry {
    pub event: Event,
    pub missing: BTreeMap<String, u32>,
    pub queued_at: u64,
}
/// `<home>/challenges/outbox.json` (directory 0700, file 0600, atomic replace).
///
/// Not locked: each command loads, changes and atomically replaces the whole file, so a crash never
/// tears it, but two sidecar processes running at the same time can drop each other's changes
/// (a queued entry, or a delivery). Both are best-effort repairs; the publish itself already met
/// the quorum. Run one sidecar command per home at a time to keep every entry.
#[derive(Debug, Default)]
pub struct Outbox {
    pub entries: Vec<OutboxEntry>,
}
impl Outbox {
    pub fn path(home: &MaxplayerHome) -> PathBuf {
        home.root.join("challenges").join("outbox.json")
    }
    /// A missing or unreadable file is an empty outbox; entries whose event fails verification or
    /// is not signed by this home's key are dropped (a tampered file can never make the sidecar
    /// send, or read as published, something it did not sign).
    pub fn load(home: &MaxplayerHome) -> Self {
        let Ok(own) = keys(home).map(|k| k.public_key()) else {
            return Self::default();
        };
        let Ok(raw) = std::fs::read_to_string(Self::path(home)) else {
            return Self::default();
        };
        let Ok(serde_json::Value::Array(rows)) = serde_json::from_str(&raw) else {
            return Self::default();
        };
        let entries = rows
            .into_iter()
            .filter_map(|row| {
                let event: Event = serde_json::from_value(row.get("event")?.clone()).ok()?;
                event.verify().ok()?;
                (event.pubkey == own).then_some(())?;
                let missing = serde_json::from_value(row.get("missing")?.clone()).ok()?;
                let queued_at = row.get("queued_at")?.as_u64()?;
                Some(OutboxEntry {
                    event,
                    missing,
                    queued_at,
                })
            })
            .collect();
        Self { entries }
    }
    pub fn save(&self, home: &MaxplayerHome) -> std::io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let path = Self::path(home);
        let dir = path.parent().expect("outbox has a parent");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let rows: Vec<_> = self
            .entries
            .iter()
            .map(|e| serde_json::json!({"event": e.event, "missing": e.missing, "queued_at": e.queued_at}))
            .collect();
        let tmp = dir.join(format!("outbox.json.{}.tmp", std::process::id()));
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(serde_json::Value::Array(rows).to_string().as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)
    }
    fn coordinate(e: &Event) -> (PublicKey, Kind, String) {
        (
            e.pubkey,
            e.kind,
            e.tags.identifier().unwrap_or_default().to_owned(),
        )
    }
    /// Queue `event` for `missing` relays. An older queued event for the same addressable
    /// coordinate is superseded (relays keep only the newest), and the oldest entries beyond
    /// `OUTBOX_MAX_ENTRIES` are dropped.
    pub fn queue(&mut self, event: &Event, missing: &[String], at: u64) {
        let key = Self::coordinate(event);
        self.entries
            .retain(|e| Self::coordinate(&e.event) != key || e.event.created_at > event.created_at);
        if self
            .entries
            .iter()
            .any(|e| Self::coordinate(&e.event) == key)
        {
            return; // a newer version is already queued
        }
        self.entries.push(OutboxEntry {
            event: event.clone(),
            missing: missing.iter().map(|u| (u.clone(), 0)).collect(),
            queued_at: at,
        });
        self.prune(at);
    }
    /// Drop relays out of attempts, delivered or expired entries, and the oldest over the cap.
    pub fn prune(&mut self, at: u64) {
        for e in &mut self.entries {
            e.missing.retain(|_, n| *n < OUTBOX_MAX_ATTEMPTS);
        }
        self.entries.retain(|e| {
            !e.missing.is_empty() && at.saturating_sub(e.queued_at) <= OUTBOX_MAX_AGE_SECS
        });
        self.entries.sort_by_key(|e| e.queued_at);
        let over = self.entries.len().saturating_sub(OUTBOX_MAX_ENTRIES);
        self.entries.drain(..over);
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
        /// false: every new connection is dropped before the WebSocket handshake (relay down).
        pub(crate) online: Arc<std::sync::atomic::AtomicBool>,
        /// The `OK` verdict for every EVENT (false: the relay refuses writes).
        pub(crate) accept: Arc<std::sync::atomic::AtomicBool>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    /// How the fixture answers one REQ.
    enum Reply {
        /// The events, then EOSE (when the fixture sends EOSE at all).
        Events(Vec<Event>),
        /// `CLOSED` (a refused read) instead of events + EOSE.
        Closed,
        /// Nothing at all: the reader waits out its EOSE timeout.
        Silent,
    }
    type Answer = Arc<dyn Fn(&serde_json::Value) -> Reply + Send + Sync>;
    /// How `relay_like_refusing` fails REQs for one kind.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Refusal {
        Closed,
        NoEose,
    }
    /// Every REQ gets every event, whatever its filter (models a relay ignoring filters).
    pub(crate) async fn fixture(events: Vec<Event>, eose: bool, accept: bool) -> Fixture {
        fixture_with(
            Arc::new(move |_| Reply::Events(events.clone())),
            eose,
            accept,
        )
        .await
    }
    /// Answers like a public relay: honours kinds/authors/#p/since/until, newest first (ties in
    /// the given order, so junk listed first wins a tie), at most `limit` (default cap 500).
    /// `sample_only` events are served only to REQs without `authors` (the applicant sample),
    /// modelling a relay that returned an event to one query and not to the trusted one.
    pub(crate) async fn relay_like(events: Vec<Event>, sample_only: Vec<Event>) -> Fixture {
        relay_like_closing(events, sample_only, None).await
    }
    /// `relay_like`, but every REQ whose `kinds` include `closed_kind` is refused with `CLOSED`.
    pub(crate) async fn relay_like_closing(
        events: Vec<Event>,
        sample_only: Vec<Event>,
        closed_kind: Option<u16>,
    ) -> Fixture {
        relay_like_refusing(
            events,
            sample_only,
            closed_kind.map(|k| (k, Refusal::Closed)),
        )
        .await
    }
    /// `relay_like`, but every REQ whose `kinds` include the refused kind fails that way.
    pub(crate) async fn relay_like_refusing(
        events: Vec<Event>,
        sample_only: Vec<Event>,
        refused: Option<(u16, Refusal)>,
    ) -> Fixture {
        let answer = move |raw: &serde_json::Value| {
            if let Some((_, how)) = refused.filter(|(k, _)| {
                raw["kinds"]
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|v| v == k))
            }) {
                return match how {
                    Refusal::Closed => Reply::Closed,
                    Refusal::NoEose => Reply::Silent,
                };
            }
            let filter: Filter = serde_json::from_value(raw.clone()).unwrap();
            let mut out: Vec<Event> = events
                .iter()
                .chain(sample_only.iter().filter(|_| filter.authors.is_none()))
                .filter(|e| filter.match_event(e, MatchEventOptions::new()))
                .cloned()
                .collect();
            out.sort_by_key(|a| std::cmp::Reverse(a.created_at));
            out.truncate(filter.limit.unwrap_or(PAGE_LIMIT));
            Reply::Events(out)
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
        use std::sync::atomic::{AtomicBool, Ordering};
        let online = Arc::new(AtomicBool::new(true));
        let accept = Arc::new(AtomicBool::new(accept));
        let (up, verdict) = (online.clone(), accept.clone());
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                if !up.load(Ordering::SeqCst) {
                    drop(tcp);
                    continue;
                }
                let answer = answer.clone();
                let recorded = recorded.clone();
                let filters = filters.clone();
                let verdict = verdict.clone();
                tokio::spawn(async move {
                    let accept = verdict.load(Ordering::SeqCst);
                    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    while let Some(Ok(message)) = ws.next().await {
                        let Message::Text(text) = message else {
                            continue;
                        };
                        let m: serde_json::Value = serde_json::from_str(&text).unwrap();
                        match m[0].as_str() {
                            Some("REQ") => {
                                filters.lock().unwrap().push(m[2].clone());
                                let events = match answer(&m[2]) {
                                    Reply::Events(events) => events,
                                    Reply::Silent => continue,
                                    Reply::Closed => {
                                        ws.send(Message::text(
                                            serde_json::json!([
                                                "CLOSED",
                                                m[1],
                                                "error: fixture refused"
                                            ])
                                            .to_string(),
                                        ))
                                        .await
                                        .unwrap();
                                        continue;
                                    }
                                };
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
        Fixture {
            url,
            writes,
            reqs,
            online,
            accept,
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
        let refused = |r: Result<Vec<Event>, String>| match r {
            Ok(events) => panic!("full page read as complete ({} events)", events.len()),
            Err(error) => error,
        };
        let error = refused(
            Relays::new(std::slice::from_ref(&relay.url))
                .unwrap()
                .fetch(&home, filter.clone())
                .await,
        );
        assert!(
            error.contains("full page without an in-range event"),
            "{error}"
        );
        // 500 matching events in one second: re-read once with inclusive until, then fail.
        let tied: Vec<_> = (0..PAGE_LIMIT)
            .map(|i| event(&keys, &format!("d{i}"), 100))
            .collect();
        let relay = relay_like(tied, vec![]).await;
        let error = refused(
            Relays::new(std::slice::from_ref(&relay.url))
                .unwrap()
                .fetch(&home, filter)
                .await,
        );
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
