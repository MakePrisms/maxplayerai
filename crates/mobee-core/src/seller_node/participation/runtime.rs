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

use std::collections::BTreeMap;
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
}

/// The node's live participation across all configured relays.
pub struct Participation {
    live: BTreeMap<String, Live>,
    /// Notifications dropped by a client's broadcast buffer before the pump read them.
    lagged: u64,
    roster: RelayRoster,
    me: PublicKey,
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

            let outcome = probe::probe_access(publisher, &reader, carrier, timeout).await;
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
                },
            );
        }

        Ok(Self {
            live,
            roster,
            me,
            lagged: 0,
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
                        // ★ The buffer overflowed and `skipped` notifications are GONE — not
                        // delayed, gone. Counted so the loss is reportable rather than invisible;
                        // a pump that swallowed this would look identical to a quiet relay.
                        Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                            self.lagged = self.lagged.saturating_add(skipped);
                            continue;
                        }
                    }
                }
            }

            for (url, message) in batch {
                match message {
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    } => {
                        if !is_ours(&subscription_id) {
                            continue;
                        }
                        ingested += 1;
                        self.apply_event(&url, &event).await?;
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
