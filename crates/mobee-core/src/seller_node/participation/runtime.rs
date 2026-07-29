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

/// Ceiling on a `CLOSED` retry hint, and the delay used when the relay gives none.
///
/// ★ The hint is a number a PEER chose. Honouring it unbounded lets any relay park participation for as
/// long as it likes by answering one REQ with `rate-limited: retry in 86400`. A cap makes the worst case
/// ours to choose; the floor of 1s keeps a `retry in 0` from becoming a hot loop.
const MAX_RESEND_DELAY_SECS: u64 = 300;
const DEFAULT_RESEND_DELAY_SECS: u64 = 30;

/// How long a reconnected socket is given to settle before access is re-proven on it.
///
/// This was a `wait_for_connection` blocking the pump. The duration is the same; what changed is that
/// the pump is free during it.
const RECONNECT_SETTLE_SECS: i64 = 10;

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
    /// A resync was owed but deferred by the floor interval. Held as STATE, because a throttled
    /// recovery that is merely dropped is a recovery that never happened.
    resync_pending: bool,
    /// Subscriptions a `CLOSED` asked us to re-send, and the earliest tick that may do it. `None` keys the
    /// membership filter; `Some(channel)` a channel filter.
    ///
    /// ★ A DEADLINE THE PUMP CHECKS, never an inline sleep. Waiting inside the batch loop blocks every
    /// other relay's traffic for a duration a peer chose, so one relay's `retry in 86400` stopped all
    /// participation. In-memory is fine for the same reason `resync_pending` is: a restart rebuilds the
    /// whole wake surface from durable cursors, which is strictly more than a pending resend would do.
    resend_due: BTreeMap<Option<String>, i64>,
    /// Set when this relay's socket was kicked for an auth failure: the tick at which its access may be
    /// re-proven. `None` means nothing is owed.
    reprove_due: Option<i64>,
}

/// The node's live participation across all configured relays.
pub struct Participation {
    live: BTreeMap<String, Live>,
    /// Notifications dropped by a client's broadcast buffer before the pump read them.
    lagged: u64,
    /// Times a `Lagged` was not permitted to discard — see [`Participation::forced_progress`].
    forced_progress: u64,
    /// Wire failures isolated to one relay or channel — see [`Participation::relay_faults`].
    relay_faults: u64,
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
            // No REQ we sent before this process started is still outstanding, so any retry marked in
            // flight is a phantom: truthful about the past, meaningless now. Cleared rather than expired —
            // nothing was refused, we simply never got to ask.
            engine.forget_retries_in_flight(now_unix())?;
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
                    resync_pending: false,
                    resend_due: BTreeMap::new(),
                    reprove_due: None,
                },
            );
        }

        Ok(Self {
            live,
            roster,
            me,
            lagged: 0,
            forced_progress: 0,
            relay_faults: 0,
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
            // ★★ Discard-safety and resync-TIMING are separate questions, and conflating them cost
            // correctness. Discarding is safe whenever forward progress exists — the cursor moved, so a
            // re-ask cannot repeat the same window. Whether the REQ may go out *yet* is a rate question.
            // Treating "too soon to re-ask" as "unsafe to discard" meant processing traffic from beyond a
            // gap for no reason other than a clock, which is the loss the discard exists to prevent.
            //
            // So: progress ⇒ always discard and replace the receiver; the resync REQ either goes now or is
            // marked pending and sent once the floor has passed. Only a genuine absence of progress forces
            // us to process past a gap, and that is what `forced_progress` counts.
            let now = now_unix();
            let mut discard: BTreeSet<String> = BTreeSet::new();
            for url in &resync {
                let Some(live) = self.live.get_mut(url) else {
                    continue;
                };
                if live.progress_since_resync == 0 {
                    self.forced_progress = self.forced_progress.saturating_add(1);
                    continue;
                }
                live.progress_since_resync = 0;
                live.notifications = live.reader.notifications();
                discard.insert(url.clone());
                // OWED, unconditionally. The floor decides only WHEN the REQ goes out; the marker comes
                // down where the REQ is confirmed sent, never here on the way in.
                live.resync_pending = true;
            }
            batch.retain(|(url, _)| !discard.contains(url));

            // Every owed resync whose floor has passed — including ones deferred on an earlier tick, which
            // are held as state rather than dropped, or a throttled recovery would be a recovery that never
            // happened. ★ This drain is deliberately OUTSIDE the `resync` branch: a deferred resync must
            // fire once the floor elapses whether or not another lag ever arrives. Moving it under the
            // lagged branch would turn "held" back into "never".
            let due_resyncs: Vec<String> = self
                .live
                .iter()
                .filter(|(_, live)| {
                    live.resync_pending
                        && now.saturating_sub(live.last_resync_unix) >= MIN_RESYNC_INTERVAL_SECS
                })
                .map(|(url, _)| url.clone())
                .collect();
            for url in &due_resyncs {
                // ★ ISOLATED TO THE RELAY THAT FAILED. `?` here aborted the whole pump before the drained
                // batch was processed, so one permanently-failing relay — which fails FIRST on every tick,
                // because `due_resyncs` is ordered — discarded every healthy relay's messages and starved
                // their retries forever. Being conservative on failure is necessary and not sufficient:
                // an error path that halts the loop turns one dead peer into a fleet outage.
                let outcome = self.resync_relay(url).await;
                if outcome.is_err() {
                    self.relay_faults = self.relay_faults.saturating_add(1);
                }
                if let Some(live) = self.live.get_mut(url) {
                    // ★★ The FLOOR records that an ATTEMPT happened, so it advances either way. Leaving it
                    // untouched on failure — which is what "on error both stay put" did — re-asks a dead
                    // relay at pump frequency, the hot loop the floor exists to prevent. The MARKER records
                    // whether the recovery HAPPENED, so only success clears it. Rate and debt are separate
                    // questions about the same event, and a failed attempt answers only the first.
                    live.last_resync_unix = now;
                    if outcome.is_ok() {
                        live.resync_pending = false;
                    }
                }
            }

            // Re-send subscriptions a `CLOSED` deferred, now that their capped deadline has passed. Grouped
            // with the resync because both are owed WIRE RECOVERY and neither arms an attribution token —
            // which is why they may run before the batch while `retry_suppressed_channels` may not.
            self.drain_resends(now).await;
            self.drain_reproves(now).await;

            // ★ THE `?`s BELOW CARRY ONLY `Store` ERRORS, BY CONSTRUCTION — and that is a property to
            // re-check, not to trust, because it is what keeps one relay from emptying this batch.
            // A `Relay` error here would abort mid-batch and drop every message after it, INCLUDING other
            // relays'. Rather than swallow one at the loop, every wire call reachable from here records a
            // debt and returns `Ok`, so there is nothing to swallow: `apply_event` turns a failed subscribe
            // into a `resend_due` entry, and `apply_closed` turns a reconnect into a `reprove_due` deadline.
            // What remains is `ingest`, `on_closed` and the cursor reads — all `StoreError`, i.e. OUR
            // persistence failing, which is nobody's peer fault and not a thing to carry on through.
            // ⚠ Adding a wire call to `apply_event` or `apply_closed` breaks this. Give it a debt to record.
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
                            live.progress_since_resync =
                                live.progress_since_resync.saturating_add(1);
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

            // Retry channels whose suppression backoff has elapsed. This is the exit from a refusal whose
            // only other way out is a membership event the relay has no reason to send.
            //
            // ★ AFTER the batch, and that ordering is the whole correctness of it. `note_retry_attempt`
            // arms `retry_started_unix`, which is what makes an `EOSE` or a `CLOSED` attributable to a REQ
            // of ours — so anything ALREADY IN HAND when the token arms becomes falsely attributable to a
            // retry it predates. An `EOSE` buffered from the pre-suppression subscription would clear the
            // flag outright; a duplicate `CLOSED` would charge a backoff step to a retry it never saw. The
            // arming has to come after everything that could be mistaken for its answer has been judged
            // against the state that was true when it arrived.
            self.retry_suppressed_channels().await?;

            if tokio::time::Instant::now() >= deadline {
                return Ok(ingested);
            }
            tokio::time::sleep(POLL_TICK).await;
        }
    }

    /// A `Participation` whose only relay is fed by a caller-supplied notification channel.
    ///
    /// ★ THE LAG SEAM (issue #235). `Lagged` is the one branch in [`Self::pump`] that no fixture could
    /// reach: overflowing a real relay's broadcast buffer is not something a test can do on demand, so
    /// three separate defects on this path were found by review and none by a test — and a verifier then
    /// proved the recovery logic had NO teeth at all, because deleting the progress precondition left every
    /// test green. A branch that cannot be reached by a test is not covered by having code that looks right.
    ///
    /// The relay client here is never connected. That is deliberate: the assertions are about which
    /// messages are DISCARDED and whether a resync is DEFERRED, and both are decided before any REQ goes
    /// out. Set `last_resync_unix` to `now` so the resync is throttled and no I/O is attempted.
    #[cfg(test)]
    fn for_lag_test(
        url: &str,
        store: SellerStore,
        me: PublicKey,
        notifications: broadcast::Receiver<RelayPoolNotification>,
        last_resync_unix: i64,
        progress_since_resync: u64,
    ) -> Self {
        let reader = Client::default();
        let mut live = BTreeMap::new();
        live.insert(
            url.to_string(),
            Live {
                reader,
                notifications,
                engine: Engine::new(store, url.to_string(), me),
                progress_since_resync,
                last_resync_unix,
                resync_pending: false,
                resend_due: BTreeMap::new(),
                reprove_due: None,
            },
        );
        Self {
            live,
            lagged: 0,
            forced_progress: 0,
            relay_faults: 0,
            roster: RelayRoster::new(vec![url.to_string()]),
            me,
            publisher: Client::default(),
            carrier: nostr_sdk::prelude::EventBuilder::new(
                nostr_sdk::prelude::Kind::Metadata,
                "{}",
            )
            .sign_with_keys(&nostr_sdk::prelude::Keys::generate())
            .expect("sign carrier"),
            probe_timeout: Duration::from_secs(1),
        }
    }

    /// A `Participation` whose relay client HOLDS a relay, so a test can observe what the pump does around a
    /// REQ rather than only what it decides before one.
    ///
    /// ★ [`Self::for_lag_test`]'s client holds no relays at all, so every subscribe fails at the POOL. That
    /// is the right fixture for "what survives a failed recovery" and the wrong one for anything about
    /// ordering: a pump that aborts on the first subscribe would pass an ordering test by never reaching the
    /// code the test is about — a green that means nothing, which is this module's recurring trap.
    ///
    /// The two modes here are the two halves of [`super::undelivered`], and having both is what makes "a pool
    /// `Ok` is not delivery" a tested claim instead of an asserted one:
    ///
    /// - `accepted = true` calls `connect`, which sets the relay's status to `Pending` SYNCHRONOUSLY and
    ///   hands REQs to its send channel. The pool reports the url in `success`, exactly as a live relay would.
    ///   Nothing is delivered anywhere — the address does not resolve — and that is fine: accepted by a relay
    ///   is the strongest claim a REQ can make at this layer, and `EOSE` or
    ///   [`SellerStore::expire_stale_retries`] settles the rest.
    /// - `accepted = false` leaves the relay un-connected, so it refuses the REQ with `NotReady` while the
    ///   POOL STILL RETURNS `Ok` — the failure recorded only in `output.failed`. That is the whole finding:
    ///   a success value with nothing behind it.
    #[cfg(test)]
    async fn for_wire_test(
        url: &str,
        store: SellerStore,
        me: PublicKey,
        notifications: broadcast::Receiver<RelayPoolNotification>,
        accepted: bool,
    ) -> Self {
        Self::for_wire_test_many(vec![(url, accepted, notifications)], store, me, now_unix()).await
    }

    /// SEVERAL relays in one pump, each with its own notification channel and its own verdict on whether
    /// the pool will accept a REQ.
    ///
    /// ★ THE FAULT-ISOLATION FIXTURE. A single-relay pump cannot distinguish "the failure was isolated"
    /// from "the failure stopped everything", because there is nothing else left to stop — every
    /// isolation test on one relay is green by construction. It takes a failing relay and a healthy relay
    /// in the SAME pump, and the failing one has to sort FIRST: the due lists are ordered by url, so a
    /// relay that fails is deterministically reached before the relay that would have worked. That
    /// ordering is the whole reason one dead peer could starve the rest.
    #[cfg(test)]
    async fn for_wire_test_many(
        relays: Vec<(&str, bool, broadcast::Receiver<RelayPoolNotification>)>,
        store: SellerStore,
        me: PublicKey,
        last_resync_unix: i64,
    ) -> Self {
        let mut live = BTreeMap::new();
        let mut urls = Vec::new();
        for (url, accepted, notifications) in relays {
            let reader = Client::default();
            reader.add_relay(url).await.expect("add relay");
            if accepted {
                reader.connect().await;
            }
            urls.push(url.to_string());
            live.insert(
                url.to_string(),
                Live {
                    reader,
                    notifications,
                    engine: Engine::new(store.clone(), url.to_string(), me),
                    progress_since_resync: 1,
                    last_resync_unix,
                    resync_pending: false,
                    resend_due: BTreeMap::new(),
                    reprove_due: None,
                },
            );
        }
        Self {
            live,
            lagged: 0,
            forced_progress: 0,
            relay_faults: 0,
            roster: RelayRoster::new(urls),
            me,
            publisher: Client::default(),
            carrier: nostr_sdk::prelude::EventBuilder::new(
                nostr_sdk::prelude::Kind::Metadata,
                "{}",
            )
            .sign_with_keys(&nostr_sdk::prelude::Keys::generate())
            .expect("sign carrier"),
            probe_timeout: Duration::from_secs(1),
        }
    }

    /// Mark a resync owed, the state a `Lagged` with forward progress leaves behind. The lag-to-owed path
    /// is covered by its own tests; this states the precondition so a test can be about the DRAIN.
    #[cfg(test)]
    fn owe_resync(&mut self, url: &str) {
        if let Some(live) = self.live.get_mut(url) {
            live.resync_pending = true;
        }
    }

    /// When this relay may next be resynced, so a test can tell "throttled" from "hot loop".
    #[cfg(test)]
    fn last_resync_unix(&self, url: &str) -> i64 {
        self.live.get(url).map_or(0, |live| live.last_resync_unix)
    }

    /// The deadline stored for a deferred re-send, if one is owed.
    #[cfg(test)]
    fn resend_deadline(&self, url: &str, channel_id: Option<&str>) -> Option<i64> {
        self.live
            .get(url)?
            .resend_due
            .get(&channel_id.map(str::to_string))
            .copied()
    }

    /// The deadline at which this relay's access will be re-proven after a reconnect, if one is owed.
    #[cfg(test)]
    fn reprove_deadline(&self, url: &str) -> Option<i64> {
        self.live.get(url).and_then(|live| live.reprove_due)
    }

    /// Whether a resync is deferred for this relay, waiting on the floor interval.
    #[cfg(test)]
    fn resync_pending(&self, url: &str) -> bool {
        self.live.get(url).is_some_and(|live| live.resync_pending)
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

    /// Wire failures that were isolated to one relay or channel instead of stopping the pump.
    ///
    /// A relay cannot be allowed to halt participation for the others — [`Self::start`] already refuses to
    /// fail because of one, and the pump has to hold the same line or a single dead peer becomes an outage.
    /// Isolation without a count would be indistinguishable from health, so it is reported: non-zero means
    /// some relay is not taking our REQs, and a rising number means it still is not.
    pub fn relay_faults(&self) -> u64 {
        self.relay_faults
    }

    /// Re-send the subscriptions a `CLOSED` deferred, for every relay whose deadline has passed.
    ///
    /// Failures isolate per subscription: the entry stays owed and its deadline is pushed out by the resync
    /// floor, so a relay that keeps refusing is re-asked at a bounded rate rather than every tick.
    async fn drain_resends(&mut self, now: i64) {
        let due: Vec<(String, Option<String>)> = self
            .live
            .iter()
            .flat_map(|(url, live)| {
                live.resend_due
                    .iter()
                    .filter(|(_, deadline)| now >= **deadline)
                    .map(move |(target, _)| (url.clone(), target.clone()))
            })
            .collect();

        for (url, target) in due {
            let Some(entry) = self.live.get(&url) else {
                continue;
            };
            let outcome = match &target {
                Some(channel_id) => match entry.engine.channel_cursor(channel_id) {
                    Ok(since) => subscribe_channel(&entry.reader, channel_id, self.me, since).await,
                    // A store read failing is not a relay fault, but it is also not a reason to stop
                    // serving other relays. Count it and move on; the entry stays owed.
                    Err(error) => Err(ParticipationError::Store(error)),
                },
                None => match entry.engine.membership_cursor() {
                    Ok(since) => subscribe_membership(&entry.reader, self.me, since).await,
                    Err(error) => Err(ParticipationError::Store(error)),
                },
            };
            let Some(live) = self.live.get_mut(&url) else {
                continue;
            };
            if outcome.is_ok() {
                live.resend_due.remove(&target);
            } else {
                self.relay_faults = self.relay_faults.saturating_add(1);
                live.resend_due
                    .insert(target, now.saturating_add(MIN_RESYNC_INTERVAL_SECS));
            }
        }
    }

    /// Re-prove access on ONE relay whose reconnect has settled, and rebuild its wake surface.
    ///
    /// ★ RE-PROVE, never assume. An auth or scope refusal is the relay saying the admission we hold may no
    /// longer be valid; resubscribing on that socket because we were admitted minutes ago keeps sending
    /// traffic to a relay that has revoked us. Admission was established by a positive probe, so it is
    /// re-checked the same way — a stale `Admitted` is exactly what the roster exists to prevent.
    ///
    /// ★★ ONE PER TICK, on purpose. `probe_access` blocks for up to `probe_timeout`, and this is the last
    /// place in the pump that waits on a relay at all. Bounded per call is not bounded per tick: N relays
    /// re-proving together would serialise N timeouts while every healthy relay's traffic waited. Taking one
    /// caps the pump's exposure at a single probe regardless of how many relays failed at once; the rest keep
    /// their deadline and are picked up on later ticks.
    async fn drain_reproves(&mut self, now: i64) {
        let Some(url) = self
            .live
            .iter()
            .find(|(_, live)| live.reprove_due.is_some_and(|at| now >= at))
            .map(|(url, _)| url.clone())
        else {
            return;
        };
        // `Client` is `Arc`-backed, so cloning the handle costs a refcount and frees the borrow on
        // `self.live` — which matters because the denial path REMOVES the entry.
        let Some(reader) = self.live.get(&url).map(|live| live.reader.clone()) else {
            return;
        };

        let outcome = probe::probe_access(
            &self.publisher,
            &url,
            &reader,
            &self.carrier,
            self.probe_timeout,
        )
        .await;
        if !matches!(outcome, ProbeOutcome::EchoObserved) {
            self.roster.record_probe(&url, outcome, now);
            if let Some(entry) = self.live.remove(&url) {
                entry.reader.disconnect().await;
            }
            return;
        }

        // ★ Cleared because the thing the deadline was FOR — re-proving access — has now happened. This is
        // not the arm-on-the-way-in mistake: the wake surface is a separate debt, and it gets its own
        // marker below rather than riding on this one.
        if let Some(live) = self.live.get_mut(&url) {
            live.reprove_due = None;
        }
        let Some(entry) = self.live.get(&url) else {
            return;
        };
        if subscribe_wake_surface(&entry.reader, &entry.engine, self.me)
            .await
            .is_err()
        {
            self.relay_faults = self.relay_faults.saturating_add(1);
            if let Some(live) = self.live.get_mut(&url) {
                live.resync_pending = true;
            }
        }
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
            // ★ ARM AFTER THE SEND. `retry_started_unix` asserts "a REQ of ours is outstanding" — the fact
            // that makes an `EOSE` or a `CLOSED` attributable to us at all. Setting it before the subscribe
            // returned marked one in flight for a REQ that never left: the channel went ineligible for the
            // whole timeout and was then charged a backoff step for a failure that was ours, not the
            // relay's. Selecting a channel and claiming to have asked it are different facts.
            //
            // ★★ ISOLATED PER CHANNEL. `?` here abandoned every channel queued behind the first failure —
            // and the failing one is deterministically first, because it stays due while the others never
            // get asked. One dead relay silently stopped the retry path for all of them. Leaving the token
            // unarmed is right; leaving the loop is not.
            match subscribe_channel(&entry.reader, &channel_id, self.me, since).await {
                Ok(()) => entry.engine.note_retry_sent(&channel_id, now)?,
                Err(_) => {
                    self.relay_faults = self.relay_faults.saturating_add(1);
                    // Not escalated — the failure is ours — but the wait restarts, or this channel is
                    // re-attempted on every tick. See `SellerStore::note_retry_send_failed`.
                    entry.engine.note_retry_send_failed(&channel_id, now)?;
                }
            }
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
                    let sent = subscribe_channel(&entry.reader, &channel_id, self.me, since).await;
                    if sent.is_err() {
                        // ★ OWED, not raised. `?` here aborted the batch, so one relay's failed subscribe
                        // discarded every message behind it — other relays' included.
                        //
                        // ★★ And a debt is needed, not just isolation: the store already says `joined`
                        // (`ingest` commits before returning actions), so the channel would sit joined with
                        // nothing listening. `joined_channels` still lists it, but only a RESTART or a
                        // lag-triggered resync re-subscribes — in a healthy long-running process neither
                        // happens, and the invite goes unanswered forever. The resend deadline repairs it,
                        // reusing the machinery the `CLOSED` path already needed.
                        self.relay_faults = self.relay_faults.saturating_add(1);
                        if let Some(live) = self.live.get_mut(url) {
                            live.resend_due.insert(
                                Some(channel_id),
                                now_unix().saturating_add(MIN_RESYNC_INTERVAL_SECS),
                            );
                        }
                    }
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
                //
                // ★★ RECORDED AS A DEADLINE, NEVER SLEPT ON. This ran `tokio::time::sleep` inline, on a
                // duration the RELAY chose, uncapped — inside the batch loop, so `rate-limited: retry in
                // 86400` from any single relay stopped all participation on every relay for a day. Two
                // separate faults in one line: blocking the shared loop, and letting a peer pick how long.
                // Now the delay is clamped to a ceiling we own and left for the pump to notice.
                let delay = hint_secs
                    .unwrap_or(DEFAULT_RESEND_DELAY_SECS)
                    .clamp(1, MAX_RESEND_DELAY_SECS);
                if let Some(live) = self.live.get_mut(url) {
                    live.resend_due
                        .insert(channel_id, now_unix().saturating_add(delay as i64));
                }
            }
            ClosedAction::ReconnectRelay => {
                // The socket's authentication is what failed, so every subscription on it is suspect.
                // Kick it, and come back for the PROOF on a deadline.
                //
                // ★ THE SETTLE IS NO LONGER A BLOCK. `disconnect`/`connect` return promptly (`connect`
                // sets the relay `Pending` synchronously and spawns its own task); what used to follow was
                // `wait_for_connection(10s)` plus a probe timeout, run inline in the batch loop. Both are
                // bounded and self-chosen, so neither is the peer-controlled hazard the `CLOSED` hint was —
                // but they MULTIPLY: several relays answering auth-failure in one tick stalled the pump by
                // 10s each, and every healthy relay's traffic waited behind all of them. The settle is now
                // a deadline the pump notices, and `drain_reproves` takes ONE relay per tick so the probe
                // timeout cannot multiply either.
                entry.reader.disconnect().await;
                entry.reader.connect().await;
                if let Some(live) = self.live.get_mut(url) {
                    live.reprove_due = Some(now_unix().saturating_add(RECONNECT_SETTLE_SECS));
                }
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
    let output = reader
        .subscribe_with_id(
            SubscriptionId::new(MEMBERSHIP_SUB),
            dialect::membership_filter(me, since),
            None::<SubscribeAutoCloseOptions>,
        )
        .await
        .map_err(|error| ParticipationError::Relay(format!("membership subscribe: {error}")))?;
    // `Ok` from the pool is not delivery — see [`super::undelivered`].
    match super::undelivered(&output) {
        Some(why) => Err(ParticipationError::Relay(format!(
            "membership subscribe reached no relay: {why}"
        ))),
        None => Ok(()),
    }
}

async fn subscribe_channel(
    reader: &Client,
    channel_id: &str,
    me: PublicKey,
    since: Option<u64>,
) -> Result<(), ParticipationError> {
    let output = reader
        .subscribe_with_id(
            SubscriptionId::new(channel_sub(channel_id)),
            dialect::channel_mention_filter(channel_id, me, since),
            None::<SubscribeAutoCloseOptions>,
        )
        .await
        .map_err(|error| {
            ParticipationError::Relay(format!("channel {channel_id} subscribe: {error}"))
        })?;
    // `Ok` from the pool is not delivery — see [`super::undelivered`]. This one gates
    // `note_retry_attempt`, so a false success here arms the attribution token on nothing.
    match super::undelivered(&output) {
        Some(why) => Err(ParticipationError::Relay(format!(
            "channel {channel_id} subscribe reached no relay: {why}"
        ))),
        None => Ok(()),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::channel_filter_id;
    use super::*;
    use nostr_sdk::prelude::{EventBuilder as TestEventBuilder, Keys, Kind};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    const TEST_RELAY: &str = "wss://relay.invalid";

    fn test_store(label: &str) -> SellerStore {
        let id = SEQ.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "mobee-participation-lag-{label}-{}-{id}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        SellerStore::open(path).expect("open store")
    }

    /// Overflow a small broadcast channel so the receiver is guaranteed to report `Lagged`.
    fn lagged_receiver(
        capacity: usize,
    ) -> (
        broadcast::Sender<RelayPoolNotification>,
        broadcast::Receiver<RelayPoolNotification>,
    ) {
        let (sender, receiver) = broadcast::channel(capacity);
        let event = TestEventBuilder::new(Kind::Metadata, "{}")
            .sign_with_keys(&Keys::generate())
            .expect("sign");
        // One more than the buffer holds: the oldest is evicted, so the next read is a `Lagged`.
        for _ in 0..capacity + 1 {
            let _ = sender.send(RelayPoolNotification::Message {
                relay_url: TEST_RELAY.parse().expect("url"),
                message: RelayMessage::Event {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(channel_sub(
                        "chan-1",
                    ))),
                    event: std::borrow::Cow::Owned(event.clone()),
                },
            });
        }
        (sender, receiver)
    }

    /// ★ With forward progress, a lag DISCARDS — and when the resync is merely too soon, it is DEFERRED
    /// rather than abandoned. Discard-safety and resync-timing are separate concerns; conflating them meant
    /// processing traffic from beyond a gap because of a clock.
    #[tokio::test]
    async fn a_lag_with_progress_discards_and_defers_a_throttled_resync() {
        let (_sender, receiver) = lagged_receiver(2);
        let store = test_store("progress");
        // `last_resync_unix = now` ⇒ the resync is inside the floor, so it must be deferred, not sent.
        let mut participation = Participation::for_lag_test(
            TEST_RELAY,
            store,
            Keys::generate().public_key(),
            receiver,
            now_unix(),
            1,
        );

        let ingested = participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert_eq!(
            ingested, 0,
            "messages from beyond the gap were processed instead of discarded"
        );
        assert!(
            participation.lagged() > 0,
            "the lag was not observed at all, so this test proves nothing"
        );
        assert_eq!(
            participation.forced_progress(),
            0,
            "progress existed, so nothing should have been forced past the gap"
        );
        assert!(
            participation.resync_pending(TEST_RELAY),
            "a resync blocked by the floor was dropped instead of deferred — recovery never happens"
        );
    }

    /// ★ Without forward progress, discarding again would re-ask for a window nothing was consumed from —
    /// the livelock. So the batch is processed instead, and that concession is COUNTED rather than silent.
    #[tokio::test]
    async fn a_lag_with_no_progress_is_forced_through_and_counted() {
        let (_sender, receiver) = lagged_receiver(2);
        let store = test_store("no-progress");
        let mut participation = Participation::for_lag_test(
            TEST_RELAY,
            store,
            Keys::generate().public_key(),
            receiver,
            now_unix(),
            0,
        );

        let ingested = participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            participation.lagged() > 0,
            "the lag was not observed at all, so this test proves nothing"
        );
        assert_eq!(
            participation.forced_progress(),
            1,
            "a lag with no progress must be counted, or the concession is silent"
        );
        assert!(
            ingested > 0,
            "nothing was processed and nothing was discarded — the pump made no progress at all, \
             which is the livelock this branch exists to avoid"
        );
        assert!(!participation.resync_pending(TEST_RELAY));
    }

    /// ★ A resync that FAILED to go out is still owed. Clearing the marker and advancing the floor before the
    /// subscribe returned threw the recovery away on any error: nothing remembered the gap needed repairing,
    /// so the hole the lag opened became permanent.
    #[tokio::test]
    async fn a_resync_that_failed_to_send_is_still_owed() {
        let (_sender, receiver) = lagged_receiver(2);
        let store = test_store("resync-failure");
        // `last_resync_unix = 0` ⇒ the floor has passed, so the resync is attempted for real. The seam's
        // client holds no relays, so the subscribe fails — the fixture IS the failure injection.
        let mut participation = Participation::for_lag_test(
            TEST_RELAY,
            store,
            Keys::generate().public_key(),
            receiver,
            0,
            1,
        );

        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("a relay fault is isolated and counted, never raised as a pump error");

        assert_eq!(
            participation.relay_faults(),
            1,
            "the resync succeeded, so this test says nothing about what happens when it fails"
        );
        assert!(
            participation.lagged() > 0,
            "the lag was not observed at all, so this test proves nothing"
        );
        assert!(
            participation.resync_pending(TEST_RELAY),
            "a resync that never reached the wire was forgotten — the marker came down before the REQ was \
             confirmed sent, so nothing will ever repair the gap"
        );
    }

    /// ★ An `EOSE` already in hand when the retry token arms PREDATES that retry, so it cannot be its answer.
    /// Arming before the drained batch is judged makes every stale signal in that batch falsely attributable —
    /// and this one lifts a suppression with nothing retried at all.
    #[tokio::test]
    async fn a_stale_eose_is_judged_before_the_retry_token_arms() {
        let store = test_store("stale-eose");
        let long_ago = now_unix() - 3_600;
        store
            .record_channel_joined(
                TEST_RELAY,
                "chan-1",
                &channel_filter_id("chan-1"),
                &"a".repeat(64),
                long_ago,
                long_ago,
            )
            .expect("join");
        // Suppressed long enough ago that the retry is due on this very tick.
        store
            .advance_suppression(TEST_RELAY, "chan-1", long_ago)
            .expect("refused");

        // An `EOSE` for the channel, buffered from the subscription that existed BEFORE the suppression.
        let (sender, receiver) = broadcast::channel(8);
        sender
            .send(RelayPoolNotification::Message {
                relay_url: TEST_RELAY.parse().expect("url"),
                message: RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(
                    SubscriptionId::new(channel_sub("chan-1")),
                )),
            })
            .expect("send eose");

        let mut participation = Participation::for_wire_test(
            TEST_RELAY,
            store.clone(),
            Keys::generate().public_key(),
            receiver,
            true,
        )
        .await;
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            store
                .suppressed_channels_due(TEST_RELAY, now_unix())
                .expect("due")
                .is_empty(),
            "no retry armed this tick, so the test never reached the ordering it is about"
        );
        assert!(
            store
                .channel_suppressed(TEST_RELAY, "chan-1")
                .expect("read"),
            "a stale EOSE lifted the suppression — it was already in hand when the retry token armed, so it \
             answers a REQ that did not exist when it arrived"
        );
    }

    /// ★ A pool `Ok` is ACCEPTANCE, NOT DELIVERY. `subscribe_with_id` returns `Ok` whenever the pool knows the
    /// relay; a relay that would not take the REQ is reported in `output.failed` and nowhere the `Result` can
    /// see. Arming `retry_started_unix` off that value marks a retry in flight for a REQ nobody sent — the
    /// channel then sits ineligible for the whole timeout and is charged a backoff step for our own failure.
    #[tokio::test]
    async fn a_req_no_relay_accepted_does_not_arm_the_retry_token() {
        let store = test_store("unaccepted-req");
        let long_ago = now_unix() - 3_600;
        store
            .record_channel_joined(
                TEST_RELAY,
                "chan-1",
                &channel_filter_id("chan-1"),
                &"a".repeat(64),
                long_ago,
                long_ago,
            )
            .expect("join");
        store
            .advance_suppression(TEST_RELAY, "chan-1", long_ago)
            .expect("refused");

        let (_sender, receiver) = broadcast::channel(8);
        // `accepted = false`: the relay is known to the pool but never connected, so it refuses with
        // `NotReady` while the pool itself still answers `Ok`.
        let mut participation = Participation::for_wire_test(
            TEST_RELAY,
            store.clone(),
            Keys::generate().public_key(),
            receiver,
            false,
        )
        .await;

        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("a relay fault is isolated and counted, never raised as a pump error");

        assert_eq!(
            participation.relay_faults(),
            1,
            "an un-connected relay took the REQ, so this test says nothing about a rejected one — and a \
             pool `Ok` with an empty success set is exactly the failure that used to be invisible"
        );
        // Due after ONE backoff and still at attempt 1: the token never armed (an armed retry is excluded
        // from the due list entirely) and our own dead socket earned the relay no escalation.
        assert_eq!(
            store
                .suppressed_channels_due(TEST_RELAY, now_unix() + 60)
                .expect("due"),
            vec![("chan-1".to_string(), 1)],
            "the retry token armed for a REQ no relay accepted — the channel is now un-retryable until the \
             timeout expires a retry that never happened"
        );
    }

    // ★ The failing relay sorts BEFORE the healthy one on purpose: due lists are ordered by url, so this
    // is the arrangement in which one dead peer reaches the loop first and starves everything behind it.
    const BAD_RELAY: &str = "wss://a-bad.invalid";
    const GOOD_RELAY: &str = "wss://b-good.invalid";

    fn joined_and_suppressed(store: &SellerStore, relay: &str, channel_id: &str, when: i64) {
        store
            .record_channel_joined(
                relay,
                channel_id,
                &channel_filter_id(channel_id),
                &"a".repeat(64),
                when,
                when,
            )
            .expect("join");
        store
            .advance_suppression(relay, channel_id, when)
            .expect("refused");
    }

    /// ★ FINDING 1. A failed resync must not take the pump down with it. The bad relay is reached first, so
    /// `?` there discarded the healthy relay's already-drained batch and starved its retries forever.
    #[tokio::test]
    async fn one_relays_failed_resync_does_not_stop_the_others() {
        let store = test_store("resync-isolation");
        let me = Keys::generate().public_key();
        let (_bad_tx, bad_rx) = broadcast::channel(8);
        let (good_tx, good_rx) = broadcast::channel(8);

        // A message the healthy relay has already handed us. It must still be processed.
        let event = TestEventBuilder::new(Kind::Metadata, "{}")
            .sign_with_keys(&Keys::generate())
            .expect("sign");
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Event {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(channel_sub(
                        "chan-1",
                    ))),
                    event: std::borrow::Cow::Owned(event),
                },
            })
            .expect("send");

        // `last_resync_unix = 0` ⇒ both floors have passed, so both resyncs are attempted this tick.
        let mut participation = Participation::for_wire_test_many(
            vec![(BAD_RELAY, false, bad_rx), (GOOD_RELAY, true, good_rx)],
            store,
            me,
            0,
        )
        .await;
        participation.owe_resync(BAD_RELAY);
        participation.owe_resync(GOOD_RELAY);

        let ingested = participation
            .pump(Duration::from_millis(1))
            .await
            .expect("a relay fault must not surface as a pump error");

        assert_eq!(
            participation.relay_faults(),
            1,
            "the bad relay's failure was not observed, so this test proves nothing about isolating it"
        );
        assert!(
            !participation.resync_pending(GOOD_RELAY),
            "the healthy relay's resync never ran — the failing relay, which sorts first, aborted the loop"
        );
        assert!(
            participation.resync_pending(BAD_RELAY),
            "the failed resync stopped being owed, so nothing will ever repair that relay's gap"
        );
        assert!(
            ingested > 0,
            "the healthy relay's already-drained batch was dropped because another relay failed"
        );
    }

    /// ★ FINDING 1, second half. Keeping the marker is right; keeping the FLOOR is not. A relay that always
    /// fails would be re-asked at pump frequency — the hot loop the floor exists to prevent.
    #[tokio::test]
    async fn a_failed_resync_still_advances_the_rate_floor() {
        let store = test_store("resync-floor");
        let (_tx, rx) = broadcast::channel(8);
        let mut participation = Participation::for_wire_test_many(
            vec![(BAD_RELAY, false, rx)],
            store,
            Keys::generate().public_key(),
            0,
        )
        .await;
        participation.owe_resync(BAD_RELAY);

        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            participation.relay_faults() >= 1,
            "the resync did not fail, so this test says nothing about what a failed one leaves behind"
        );
        assert!(
            participation.resync_pending(BAD_RELAY),
            "still owed — the recovery has not happened"
        );
        assert!(
            participation.last_resync_unix(BAD_RELAY) > 0,
            "the floor did not move on a failed attempt, so this relay is re-asked every tick — a hot \
             loop against a relay that is already failing"
        );
        // ★ The count IS the hot-loop evidence, and it has its own message because it is a different
        // claim: one pump, one attempt. Asserting only `>= 1` above would let a loop hide inside a green.
        assert_eq!(
            participation.relay_faults(),
            1,
            "one pump made more than one resync attempt at the same relay — the floor is not holding it \
             down, which is the hot loop rather than a retry"
        );
    }

    /// ★ FINDING 2. A failed retry send must not abandon the channels queued behind it, and the failing
    /// channel must not be re-attempted every tick either.
    #[tokio::test]
    async fn one_channels_failed_retry_does_not_block_the_rest() {
        let store = test_store("retry-isolation");
        let me = Keys::generate().public_key();
        let long_ago = now_unix() - 3_600;
        joined_and_suppressed(&store, BAD_RELAY, "chan-bad", long_ago);
        joined_and_suppressed(&store, GOOD_RELAY, "chan-good", long_ago);

        let (_bad_tx, bad_rx) = broadcast::channel(8);
        let (_good_tx, good_rx) = broadcast::channel(8);
        let mut participation = Participation::for_wire_test_many(
            vec![(BAD_RELAY, false, bad_rx), (GOOD_RELAY, true, good_rx)],
            store.clone(),
            me,
            now_unix(),
        )
        .await;

        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("a channel fault must not surface as a pump error");

        assert_eq!(
            participation.relay_faults(),
            1,
            "the bad relay's retry did not fail, so nothing was isolated here"
        );
        assert!(
            store
                .suppressed_channels_due(GOOD_RELAY, now_unix())
                .expect("due")
                .is_empty(),
            "the healthy channel's retry never went out — it was queued behind a failing relay that is \
             reached first and stays due forever"
        );
        assert!(
            store
                .suppressed_channels_due(BAD_RELAY, now_unix())
                .expect("due")
                .is_empty(),
            "the failed channel is still due right now, so it is re-attempted on every tick — the wait \
             must restart even though the failure was ours and earns no escalation"
        );
        assert_eq!(
            store
                .suppressed_channels_due(BAD_RELAY, now_unix() + 60)
                .expect("later"),
            vec![("chan-bad".to_string(), 1)],
            "the failed channel must come back due after ONE backoff, still at attempt 1 — our own dead \
             socket is not the relay refusing us"
        );
    }

    /// ★ FINDING 3. A `CLOSED` hint is a number a PEER chose. Sleeping on it inline, uncapped, inside the
    /// batch loop let any single relay stop all participation for as long as it liked.
    #[tokio::test]
    async fn a_relay_supplied_retry_hint_is_capped_and_never_slept_on() {
        let store = test_store("resend-hint");
        let me = Keys::generate().public_key();
        joined_and_suppressed(&store, GOOD_RELAY, "chan-1", now_unix());

        let (good_tx, good_rx) = broadcast::channel(8);
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Closed {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(channel_sub(
                        "chan-1",
                    ))),
                    message: std::borrow::Cow::Borrowed("rate-limited: retry in 86400 seconds"),
                },
            })
            .expect("send closed");

        let mut participation = Participation::for_wire_test_many(
            vec![(GOOD_RELAY, true, good_rx)],
            store,
            me,
            now_unix(),
        )
        .await;

        let before = now_unix();
        // A real clock, deliberately: pausing time would make the inline sleep instant and hide the defect.
        let finished = tokio::time::timeout(
            Duration::from_secs(3),
            participation.pump(Duration::from_millis(1)),
        )
        .await;

        assert!(
            finished.is_ok(),
            "the pump blocked on a relay-supplied delay — one relay can stop every relay's participation \
             for as long as it asks"
        );
        finished.expect("not timed out").expect("pump");
        let deadline = participation
            .resend_deadline(GOOD_RELAY, Some("chan-1"))
            .expect("the re-send was neither sent nor recorded, so the REQ is simply lost");
        assert!(
            deadline <= before + MAX_RESEND_DELAY_SECS as i64 + 1,
            "a peer-supplied hint of 86400s was honoured as given; the ceiling has to be ours to choose"
        );
        assert!(
            deadline > before,
            "the re-send is due immediately, which makes it a hot loop rather than a backoff"
        );
    }

    /// ★ ROUND 10 / FINDING 1. A failed subscribe inside the batch loop must not empty the batch — and it
    /// must leave a DEBT, because the store already says `joined` while nothing is listening.
    #[tokio::test]
    async fn a_failed_subscribe_owes_a_resend_and_does_not_empty_the_batch() {
        let store = test_store("subscribe-isolation");
        let relay_keys = Keys::generate();
        let me = Keys::generate().public_key();

        // An invite on the BAD relay: triage → ChannelJoined → Action::SubscribeChannel, which fails.
        let invite = TestEventBuilder::new(Kind::Custom(44100), "")
            .tag(nostr_sdk::prelude::Tag::public_key(me))
            .tag(nostr_sdk::prelude::Tag::parse(["h", "chan-x"]).expect("h tag"))
            .sign_with_keys(&relay_keys)
            .expect("sign invite");
        let (bad_tx, bad_rx) = broadcast::channel(8);
        bad_tx
            .send(RelayPoolNotification::Message {
                relay_url: BAD_RELAY.parse().expect("url"),
                message: RelayMessage::Event {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(MEMBERSHIP_SUB)),
                    event: std::borrow::Cow::Owned(invite),
                },
            })
            .expect("send invite");

        // Ambient traffic on the HEALTHY relay, queued behind it.
        let (good_tx, good_rx) = broadcast::channel(8);
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Event {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(channel_sub(
                        "chan-good",
                    ))),
                    event: std::borrow::Cow::Owned(
                        TestEventBuilder::new(Kind::Custom(9), "hello")
                            .sign_with_keys(&Keys::generate())
                            .expect("sign"),
                    ),
                },
            })
            .expect("send ambient");

        let mut participation = Participation::for_wire_test_many(
            vec![(BAD_RELAY, false, bad_rx), (GOOD_RELAY, true, good_rx)],
            store,
            me,
            now_unix(),
        )
        .await;

        let ingested = participation
            .pump(Duration::from_millis(1))
            .await
            .expect("a failed subscribe must not surface as a pump error");

        assert_eq!(
            participation.relay_faults(),
            1,
            "the bad relay's subscribe did not fail, so this test proves nothing about isolating it"
        );
        assert_eq!(
            ingested, 2,
            "a message was dropped — the bad relay's failed subscribe aborted the batch, taking the \
             healthy relay's traffic with it"
        );
        let before = now_unix();
        let deadline = participation
            .resend_deadline(BAD_RELAY, Some("chan-x"))
            .expect(
                "the channel is joined in the store with nothing listening and no debt recorded — only a \
                 restart would ever repair it",
            );
        assert!(
            deadline > before - 1 && deadline <= before + MIN_RESYNC_INTERVAL_SECS + 1,
            "the owed re-send is not on a bounded near-term deadline"
        );
    }

    /// ★ ROUND 10 / FINDING 2. An auth-failure `CLOSED` used to block the batch loop for 10s plus a probe
    /// timeout. The settle is a deadline now; nothing waits.
    #[tokio::test]
    async fn an_auth_failure_defers_the_reprove_instead_of_blocking_the_pump() {
        let store = test_store("reconnect-deadline");
        let me = Keys::generate().public_key();
        let (good_tx, good_rx) = broadcast::channel(8);
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Closed {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(MEMBERSHIP_SUB)),
                    message: std::borrow::Cow::Borrowed("auth-required: we need your NIP-42 auth"),
                },
            })
            .expect("send closed");

        let mut participation = Participation::for_wire_test_many(
            vec![(GOOD_RELAY, true, good_rx)],
            store,
            me,
            now_unix(),
        )
        .await;

        let before = now_unix();
        // Real clock: the old code's 10s wait plus probe timeout has to be able to show up as a stall.
        let finished = tokio::time::timeout(
            Duration::from_secs(4),
            participation.pump(Duration::from_millis(1)),
        )
        .await;

        assert!(
            finished.is_ok(),
            "the pump blocked on a reconnect settle — every other relay's traffic waits behind it, and \
             several relays failing at once serialise the stalls"
        );
        finished.expect("not timed out").expect("pump");
        let deadline = participation
            .reprove_deadline(GOOD_RELAY)
            .expect("no re-prove was recorded, so the socket was kicked and access never re-checked");
        assert!(
            deadline > before,
            "the re-prove is due immediately, which probes a socket that has had no time to settle"
        );
        assert!(
            deadline <= before + RECONNECT_SETTLE_SECS + 1,
            "the settle window grew beyond the bound it replaced"
        );
    }

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
