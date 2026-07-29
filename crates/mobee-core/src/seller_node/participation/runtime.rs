//! The live participation runtime: sockets, subscriptions, and the pump that feeds [`Engine`].
//!
//! This is the only module here that performs I/O. It holds one **reader** client per admitted
//! relay, asks [`probe`] to establish access before addressing any of them, subscribes the two
//! filters, and applies whatever [`Action`]s the engine returns.
//!
//! It spawns nothing. [`Participation::pump`] is an ordinary future the caller drives wherever its
//! own loop lives — which matters because the seller node's run loop is `!Send` under the `acp`
//! feature, so a module that reached for `tokio::spawn` would fail to compile on the shipped
//! feature combination while looking fine on the workspace default.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::sync::broadcast;

use nostr_sdk::prelude::{
    Client, Event, PublicKey, RelayMessage, RelayPoolNotification, SubscribeAutoCloseOptions,
    SubscriptionId,
};

use super::super::store::{SellerStore, StoreError};
use super::engine::{Action, ClosedAction, Engine};
use super::relays::{AccessState, ProbeOutcome, RelayRoster};
use super::{ParticipationConfig, dialect, probe};

/// The subscription id of the global membership filter on every relay.
const MEMBERSHIP_SUB: &str = "participation:membership";

/// How long the pump waits between drains. Short enough that a mention is picked up promptly,
/// long enough that an idle node is not spinning.
const POLL_TICK: Duration = Duration::from_millis(50);

/// Floor on how often one relay may be resynced.
///
/// Recovery must not run at pump frequency: a backlog large enough to lag us will still be arriving on
/// the next tick, so an unthrottled resync re-asks for the same window ~20x/second and turns a recovery
/// path into a hot loop against the relay.
const MIN_RESYNC_INTERVAL_SECS: i64 = 5;

/// The subscription id of one channel's mention filter.
fn channel_sub(channel_id: &str) -> String {
    format!("participation:chan:{channel_id}")
}

/// Recover the channel a subscription id belongs to. `None` for the membership subscription or for
/// any id that is not ours — a `CLOSED` for someone else's subscription must not be acted on.
fn channel_of_sub(sub: &str) -> Option<&str> {
    sub.strip_prefix("participation:chan:")
}

/// Whether a subscription id is one of ours.
///
/// A socket multiplexes every subscription on it, including the ones the rest of the node opened
/// and the generated ids `fetch_events` uses. Acting on a stranger's `CLOSED` would drop channels
/// for reasons that had nothing to do with us.
fn is_ours(sub: &SubscriptionId) -> bool {
    let sub = sub.to_string();
    sub == MEMBERSHIP_SUB || channel_of_sub(&sub).is_some()
}

#[derive(Debug)]
pub enum ParticipationError {
    Store(StoreError),
    Relay(String),
}

impl std::fmt::Display for ParticipationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParticipationError::Store(error) => write!(formatter, "participation store: {error}"),
            ParticipationError::Relay(error) => write!(formatter, "participation relay: {error}"),
        }
    }
}

impl std::error::Error for ParticipationError {}

impl From<StoreError> for ParticipationError {
    fn from(value: StoreError) -> Self {
        ParticipationError::Store(value)
    }
}

/// One relay we are participating on.
struct Live {
    /// ★ The READ side, and deliberately not the side that publishes the access carrier. A client
    /// cannot observe its own published events (see [`probe`]), so merging this with the publisher
    /// would make the access probe report failure on relays that work perfectly. Two sockets is the
    /// requirement, not an oversight.
    reader: Client,
    /// ★ Created ONCE, at subscribe time, and held for the relay's whole life.
    ///
    /// A `broadcast::Receiver` only receives what is sent after it exists, so building one per pump
    /// call silently drops everything that arrived between calls — invites, removals, mentions, all
    /// of it, with no error and no gap in the logs. Two acceptance legs failed on exactly this, and
    /// they failed by *missing* events rather than by erroring, which is how it would have shipped.
    notifications: broadcast::Receiver<RelayPoolNotification>,
    engine: Engine,
    /// Events processed since this relay was last resynced — the FORWARD-PROGRESS guarantee.
    ///
    /// A `Lagged` discards the batch in hand so we never act on data from beyond a gap. But if the
    /// discard happens before anything was processed, the cursor does not move, so the resync re-asks
    /// for the same window, floods again, and lags again — the discard removes the very progress that
    /// would end the loop. Zero here means discarding again would repeat it.
    progress_since_resync: u64,
    /// When this relay was last resynced, so recovery cannot re-fire at pump frequency.
    last_resync_unix: i64,
}

/// The node's live participation across all configured relays.
pub struct Participation {
    live: BTreeMap<String, Live>,
    /// Notifications dropped by a client's broadcast buffer before the pump read them.
    lagged: u64,
    /// Times a `Lagged` was not permitted to discard — see [`Participation::forced_progress`].
    forced_progress: u64,
    roster: RelayRoster,
    me: PublicKey,
    /// Kept so access can be RE-proven, not assumed to have survived. A relay whose auth or scope
    /// failed has told us the admission we hold may be stale, and the only way to know is to run the
    /// same positive probe again. `Client` is `Arc`-backed, so holding it costs a refcount.
    publisher: Client,
    carrier: Event,
    probe_timeout: Duration,
}

impl Participation {
    /// Establish access on every configured relay, then subscribe the wake surface on the ones that
    /// proved admission.
    ///
    /// `publisher` publishes the probe carrier; `carrier` is the event to use (the node's persona).
    /// Neither is retained: this module holds no way to publish anything after start-up.
    ///
    /// A relay that fails to connect or fails the probe is recorded denied and then left entirely
    /// alone. Start-up never fails because of a relay — a social surface must not be able to stop
    /// the node that earns money.
    pub async fn start(
        config: &ParticipationConfig,
        store: SellerStore,
        me: PublicKey,
        publisher: &Client,
        carrier: &Event,
    ) -> Result<Self, ParticipationError> {
        let mut roster = RelayRoster::new(config.relays.clone());
        let mut live = BTreeMap::new();
        let timeout = Duration::from_secs(config.probe_timeout_secs.max(1));

        let candidates: Vec<String> = roster
            .relays_to_probe()
            .map(|entry| entry.url.clone())
            .collect();

        for url in candidates {
            let reader = match connect_reader(&url, publisher).await {
                Ok(reader) => reader,
                Err(error) => {
                    roster.record_probe(&url, ProbeOutcome::Refused(error), now_unix());
                    continue;
                }
            };

            let outcome = probe::probe_access(publisher, &url, &reader, carrier, timeout).await;
            let admitted = matches!(outcome, ProbeOutcome::EchoObserved);
            roster.record_probe(&url, outcome, now_unix());

            if !admitted {
                // Close the socket we opened to probe. A denied relay gets nothing further — not a
                // REQ, not a retry, not a heartbeat.
                reader.disconnect().await;
                continue;
            }

            let engine = Engine::new(store.clone(), url.clone(), me);
            // Subscribe to the notification stream BEFORE the first REQ goes out, or the events
            // that REQ produces are broadcast to nobody.
            let notifications = reader.notifications();
            subscribe_wake_surface(&reader, &engine, me).await?;
            live.insert(
                url.clone(),
                Live {
                    reader,
                    notifications,
                    engine,
                    progress_since_resync: 0,
                    last_resync_unix: 0,
                },
            );
        }

        Ok(Self {
            live,
            roster,
            me,
            lagged: 0,
            forced_progress: 0,
            publisher: publisher.clone(),
            carrier: carrier.clone(),
            probe_timeout: timeout,
        })
    }

    /// The per-relay access states, for `status` and for tests that need to prove a relay was
    /// classified rather than merely unused.
    pub fn access_states(&self) -> Vec<(String, AccessState)> {
        self.roster
            .entries()
            .map(|entry| (entry.url.clone(), entry.state.clone()))
            .collect()
    }

    /// Whether we are participating anywhere. `false` is the normal state for a node whose config
    /// lists no relays, or whose relays all denied it.
    pub fn is_live(&self) -> bool {
        !self.live.is_empty()
    }

    /// The identity each reader will authenticate as, per relay.
    ///
    /// Exists because a reader with **no** signer is the one failure that cannot be seen from
    /// behaviour on a relay that does not demand NIP-42: it connects, subscribes, and reads nothing,
    /// forever. `Err` here means that client could not produce an identity at all — which is exactly
    /// the anonymous-reader defect, and it is a question worth being able to ask directly rather
    /// than inferring from silence.
    pub async fn reader_identities(&self) -> Vec<(String, Result<PublicKey, String>)> {
        let mut identities = Vec::new();
        for (url, live) in &self.live {
            let identity = match live.reader.signer().await {
                Ok(signer) => signer
                    .get_public_key()
                    .await
                    .map_err(|error| format!("reader signer cannot produce a pubkey: {error}")),
                Err(error) => Err(format!("reader has NO signer — it will stay anonymous and every \
                                           REQ will be refused with auth-required: {error}")),
            };
            identities.push((url.clone(), identity));
        }
        identities
    }

    /// Process inbound traffic until `budget` elapses.
    ///
    /// Returns the number of events ingested, so a caller (or a test) can distinguish "nothing
    /// arrived" from "the pump never ran" — a distinction the count makes and a bare `Ok(())`
    /// destroys.
    ///
    /// Drains every relay each tick rather than blocking on one, so a chatty relay cannot starve a
    /// quiet one out of the whole budget.
    pub async fn pump(&mut self, budget: Duration) -> Result<usize, ParticipationError> {
        let deadline = tokio::time::Instant::now() + budget;
        let mut ingested = 0usize;

        loop {
            let mut batch: Vec<(String, RelayMessage)> = Vec::new();
            let mut resync: BTreeSet<String> = BTreeSet::new();
            for (url, live) in self.live.iter_mut() {
                loop {
                    match live.notifications.try_recv() {
                        // ★ The `Message` variant, not `Event`: `Event` is suppressed for anything
                        // already in the client's database, and it cannot carry `CLOSED` at all.
                        Ok(RelayPoolNotification::Message { message, .. }) => {
                            batch.push((url.clone(), message));
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::TryRecvError::Empty) => break,
                        Err(broadcast::error::TryRecvError::Closed) => break,
                        // ★ The buffer overflowed and `skipped` notifications are GONE — not delayed,
                        // gone. Two things follow, and only the first is obvious.
                        //
                        // Counting is not enough: a later event would carry this filter's cursor PAST
                        // the skipped ones, turning a recoverable gap into permanent loss.
                        //
                        // And marking the relay for resync is STILL not enough while we keep reading:
                        // every message after the gap is data from beyond a hole we have not filled,
                        // and processing it advances the cursor just the same. If the resubscribe is
                        // then refused or rate-limited, no replay ever arrives and the gap is
                        // permanent. So stop draining this relay, drop what we already hold from it,
                        // and process nothing of its traffic until the replay lands. Discarding is
                        // safe BECAUSE the cursor is behind those messages — the replay re-delivers
                        // them.
                        Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                            self.lagged = self.lagged.saturating_add(skipped);
                            resync.insert(url.clone());
                            break;
                        }
                    }
                }
            }

            // Drop everything already in hand from a relay that lagged. These arrived after its gap,
            // so acting on them is acting on data from beyond a hole — and it is the cursor advance
            // that would make the hole permanent.
            //
            // ★ Dropping the in-hand batch is NOT sufficient on its own. After a `Lagged`, the receiver
            // is positioned at the oldest still-buffered post-gap message, so the next tick would go on
            // consuming pre-resync traffic — the same hole, one tick later. The receiver has to be
            // REPLACED, and replaced BEFORE the resync REQs are sent: a fresh receiver starts at the
            // channel's current tail, discarding the stale backlog, and doing it first guarantees the
            // replay lands in the receiver we will actually read.
            //
            // ★★ And the discard is only safe to repeat once something has been PROCESSED. A cleared
            // suppression re-subscribes from a cursor that stopped advancing, so the relay can serve a
            // backlog big enough to lag us; discarding before any of it is processed leaves the cursor
            // where it was, so the resync re-asks for the same window and lags again. Two individually
            // correct behaviours — never act past a gap, and re-ask from the durable cursor — compose
            // into a livelock unless forward progress is a precondition for discarding again.
            //
            // When it is not safe to discard, we process what we hold instead. That may advance a cursor
            // over skipped events, which is a real loss — but a bounded one, against a livelock that
            // loses everything forever. It is COUNTED rather than silent, because the whole failure class
            // in this module is losses that looked like quiet.
            if !resync.is_empty() {
                let now = now_unix();
                let mut discard: BTreeSet<String> = BTreeSet::new();
                for url in &resync {
                    let Some(live) = self.live.get_mut(url) else {
                        continue;
                    };
                    let throttled = now.saturating_sub(live.last_resync_unix)
                        < MIN_RESYNC_INTERVAL_SECS;
                    if live.progress_since_resync == 0 || throttled {
                        self.forced_progress = self.forced_progress.saturating_add(1);
                        continue;
                    }
                    live.progress_since_resync = 0;
                    live.last_resync_unix = now;
                    live.notifications = live.reader.notifications();
                    discard.insert(url.clone());
                }
                batch.retain(|(url, _)| !discard.contains(url));
                for url in &discard {
                    self.resync_relay(url).await?;
                }
            }

            // Retry channels whose suppression backoff has elapsed. This is the exit from a refusal
            // whose only other way out is a membership event the relay has no reason to send.
            self.retry_suppressed_channels().await?;

            for (url, message) in batch {
                match message {
                    RelayMessage::EndOfStoredEvents(subscription_id) => {
                        // A protocol-owed response, which is why it is safe to read success from it.
                        if let Some(channel_id) = channel_of_sub(subscription_id.as_str()) {
                            let channel_id = channel_id.to_string();
                            if let Some(entry) = self.live.get(&url) {
                                entry.engine.note_channel_served(&channel_id, now_unix())?;
                            }
                        }
                    }
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    } => {
                        if !is_ours(&subscription_id) {
                            continue;
                        }
                        ingested += 1;
                        self.apply_event(&url, &event).await?;
                        // Forward progress for this relay: the cursor has moved, so a later `Lagged` may
                        // safely discard again without re-asking for the window we just consumed.
                        if let Some(live) = self.live.get_mut(&url) {
                            live.progress_since_resync = live.progress_since_resync.saturating_add(1);
                        }
                    }
                    RelayMessage::Closed {
                        subscription_id,
                        message,
                    } => {
                        if !is_ours(&subscription_id) {
                            continue;
                        }
                        self.apply_closed(&url, &subscription_id, &message).await?;
                    }
                    _ => continue,
                }
            }

            if tokio::time::Instant::now() >= deadline {
                return Ok(ingested);
            }
            tokio::time::sleep(POLL_TICK).await;
        }
    }

    /// Notifications the client's broadcast buffer dropped before we read them. Non-zero means the
    /// pump is not being driven often enough, and that events were lost — never that none arrived.
    pub fn lagged(&self) -> u64 {
        self.lagged
    }

    /// Times a `Lagged` was NOT allowed to discard, because doing so would have re-asked for a window
    /// nothing had been consumed from yet, or because recovery was already running at its floor.
    ///
    /// Non-zero means events may have been skipped while a cursor advanced over them — a bounded loss
    /// taken deliberately in place of an unbounded one. It is reported rather than inferred, because a
    /// silent version of this is indistinguishable from a healthy relay.
    pub fn forced_progress(&self) -> u64 {
        self.forced_progress
    }

    /// Re-subscribe every channel whose suppression backoff has elapsed, one attempt each.
    ///
    /// The suppression stays raised until the relay answers with `EOSE`; this only sends the REQ and
    /// records that it is in flight.
    async fn retry_suppressed_channels(&mut self) -> Result<(), ParticipationError> {
        let now = now_unix();
        let mut retries: Vec<(String, String, Option<u64>)> = Vec::new();
        for (url, entry) in self.live.iter() {
            for channel_id in entry.engine.channels_to_retry(now)? {
                let since = entry.engine.channel_cursor(&channel_id)?;
                retries.push((url.clone(), channel_id, since));
            }
        }
        for (url, channel_id, since) in retries {
            let Some(entry) = self.live.get(&url) else {
                continue;
            };
            subscribe_channel(&entry.reader, &channel_id, self.me, since).await?;
        }
        Ok(())
    }

    /// Re-request every active filter on one relay from its durable cursor.
    ///
    /// This is the recovery half of a `Lagged` drop. The cursors are the only record of how far we
    /// got, and they are behind the events we lost, so re-subscribing from them makes the relay
    /// re-send the gap. Replay is safe by construction rather than by luck: owed rows are keyed on
    /// `event_id`, and membership is applied in author order, so a re-delivered event either changes
    /// nothing or corrects the ordering.
    async fn resync_relay(&mut self, url: &str) -> Result<(), ParticipationError> {
        let Some(entry) = self.live.get(url) else {
            return Ok(());
        };
        subscribe_wake_surface(&entry.reader, &entry.engine, self.me).await
    }

    async fn apply_event(&mut self, url: &str, event: &Event) -> Result<(), ParticipationError> {
        let actions = {
            let Some(entry) = self.live.get(url) else {
                return Ok(());
            };
            entry.engine.ingest(event, now_unix())?
        };

        for action in actions {
            let Some(entry) = self.live.get(url) else {
                return Ok(());
            };
            match action {
                Action::SubscribeChannel { channel_id } => {
                    let since = entry.engine.channel_cursor(&channel_id)?;
                    subscribe_channel(&entry.reader, &channel_id, self.me, since).await?;
                }
                Action::UnsubscribeChannel { channel_id } => {
                    entry
                        .reader
                        .unsubscribe(&SubscriptionId::new(channel_sub(&channel_id)))
                        .await;
                }
                // The job path owns trade events; participation's role is to not get in the way of
                // them. The gateway ingester reads them from its own marketplace subscription, so
                // there is nothing to hand over — this arm exists so that "a trade event reached
                // the social surface" is a case the code has an answer for.
                Action::ForwardToJobPath { .. } => {}
            }
        }
        Ok(())
    }

    async fn apply_closed(
        &mut self,
        url: &str,
        sub: &SubscriptionId,
        reason: &str,
    ) -> Result<(), ParticipationError> {
        let sub_string = sub.to_string();
        let channel_id = channel_of_sub(&sub_string).map(str::to_string);
        let action = {
            let Some(entry) = self.live.get(url) else {
                return Ok(());
            };
            entry
                .engine
                .on_closed(channel_id.clone(), reason, now_unix())?
        };

        let Some(entry) = self.live.get(url) else {
            return Ok(());
        };
        match action {
            ClosedAction::DropChannel { channel_id } => {
                entry
                    .reader
                    .unsubscribe(&SubscriptionId::new(channel_sub(&channel_id)))
                    .await;
            }
            ClosedAction::ResendAfter {
                channel_id,
                hint_secs,
            } => {
                // ★ The relay refused the REQ outright, so the subscription never existed on its
                // side and nothing is queued behind it. Waiting would wait forever; the REQ has to
                // go again. Backing off first is what keeps that from becoming a hot loop.
                tokio::time::sleep(Duration::from_secs(hint_secs.unwrap_or(30))).await;
                match channel_id {
                    Some(channel_id) => {
                        let since = entry.engine.channel_cursor(&channel_id)?;
                        subscribe_channel(&entry.reader, &channel_id, self.me, since).await?;
                    }
                    None => {
                        let since = entry.engine.membership_cursor()?;
                        subscribe_membership(&entry.reader, self.me, since).await?;
                    }
                }
            }
            ClosedAction::ReconnectRelay => {
                // The socket's authentication is what failed, so every subscription on it is
                // suspect. Reconnect, then rebuild the whole wake surface from the store rather
                // than from what we thought was subscribed.
                entry.reader.disconnect().await;
                entry.reader.connect().await;
                entry
                    .reader
                    .wait_for_connection(Duration::from_secs(10))
                    .await;

                // ★ RE-PROVE access before trusting the reconnected socket. An auth or scope refusal
                // is the relay saying our admission may no longer hold; resubscribing on that socket
                // because we were admitted MINUTES ago would keep sending traffic to a relay that has
                // revoked us. The admission was established by a positive probe, so it is re-checked
                // the same way — a stale `Admitted` is exactly what the roster exists to prevent.
                let outcome = probe::probe_access(
                    &self.publisher,
                    url,
                    &entry.reader,
                    &self.carrier,
                    self.probe_timeout,
                )
                .await;
                if !matches!(outcome, ProbeOutcome::EchoObserved) {
                    self.roster.record_probe(url, outcome, now_unix());
                    if let Some(entry) = self.live.remove(url) {
                        entry.reader.disconnect().await;
                    }
                    return Ok(());
                }
                subscribe_wake_surface(&entry.reader, &entry.engine, self.me).await?;
            }
            ClosedAction::Ignore => {}
        }
        Ok(())
    }

    /// Close every socket. Sockets are the only thing to release: all participation state is
    /// already durable, which is what makes a `kill -9` and a clean stop resume identically.
    pub async fn shutdown(self) {
        for (_, entry) in self.live {
            entry.reader.disconnect().await;
        }
    }
}

/// Open the client that publishes the access-probe carrier, connected to every configured relay.
///
/// It signs through the node's signer actor, exactly as the persona path does — the seller key stays
/// in the actor, which default-denies every kind outside its allowlist, so a holder of this client
/// cannot sign a trade-path event with it.
///
/// ⚠ This must stay a DIFFERENT [`Client`] from the readers in [`Participation`]. See
/// [`super::probe`]: a client cannot read back its own publishes, so a merged client turns the probe
/// into a function that denies every working relay.
pub async fn persona_publisher(
    signer: super::super::signer::SignerHandle,
    config: &ParticipationConfig,
) -> Result<Client, ParticipationError> {
    let adapter = super::super::buzz::NodeNostrSigner::new(signer)
        .map_err(|error| ParticipationError::Relay(format!("probe signer: {error}")))?;
    let client = Client::new(adapter);
    client.automatic_authentication(true);
    for url in &config.relays {
        client
            .add_relay(url)
            .await
            .map_err(|error| ParticipationError::Relay(format!("add relay {url}: {error}")))?;
    }
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(10)).await;
    Ok(client)
}

/// Open an authenticated reader for one relay.
///
/// NIP-42 is left to `automatic_authentication`, exactly as the persona path does it — the seller
/// key stays in the signer actor and is never handled here.
/// Open an authenticated reader for one relay, on the SAME identity as `publisher`.
///
/// ★ The reader must carry a signer. `automatic_authentication` can only answer a NIP-42 challenge
/// if there is something to sign with — a signer-less client sets the flag, stays **anonymous**, and
/// gets `auth-required:` on every REQ. It then reads nothing, forever, which the probe reports as
/// "admission unproven" and which is indistinguishable from a relay that revoked us. A fixture relay
/// that does not require NIP-42 cannot tell the two apart, which is how it got this far.
///
/// ★ And it must be THAT key. Admission is granted per-pubkey, so a reader on any other identity is
/// unadmitted even when it authenticates perfectly well.
///
/// Both are guaranteed here by lifting the signer off the publisher rather than accepting one: the
/// reader's identity cannot drift from the publisher's or from the carrier's author, because there
/// is no parameter through which it could.
///
/// Sharing the key does NOT reintroduce the own-echo trap — that trap is per `Client` **database**,
/// not per key, so a second Client on the same key still observes the first one's publishes. Two
/// clients is the requirement; two identities never was.
async fn connect_reader(url: &str, publisher: &Client) -> Result<Client, String> {
    let signer = publisher
        .signer()
        .await
        .map_err(|error| format!("the publisher has no signer to authenticate the reader: {error}"))?;
    let client = Client::new(signer);
    client.automatic_authentication(true);
    client
        .add_relay(url)
        .await
        .map_err(|error| format!("add relay {url}: {error}"))?;
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(10)).await;
    Ok(client)
}

/// Subscribe both filters of the wake surface, resuming each from its own stored cursor.
async fn subscribe_wake_surface(
    reader: &Client,
    engine: &Engine,
    me: PublicKey,
) -> Result<(), ParticipationError> {
    subscribe_membership(reader, me, engine.membership_cursor()?).await?;
    // Exactly the channels we were in when we stopped — read from the store, never re-derived from
    // the membership feed, which resumes past the notifications that admitted us.
    for channel_id in engine.channels_to_resume()? {
        let since = engine.channel_cursor(&channel_id)?;
        subscribe_channel(reader, &channel_id, me, since).await?;
    }
    Ok(())
}

async fn subscribe_membership(
    reader: &Client,
    me: PublicKey,
    since: Option<u64>,
) -> Result<(), ParticipationError> {
    reader
        .subscribe_with_id(
            SubscriptionId::new(MEMBERSHIP_SUB),
            dialect::membership_filter(me, since),
            None::<SubscribeAutoCloseOptions>,
        )
        .await
        .map(|_| ())
        .map_err(|error| ParticipationError::Relay(format!("membership subscribe: {error}")))
}

async fn subscribe_channel(
    reader: &Client,
    channel_id: &str,
    me: PublicKey,
    since: Option<u64>,
) -> Result<(), ParticipationError> {
    reader
        .subscribe_with_id(
            SubscriptionId::new(channel_sub(channel_id)),
            dialect::channel_mention_filter(channel_id, me, since),
            None::<SubscribeAutoCloseOptions>,
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            ParticipationError::Relay(format!("channel {channel_id} subscribe: {error}"))
        })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_subscription_id_round_trips_to_its_channel() {
        let sub = channel_sub("chan-1");
        assert_eq!(channel_of_sub(&sub), Some("chan-1"));
    }

    #[test]
    fn the_membership_subscription_is_not_mistaken_for_a_channel() {
        // A `CLOSED` on the global feed must not be read as "drop channel <nothing>".
        assert_eq!(channel_of_sub(MEMBERSHIP_SUB), None);
    }

    #[test]
    fn a_foreign_subscription_id_is_not_ours() {
        // The gateway's own marketplace subscription shares the socket on the mobee relay; a
        // CLOSED for it must not drop any of our channels.
        assert_eq!(channel_of_sub("mobee:offers"), None);
        assert_eq!(channel_of_sub("sub-42"), None);
    }
}
