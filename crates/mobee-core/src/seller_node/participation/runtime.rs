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
    Client, Event, PublicKey, RelayMessage, RelayOptions, RelayPoolNotification, RelayStatus,
    SubscribeAutoCloseOptions, SubscriptionId,
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
    /// re-proven. `None` means nothing is owed — and, per [`Live::quarantined`], that it is the only
    /// state in which anything may be asked of this relay.
    reprove_due: Option<i64>,
}

impl Live {
    /// Whether all outbound wire recovery for this relay is suspended pending proof of access.
    ///
    /// ★ `reprove_due` HAS TO BE A QUARANTINE AND NOT MERELY A DEADLINE. It is set when the relay told
    /// us our authentication or scope failed, which is the relay saying the admission we hold may no
    /// longer be valid. As a bare deadline it left the relay fully live in the meantime, so the resync,
    /// re-send and retry lanes went on sending REQs — on the very socket whose access is in question,
    /// and often in the SAME tick that kicked it, since `retry_suppressed_channels` runs after the
    /// batch that set this. If the probe then fails the relay is dropped, so every one of those REQs
    /// was addressed to a relay we had already been told we may not read from.
    ///
    /// ★★ Independent recovery lanes that share a socket have to share the socket's gate. Each lane
    /// was individually right about its own precondition and none of them owned this one, which is how
    /// three correct lanes composed into traffic nobody had authorised.
    ///
    /// Debts survive the quarantine untouched — they are owed, not cancelled — and the rate floors are
    /// not advanced either, because a quarantine means no attempt was made. Whichever way
    /// [`Participation::drain_reproves`] resolves, it resolves: proof clears this, and a failed probe
    /// removes the relay entirely, so nothing can be held here forever.
    fn quarantined(&self) -> bool {
        self.reprove_due.is_some()
    }
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
    /// Kept so a relay admitted AFTER boot can be given an [`Engine`]. Before the quarantine retry
    /// existed, every engine was built inside `start` where the store was still a parameter — a relay
    /// could only ever join the live set at boot, so nothing needed the store later.
    store: SellerStore,
}

impl Participation {
    /// Establish access on every configured relay, then subscribe the wake surface on the ones that
    /// proved admission.
    ///
    /// `publisher` publishes the probe carrier; `carrier` is the event to use (the node's persona).
    /// ★ BOTH ARE RETAINED, and the comment here used to say they were not — true when the only probe
    /// happened at start-up, falsified in place once the tick began re-probing due quarantines. They are
    /// held for THAT and nothing else: the sole publish this module performs is the access probe's own
    /// persona write. There is still no path by which participation publishes anything else.
    ///
    /// A relay that REFUSES us is recorded denied and then left entirely alone. One that merely fails
    /// to answer is quarantined and tried again on a backoff — see [`AccessState::Quarantined`].
    /// Start-up never fails because of a relay — a social surface must not be able to stop the node
    /// that earns money.
    pub async fn start(
        config: &ParticipationConfig,
        store: SellerStore,
        me: PublicKey,
        publisher: &Client,
        carrier: &Event,
    ) -> Result<Self, ParticipationError> {
        let mut participation = Self {
            live: BTreeMap::new(),
            roster: RelayRoster::new(config.relays.clone()),
            me,
            lagged: 0,
            forced_progress: 0,
            relay_faults: 0,
            publisher: publisher.clone(),
            carrier: carrier.clone(),
            probe_timeout: Duration::from_secs(config.probe_timeout_secs.max(1)),
            store,
        };

        // Boot drains every relay that is owed a probe; the runtime tick takes at most one per pass.
        // ★ BOTH GO THROUGH THE SAME BODY on purpose — two copies of "how access is established" would
        // drift, and the copy that drifted would be the one that only runs in production.
        while participation.probe_one_due(now_unix()).await? {}

        Ok(participation)
    }

    /// Establish access on ONE relay that the roster says is owed a probe and that holds no live
    /// socket. `Ok(false)` ⇒ nothing was due, which is how a caller draining to exhaustion stops.
    ///
    /// ★★ THIS IS WHAT MAKES A QUARANTINE A RETRY RATHER THAN A LABEL. [`RelayRoster::relays_to_probe`]
    /// used to be read only at boot, so a relay quarantined at runtime waited on a caller that never
    /// came — the backoff was inert and a briefly-slow relay was lost until process restart.
    ///
    /// ★ ONE PER CALL, for the same reason [`Self::drain_reproves`] is capped: a probe WAITS, up to
    /// `probe_timeout`, so draining every due relay in one pass would spend the pump's whole budget on
    /// the relays that are by definition not the ones carrying work.
    ///
    /// The errors are the STORE's and the subscription's, never the relay's. Boot propagates them — a
    /// broken store must stop the node — while the tick logs and moves on.
    async fn probe_one_due(&mut self, now: i64) -> Result<bool, ParticipationError> {
        let due: Vec<String> = self
            .roster
            .relays_to_probe(now)
            .map(|entry| entry.url.clone())
            .collect();
        let Some(url) = due.into_iter().find(|url| !self.live.contains_key(url)) else {
            return Ok(false);
        };

        let reader = match connect_reader(&url, &self.publisher).await {
            Ok(reader) => reader,
            Err(error) => {
                // The socket never opened, so the relay never said anything: SILENCE, not refusal.
                // Recording this as a refusal is what turned one unreachable moment into a permanent
                // loss. See [`ProbeOutcome::Unreachable`].
                self.roster
                    .record_probe(&url, ProbeOutcome::Unreachable(error), now);
                return Ok(true);
            }
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
            // Close the socket we opened to probe. An unproven relay gets nothing further — not a REQ,
            // not a heartbeat. A quarantined one comes back through THIS path once its backoff
            // elapses, on a socket opened fresh.
            reader.disconnect().await;
            return Ok(true);
        }

        let engine = Engine::new(self.store.clone(), url.clone(), self.me);
        // No REQ is outstanding on a socket we do not hold, so any retry marked in flight for this
        // relay is a phantom: truthful about the past, meaningless now. Cleared rather than expired —
        // nothing was refused, we simply never got to ask.
        //
        // ★ That invariant is about the SOCKET, not about boot. It was written when this ran only at
        // start-up ("nothing we sent before this process began is still outstanding") and it holds
        // just as well for a relay re-admitted mid-process, whose old socket is equally gone.
        // Subscribe to the notification stream BEFORE the first REQ goes out, or the events that REQ
        // produces are broadcast to nobody.
        let notifications = reader.notifications();
        let installed = match engine.forget_retries_in_flight(now) {
            Err(error) => Err(ParticipationError::from(error)),
            Ok(_) => subscribe_wake_surface(&reader, &engine, self.me)
                .await
                .map(|_| ()),
        };

        // ★★ A BROKEN STORE IS NOT THIS RELAY'S FAULT AND MUST NOT BE CHARGED TO ITS BUDGET. The two
        // failures above are not the same kind of fact: a failed subscribe is the wire, and this
        // relay's admission is genuinely back in doubt — but `forget_retries_in_flight` failing is
        // OUR sqlite, and it would fail identically against every relay in the config. Folded into
        // the silence budget it denies three PROVEN relays in three ticks and blames them for it,
        // which is the round-13 defect wearing a local error's clothes. So it propagates unrecorded:
        // boot stops the node, the tick logs it every pass, and the relay keeps the state it had.
        if let Err(error) = installed {
            if let ParticipationError::Store(_) = error {
                reader.disconnect().await;
                return Err(error);
            }
            // ★★ ADMISSION WAS PROVEN AND WE STILL HOLD NOTHING — and recording the admission before
            // finding that out is what stranded the relay. `Admitted` is excluded by
            // [`RelayRoster::relays_to_probe`], and every recovery lane iterates `self.live`, which
            // has no entry for a relay whose install failed. So the one state reachable by NO lane at
            // all was the state a relay landed in after its probe SUCCEEDED — strictly worse than
            // failing it, and the tick path reached it by logging this error and carrying on.
            //
            // The verdict is a silence because that is what the node is experiencing: it is not being
            // served by this relay and nobody refused it. That buys the backoff, the attempt ceiling,
            // and an end — the machinery already built for exactly this shape.
            //
            // ⚠ NO TEST IN THIS SUITE REDDENS FOR THIS BRANCH, and it is stated rather than implied.
            // Reaching it needs a relay that serves the carrier back AND then fails the subscribe:
            // `LocalRelay` can do the first and `PGateRelay` neither, so the failure would have to be
            // raced rather than arranged. The ORDERING above is what the fix actually is — the record
            // follows the insert — and that much is plain in the control flow. Treat this handler as
            // insurance with a reason, not as a proven repair, and do not let its presence read as
            // coverage. Same standing as [`clear_registrations`]'s per-id loop, one lane over. The
            // `Store` split just above and the pump's propagation of it are untested for the same
            // reason — reaching either needs this branch.
            self.roster.record_probe(
                &url,
                ProbeOutcome::Unreachable(format!(
                    "admitted, but the wake surface could not be installed: {error}"
                )),
                now,
            );
            reader.disconnect().await;
            // ★★ `Ok`, NOT `Err`, AND THIS FUNCTION'S OWN DOC SAYS WHY: "start-up never fails because of
            // a relay — a social surface must not be able to stop the node that earns money."
            // Returning `Err` here handed exactly that power to one peer: `start` drains this with `?`, so
            // a single relay that echoed the carrier and then dropped the subscribe aborted participation
            // start-up for EVERY relay. The silence is recorded, so the backoff and the ceiling still
            // apply to the relay that failed — and only to it. Same rule the resync lane already spells
            // out one screen down: an error path that halts the loop turns one dead peer into an outage.
            return Ok(true);
        }

        self.live.insert(
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
        // ★ AFTER the insert, not before it. The admission is recorded once the thing it entitles us
        // to — a live, subscribed socket — actually exists. Nothing between here and the insert can
        // fail, so the ordering costs one `String` clone and removes the window entirely rather than
        // arguing about how narrow it is.
        self.roster
            .record_probe(&url, ProbeOutcome::EchoObserved, now);
        Ok(true)
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
            // ★ FIRST, because a socket nobody will revive is a precondition of every lane below, and
            // learning about it after they have each spent an attempt on it is learning about it too late.
            // With the SDK's own supervision off, this is the only thing that reconnects anything.
            self.revive_dead_sockets().await;

            let due_resyncs: Vec<String> = self
                .live
                .iter()
                .filter(|(_, live)| {
                    live.resync_pending
                        && !live.quarantined()
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
                // ★★ The FLOOR records that an ATTEMPT happened, so it advances either way. Leaving it
                // untouched on failure — which is what "on error both stay put" did — re-asks a dead
                // relay at pump frequency, the hot loop the floor exists to prevent. The DEBTS record
                // whether the recovery HAPPENED, so only success discharges them. Rate and debt are
                // separate questions about the same event, and a failed attempt answers only the first.
                match self.resync_relay(url).await {
                    // ★ And success discharges EVERY debt this install paid, not just the one that asked
                    // for it — see [`Self::settle_wake_surface`]. It advances the floor too.
                    Ok(installed) => self.settle_wake_surface(url, &installed, now),
                    Err(error) => {
                        // ★★★ OUR STORE IS NOT THIS RELAY'S FAULT, and this helper fails as either.
                        // `resync_relay` reads three cursors before it sends anything, so an `Err` here is
                        // a wire failure OR our sqlite — and one handling is wrong for both: the relay is
                        // charged a fault that belongs to no relay, and the only condition that will not
                        // heal on its own is swallowed by the `_`. The same split `drain_reproves` makes
                        // below, on the same error out of the same helper.
                        if let ParticipationError::Store(_) = error {
                            return Err(error);
                        }
                        self.relay_faults = self.relay_faults.saturating_add(1);
                        if let Some(live) = self.live.get_mut(url) {
                            live.last_resync_unix = now;
                        }
                    }
                }
            }

            // Re-send subscriptions a `CLOSED` deferred, now that their capped deadline has passed. Grouped
            // with the resync because both are owed WIRE RECOVERY and neither arms an attribution token —
            // which is why they may run before the batch while `retry_suppressed_channels` may not.
            //
            // ★ ALL FOUR RECOVERY LANES — resync above, these two, and the retry after the batch — ARE
            // GATED ON [`Live::quarantined`]. They fire on independent deadlines and share one socket, so
            // being individually right about their own preconditions was not enough: whether the relay
            // will still talk to us is a precondition of all of them, and it belongs to none of them.
            // `drain_reproves` is the exception because it IS that gate.
            self.drain_resends(now).await?;
            self.drain_reproves(now).await?;

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

            // A relay whose quarantine backoff has elapsed and that holds NO live socket. Deliberately
            // outside the four-lane gate above: those lanes recover an existing socket and so depend on
            // "will this relay still talk to us", whereas this one has no socket to recover and exists
            // precisely to ask that question again.
            //
            // ★ AFTER THE BATCH, AND THAT IS THE POINT OF THE PLACEMENT. This call BLOCKS — a connect
            // waits, then the probe waits — and `drain_reproves` above can block on a probe of its own
            // in the same pass. Sitting ahead of the batch, the two of them held messages ALREADY IN
            // HAND behind two round trips to relays that are by definition not the ones carrying work.
            // Traffic we have already read is now judged first, and the relay we are not yet serving
            // waits for the tail of the tick instead of the head of it.
            //
            // ⛔ NEITHER THE PLACEMENT NOR THE ONE-PER-TICK CAP IS PROTECTED AS A SET, and the surviving
            // mutations are named because "untested" is too vague to act on: moving this call back ABOVE
            // the batch loop, or changing it to drain every due probe instead of one, would both leave the
            // whole suite green. The guard test that exists uses ONE relay with no buffered traffic, so it
            // cannot see either. What it would take is two fixture relays, buffered live traffic, and
            // several due quarantines, asserting the buffered traffic is ingested first and exactly one
            // probe goes out. Not built; do not read the comment above as coverage.
            //
            // Best-effort FOR A RELAY'S OWN FAILURE, and only that. A relay we are not yet serving must
            // not be able to empty a batch or wedge the pump, so a subscribe that failed on the wire is
            // logged; it was recorded as a silence, and the backoff brings it round again while the
            // attempt ceiling stops that being forever.
            //
            // ★★ A `Store` ERROR IS NOT THAT, AND SWALLOWING IT SPINS. It is recorded against no relay —
            // deliberately, because our sqlite failing is not any peer's fault — which means nothing
            // moves: the relay stays due, and the next tick spends another connect and another probe on
            // a HEALTHY relay to fail in the same place, every pass, forever. Fixing the misattribution
            // without fixing the loop just traded a wrong verdict for a hot one.
            //
            // So it propagates, which is what every other `Store` error in this pump already does for
            // the reason given below: OUR persistence failing is not a thing to carry on through.
            if let Err(error) = self.probe_one_due(now).await {
                if let ParticipationError::Store(_) = error {
                    return Err(error);
                }
                eprintln!(
                    "participation: re-probe of a due relay failed ({error}); it stays unproven and \
                     comes back once its backoff elapses"
                );
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
                engine: Engine::new(store.clone(), url.to_string(), me),
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
            store,
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
            store,
        }
    }

    /// One relay reached through [`connect_reader`], against a URL that really accepts sockets.
    ///
    /// ★ THROUGH `connect_reader` ON PURPOSE. The other seams build their own `Client`, which means the
    /// relay OPTIONS — the thing that decides whether the SDK supervises the socket behind our back — are
    /// invisible to them. A test that constructs its own client cannot see a change to how production
    /// constructs one, so it would report every setting as green.
    ///
    /// ★★ `keys` MUST be the identity the wake surface is addressed to. The reader authenticates with the
    /// publisher's signer, and a `#p`-gating relay refuses a filter whose `#p` is not the authenticated
    /// pubkey — with `restricted:`, which this module turns into a reconnect. Signing as anyone else makes
    /// every test on this seam pass or fail for that reason instead of its own; it cost one round-13
    /// assertion that read as green while measuring a refusal.
    ///
    /// ⚠ WHAT THIS SEAM CANNOT REACH: `ProbeOutcome::EchoObserved`. [`PGateRelay`] acknowledges an
    /// `EVENT` without storing it and answers every `REQ` with a bare `EOSE`, so the read-back finds
    /// nothing and a probe through here resolves `EchoMissing` however healthy the transport is. The
    /// publisher and carrier below are what make the probe REACH the relay; they cannot make it come
    /// back. A test on this seam can therefore assert that a probe was ATTEMPTED — the fixture counts
    /// the socket — and must not assert re-admission.
    ///
    /// ★ THE LIMIT IS THIS FIXTURE'S, NOT THE SUITE'S, and the distinction matters to anyone deciding
    /// where to put the next test. `nostr_relay_builder`'s `LocalRelay` DOES store and serve, which is
    /// how [`super::probe`]'s own tests reach `EchoObserved` — what it cannot do is answer a `#p`
    /// filter with `restricted:`, which is the whole reason this seam uses the other one. Neither
    /// fixture is a superset of the other, so a test wanting both admission AND the gate has no home
    /// in process yet.
    #[cfg(test)]
    async fn for_live_test(url: &str, store: SellerStore, keys: nostr_sdk::prelude::Keys) -> Self {
        let me = keys.public_key();
        // ★★ THE PUBLISHER IS CONNECTED AND AUTHENTICATED, LIKE PRODUCTION'S. `start` receives a
        // publisher its caller already connected; a seam that hands over a bare `Client::new` has a
        // publisher that reaches nothing, so EVERY probe through it resolves `EchoMissing` — not
        // because admission failed but because the publish never left the process. That made the
        // echo leg of revive → probe → echo → install unreachable from this seam, and any test that
        // advanced past `reprove_due` measured the seam's own wiring instead of the code.
        let publisher = Client::new(keys.clone());
        publisher.automatic_authentication(true);
        publisher
            .add_relay(url)
            .await
            .expect("add the fixture relay to the publisher");
        publisher.connect().await;
        publisher
            .wait_for_connection(Duration::from_secs(5))
            .await;
        let reader = connect_reader(url, &publisher)
            .await
            .expect("connect reader to the fixture relay");
        let notifications = reader.notifications();
        let mut live = BTreeMap::new();
        live.insert(
            url.to_string(),
            Live {
                reader,
                notifications,
                engine: Engine::new(store.clone(), url.to_string(), me),
                progress_since_resync: 1,
                last_resync_unix: now_unix(),
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
            publisher,
            // ★★ SIGNED WITH `keys` — OUR identity, not a stranger's. The probe publishes the carrier
            // and reads it back BY ID to prove admission; a carrier signed by a generated throwaway
            // is a foreign event, which a gating relay may refuse and which an author-scoped read
            // never matches. Either way the echo cannot arrive, so this seam could only ever
            // demonstrate failure. Same lesson as the `keys` note above, one field over — that note
            // had learned it for the READER and not yet for the CARRIER.
            carrier: nostr_sdk::prelude::EventBuilder::new(
                nostr_sdk::prelude::Kind::Metadata,
                r#"{"name":"mobee-acceptance"}"#,
            )
            .sign_with_keys(&keys)
            .expect("sign carrier"),
            probe_timeout: Duration::from_secs(1),
            store,
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

    /// Mark a re-send owed, so a test can be about the DRAIN rather than about how the debt was recorded.
    #[cfg(test)]
    fn owe_resend(&mut self, url: &str, channel_id: Option<&str>, deadline: i64) {
        if let Some(live) = self.live.get_mut(url) {
            live.resend_due
                .insert(channel_id.map(str::to_string), deadline);
        }
    }

    /// Install the wake surface, so a test starts from the state a live relay is actually in — with
    /// subscriptions REGISTERED in the SDK, which is the precondition the reconnect has to destroy.
    #[cfg(test)]
    async fn install_wake_surface(&mut self, url: &str) {
        let Some(entry) = self.live.get(url) else {
            return;
        };
        subscribe_wake_surface(&entry.reader, &entry.engine, self.me)
            .await
            .expect("install wake surface");
    }

    /// Kill the socket while leaving the SDK's subscription registry intact — the state a relay that
    /// refused our auth and then dropped the connection leaves behind, and the one in which the SDK's
    /// bulk `unsubscribe_all()` stops early.
    #[cfg(test)]
    async fn drop_socket(&self, url: &str) {
        if let Some(entry) = self.live.get(url) {
            entry.reader.disconnect().await;
        }
    }

    /// How many subscriptions the SDK still holds registered for this relay.
    ///
    /// ★ This is the SDK's own state, not ours — and it is the SAME map `resubscribe()` reads:
    /// `RelayPool::subscriptions` is built by asking each `Relay` for its registry, not from a separate
    /// pool-side cache. Verified in `nostr-relay-pool-0.44.1` `pool/mod.rs`.
    #[cfg(test)]
    async fn registered_subscriptions(&self, url: &str) -> usize {
        match self.live.get(url) {
            Some(entry) => entry.reader.subscriptions().await.len(),
            None => 0,
        }
    }

    /// Put a relay into quarantine at a chosen deadline — the state an auth failure leaves behind.
    #[cfg(test)]
    fn owe_reprove(&mut self, url: &str, at: i64) {
        if let Some(live) = self.live.get_mut(url) {
            live.reprove_due = Some(at);
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
    ///
    /// ★ THE ONE LANE THAT CARRIES A SNAPSHOT, so it is the one lane that has to re-validate. Every other
    /// recovery here re-derives its work from the store at the moment it fires — the resync asks
    /// `channels_to_resume`, the retry asks `channels_to_retry` — so a membership change between deciding
    /// and doing simply changes the answer. This lane records a channel id and comes back to it later, and
    /// a recorded intent cannot notice that its reason expired: a `44101` or a refusal in the interval
    /// leaves the channel un-resumable while the debt still says "ask for this". Record-time truth is not
    /// fire-time truth. So the gate is checked HERE as well as at the set-sites, and the debt is dropped
    /// rather than sent.
    async fn drain_resends(&mut self, now: i64) -> Result<(), ParticipationError> {
        let due: Vec<(String, Option<String>)> = self
            .live
            .iter()
            // Nothing goes out on a relay whose access is unproven — see [`Live::quarantined`]. The
            // deadlines stay exactly as they are: the debt is owed, and a quarantine is not payment.
            .filter(|(_, live)| !live.quarantined())
            .flat_map(|(url, live)| {
                live.resend_due
                    .iter()
                    .filter(|(_, deadline)| now >= **deadline)
                    .map(move |(target, _)| (url.clone(), target.clone()))
            })
            .collect();

        for (url, target) in due {
            // A channel we have since left, or that has since been refused, is not ours to ask for — and
            // re-asking a suppressed one would send the REQ from the wrong lane as well as the wrong
            // state: unmarked, so the `EOSE` answering it could not clear the suppression it was meant to.
            // Cancel the debt; this is an outcome, not a fault.
            let resumable = match self.live.get(&url) {
                Some(entry) => match &target {
                    Some(channel_id) => entry.engine.is_resumable(channel_id),
                    None => Ok(true),
                },
                None => continue,
            };
            match resumable {
                Ok(true) => {}
                Ok(false) => {
                    if let Some(live) = self.live.get_mut(&url) {
                        live.resend_due.remove(&target);
                    }
                    continue;
                }
                // ★★★ THE TYPE PROVES WHOSE FAULT THIS IS: `is_resumable` returns `StoreError`, so there
                // is no wire failure this arm could be describing. It used to charge the relay a fault and
                // push the debt out a whole interval — wrong on both counts, because the relay did nothing
                // and the store failure was never reported. It leaves by the one door that names it.
                Err(error) => return Err(ParticipationError::Store(error)),
            }
            let Some(entry) = self.live.get(&url) else {
                continue;
            };
            // ★★★ THE CURSOR READ IS LIFTED OUT OF `outcome`, so that what remains can only be a wire
            // result — `subscribe_channel` and `subscribe_membership` return nothing but
            // `ParticipationError::Relay`. Folding the store failure into the same `Result` as the send
            // made `outcome.is_ok()` a question not worth asking: one bucket holding both "the relay
            // refused" and "our sqlite is down", handled as the first. Reading the cursor with `?` DELETES
            // that ambiguity instead of testing for it — the best fix removes the surface. The comment
            // that stood here said "a store read failing is not a relay fault" and the next line counted
            // one anyway, which is how it survived four reviews: a comment cannot recompile.
            let outcome = match &target {
                Some(channel_id) => {
                    let since = entry.engine.channel_cursor(channel_id)?;
                    subscribe_channel(&entry.reader, channel_id, self.me, since).await
                }
                None => {
                    let since = entry.engine.membership_cursor()?;
                    subscribe_membership(&entry.reader, self.me, since).await
                }
            };
            let Some(live) = self.live.get_mut(&url) else {
                continue;
            };
            // `outcome` can only be a wire result by the construction above, so this fault is honestly the
            // relay's and the deferral is charged to the party that earned it.
            if outcome.is_ok() {
                live.resend_due.remove(&target);
            } else {
                self.relay_faults = self.relay_faults.saturating_add(1);
                live.resend_due
                    .insert(target, now.saturating_add(MIN_RESYNC_INTERVAL_SECS));
            }
        }
        Ok(())
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
    async fn drain_reproves(&mut self, now: i64) -> Result<(), ParticipationError> {
        let Some(url) = self
            .live
            .iter()
            .find(|(_, live)| live.reprove_due.is_some_and(|at| now >= at))
            .map(|(url, _)| url.clone())
        else {
            return Ok(());
        };
        // `Client` is `Arc`-backed, so cloning the handle costs a refcount and frees the borrow on
        // `self.live` — which matters because the denial path REMOVES the entry.
        let Some(reader) = self.live.get(&url).map(|live| live.reader.clone()) else {
            return Ok(());
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
            return Ok(());
        }

        // ⛔ THE QUARANTINE IS **NOT** LIFTED HERE, AND IT USED TO BE. Clearing `reprove_due` before the
        // rebuild was the arm-before-the-event mistake wearing its opposite face: `Live::quarantined` reads
        // this field, and it is the gate on all four recovery lanes — so lifting it on the strength of the
        // ECHO alone declared the relay fit to be sent to before anything had been re-subscribed on it.
        // The echo proves the transport; only the rebuild proves the access. It is cleared in the success
        // arm, with the rest of what success earns.
        let rebuilt = {
            let Some(entry) = self.live.get(&url) else {
                return Ok(());
            };
            subscribe_wake_surface(&entry.reader, &entry.engine, self.me).await
        };
        match rebuilt {
            // ★ The rebuild that ends a quarantine pays whatever the quarantine held: the resync that could
            // not run, the re-sends that could not go out. Same settlement as the resync drain, because it
            // is the same install — see [`Self::settle_wake_surface`].
            Ok(installed) => {
                // ★★★ AND ONLY HERE IS THE BUDGET CLEARED, because only here has this relay proved it will
                // SERVE us. Recording the success straight after the echo — which is where it was — reset
                // the attempt tally on the strength of a kind-0 read, and [`super::probe`]'s own doc warns
                // that a relay may serve public metadata while refusing every `#p`-gated read. So a relay
                // that echoed the carrier and then `CLOSED auth-required:` the membership filter on every
                // cycle had its budget cleared each time and could NEVER reach `Denied`: an unbounded
                // ping-pong, built by a fix whose whole purpose was to make the retry terminate.
                //
                // ⇒ THE BUDGET MUST BE CLEARED BY THE PROOF THAT MATTERS, NOT THE ONE THAT ARRIVES FIRST.
                // The echo proves the transport; the wake surface proves the access. `probe_one_due` got
                // this right by construction — it records after the install — and this path did not.
                self.roster
                    .record_probe(&url, ProbeOutcome::EchoObserved, now);
                if let Some(live) = self.live.get_mut(&url) {
                    live.reprove_due = None;
                }
                self.settle_wake_surface(&url, &installed, now)
            }
            // ★★★ THE FAILURE IS FOLDED INTO THE ROSTER, and leaving it out was the SAME FIX HALF-DONE for
            // the third time on this branch. Moving the budget reset into the success arm stopped the
            // roster being wrongly cleared; it did nothing about the failure, which recorded NOTHING. So the
            // relay stayed live, stayed `Admitted`, had its quarantine lifted by the echo, and was retried
            // by the RESYNC lane at the floor rate — forever, because nothing on that path can ever reach
            // the attempt ceiling. The ping-pong did not die; it changed lanes.
            //
            // ⇒ SHAPE, and it has cost three rounds: WHEN A SUCCESS PATH AND A FAILURE PATH SHARE A
            // PREAMBLE, FIXING THE SUCCESS PATH DOES NOT FIX THE PREAMBLE — and the failure path is where
            // the unbounded behaviour lives, because that is the one that repeats.
            //
            // Now it is shaped exactly like the probe-failure branch at the top of this function: record the
            // silence, drop the socket, and let `relays_to_probe` own the backoff, the ceiling and the
            // eventual denial. One body decides how a relay that will not serve us is treated.
            //
            // ⛔ UNTESTED, AND IT IS THE THIRD BRANCH IN THIS FAMILY BLOCKED BY ONE MISSING FIXTURE
            // CAPABILITY — worth stating as a set rather than three times as an apology. All three need a
            // relay that ECHOES the carrier and THEN fails a subscribe: this one, `probe_one_due`'s install
            // failure, and the `Store` split inside it. `PGateRelay` gates `#p` and answers `restricted:`
            // but cannot serve a stored event back, so it can never get past the echo; `LocalRelay` serves
            // events but cannot gate, so it can never fail the subscribe. Note a CLOSED will NOT do it —
            // `subscribe_with_id` returns once the REQ is sent, so a refusal arrives too late to become an
            // `Err`; the socket has to be GONE by subscribe time. ⇒ the one fixture that unblocks all three
            // is `PGateRelay` plus stored-event service plus a close-after-serving-the-echo control. Until
            // then this is insurance with a reason, and its presence is not coverage.
            Err(error) => {
                // ★★★ AND THE STORE IS SPLIT OUT HERE TOO — THE FOURTH SITE, AND I HAD ALREADY WRITTEN
                // THIS EXACT SPLIT IN `probe_one_due` ONE FUNCTION AWAY. `subscribe_wake_surface` reads
                // cursors, so it returns `Store` as readily as `Relay`, and folding both into the relay's
                // silence budget charges OUR sqlite to a healthy peer — the same misattribution the last
                // three rounds each fixed in a different place. A local failure is identical across every
                // relay, which is exactly why it cannot be any relay's fault.
                if let ParticipationError::Store(_) = error {
                    self.relay_faults = self.relay_faults.saturating_add(1);
                    return Err(error);
                }
                self.relay_faults = self.relay_faults.saturating_add(1);
                self.roster.record_probe(
                    &url,
                    ProbeOutcome::Unreachable(format!(
                        "echoed the carrier but the wake surface could not be rebuilt: {error}"
                    )),
                    now,
                );
                if let Some(entry) = self.live.remove(&url) {
                    entry.reader.disconnect().await;
                }
            }
        }
        Ok(())
    }

    /// Re-subscribe every channel whose suppression backoff has elapsed, one attempt each.
    ///
    /// The suppression stays raised until the relay answers with `EOSE`; this only sends the REQ and
    /// records that it is in flight.
    async fn retry_suppressed_channels(&mut self) -> Result<(), ParticipationError> {
        let now = now_unix();
        let mut retries: Vec<(String, String, Option<u64>)> = Vec::new();
        for (url, entry) in self.live.iter() {
            // ★ SKIPPED ENTIRELY on a quarantined relay, not merely stopped short of the send — because
            // `channels_to_retry` expires stale retry tokens as a side effect, and expiring one while we
            // are deliberately not asking would charge the relay a backoff step for OUR silence.
            if entry.quarantined() {
                continue;
            }
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
    async fn resync_relay(&mut self, url: &str) -> Result<Vec<String>, ParticipationError> {
        let Some(entry) = self.live.get(url) else {
            return Ok(Vec::new());
        };
        subscribe_wake_surface(&entry.reader, &entry.engine, self.me).await
    }

    /// Take one relay's socket down and back up, and owe a fresh proof of access.
    ///
    /// ★★ ONE OWNER FOR RECONNECTING, TWO TRIGGERS: a `CLOSED` telling us our authentication failed, and
    /// a socket we found dead. They are the same act — the registry has to be emptied before the socket
    /// returns or the SDK replays it, and admission has to be re-proven because a socket that went away is
    /// not a socket we are still admitted on. Two call sites doing that by hand would be two chances to
    /// forget half of it.
    ///
    /// ★ The registrations go first, and that ordering is the whole point: see [`clear_registrations`].
    ///
    /// Nothing here blocks — `disconnect`/`connect` return promptly (`connect` sets the relay `Pending`
    /// synchronously and spawns its own task) and the settle is left as a deadline. That is why this needs
    /// no one-per-tick cap while [`Self::drain_reproves`] does: the probe waits, this does not.
    async fn reconnect_relay(&mut self, url: &str) {
        let Some(entry) = self.live.get(url) else {
            return;
        };
        clear_registrations(&entry.reader).await;
        entry.reader.disconnect().await;
        entry.reader.connect().await;
        if let Some(live) = self.live.get_mut(url) {
            live.reprove_due = Some(now_unix().saturating_add(RECONNECT_SETTLE_SECS));
        }
    }

    /// Bring back any relay whose socket died, because with SDK auto-reconnect off nothing else will.
    ///
    /// ★★★ THE DUTY THAT COMES WITH TAKING THE WHEEL. Disabling the pool's own supervision (see
    /// [`connect_reader`]) is what makes every reconnect ours to gate — and it means a lost socket now ends
    /// `Terminated` with no retry behind it. Left there, participation on that relay simply stops: no error,
    /// no timer, nothing owed. A silent park is exactly what this module treats as the worst outcome, so
    /// removing the SDK's retry without replacing it would have been a worse trade than the leak it fixed.
    ///
    /// ★★ It runs BEFORE the recovery lanes, not after, so the quarantine it raises is honoured on the same
    /// tick — otherwise the resync, re-send and retry lanes each spend an attempt on a socket we already
    /// know is gone.
    ///
    /// ★ `reprove_due` is the rate limit, and no second knob is needed: a revived relay is quarantined
    /// until its probe, and the probe cannot pass on a socket that is still dead — so a relay that will not
    /// come back is removed after one attempt rather than reconnected every tick. The count is what keeps a
    /// flapping relay from looking like a healthy one.
    async fn revive_dead_sockets(&mut self) {
        let candidates: Vec<(String, Client)> = self
            .live
            .iter()
            .filter(|(_, live)| !live.quarantined())
            .map(|(url, live)| (url.clone(), live.reader.clone()))
            .collect();
        let mut dead: Vec<String> = Vec::new();
        for (url, reader) in candidates {
            if reader_socket_is_dead(&reader, &url).await {
                dead.push(url);
            }
        }
        for url in dead {
            self.relay_faults = self.relay_faults.saturating_add(1);
            self.reconnect_relay(&url).await;
        }
    }

    /// Settle every debt one wake-surface rebuild paid.
    ///
    /// ★★★ THE DUAL OF THE FIRE-TIME RULE. Round 11 made sure a debt SURVIVES until its work is done; this
    /// is the other half — a debt whose work has been done must DIE. A rebuild installs the membership
    /// filter and every resumable channel, so it discharges a pending resync AND any re-send owed for the
    /// filters it covers. Clearing only the marker that triggered it left the others recorded, and they
    /// fired again on the next tick: the same REQs a second time, which on a relay that rate-limits is a
    /// self-inflicted refusal.
    ///
    /// ★★ ONE SETTLEMENT, BOTH CALLERS — the resync drain and a successful re-prove. They pay the same
    /// debts by running the same install, so they cannot be allowed to disagree about what that discharges;
    /// the same reason [`Engine::is_resumable`] is shared rather than restated.
    ///
    /// ★ Scoped to what was ACTUALLY installed, never "everything owed". A channel the rebuild skipped —
    /// left, or suppressed — still owes whatever it owed; pruning it here would silently drop a
    /// subscription nothing else re-sends.
    ///
    /// The floor moves too: the work happened now, so "may we ask again yet" counts from now.
    fn settle_wake_surface(&mut self, url: &str, installed: &[String], now: i64) {
        let Some(live) = self.live.get_mut(url) else {
            return;
        };
        live.resync_pending = false;
        live.last_resync_unix = now;
        live.resend_due.remove(&None);
        for channel_id in installed {
            live.resend_due.remove(&Some(channel_id.clone()));
        }
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
                    // Nothing may be asked of a relay whose access is unproven, so the invite is owed
                    // rather than answered — the same debt a failed send records below, and no fault,
                    // because nothing failed. [`Self::drain_resends`] honours the quarantine too, so the
                    // deadline being immediately past is harmless: it fires on the first tick after proof.
                    if entry.quarantined() {
                        if let Some(live) = self.live.get_mut(url) {
                            live.resend_due.insert(Some(channel_id), now_unix());
                        }
                        continue;
                    }
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
                    // ★ THE RESET-SITE FOR THE DEBT ABOVE. Leaving the channel is precisely the fact that
                    // makes an owed re-send wrong, and the debt is in memory where no store gate can see
                    // it — so cancelling it belongs at the event, paired with the set-site, not left for
                    // the drain alone to notice. Both ends, per the idempotency-pair rule.
                    if let Some(live) = self.live.get_mut(url) {
                        live.resend_due.remove(&Some(channel_id));
                    }
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
                // The second reset-site: a refusal raises the suppression, and a suppressed channel is
                // re-asked by the retry lane alone. An owed re-send would ask again from outside the
                // backoff, unmarked.
                if let Some(live) = self.live.get_mut(url) {
                    live.resend_due.remove(&Some(channel_id));
                }
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
                //
                // ★★★ AND CLAMPED AT THE BOTTOM, WHICH IS THE HALF THAT WAS MISSING. A ceiling stops a
                // peer stalling us; only a FLOOR stops one driving us. `retry in 1` bought the relay one
                // re-send per second — a cadence chosen by the peer, which is the same fault as the
                // uncapped sleep wearing the opposite sign. The floor is the interval that already
                // answers "how often may we re-ask one relay", so there is one rate to reason about
                // rather than two that can drift.
                //
                // A channel that is SUPPRESSED never reaches here at all: `on_closed` turns a rate-limit
                // answering a retry into the refusal it is, so the suppression backoff stays the only
                // owner of that channel's cadence.
                let delay = hint_secs
                    .unwrap_or(DEFAULT_RESEND_DELAY_SECS)
                    .clamp(MIN_RESYNC_INTERVAL_SECS as u64, MAX_RESEND_DELAY_SECS);
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
                //
                // ★★ AND IT CHECKS ITS OWN GATE FIRST. An auth-required relay closes EVERY subscription
                // we hold, so one tick's batch carries one of these per channel plus one for membership.
                // Unguarded, each kicked the socket again and re-stamped the settle from `now` — the relay
                // deciding when its own access gets re-checked, by refusing more. A reconnect that is
                // already owed is already owed; the second `CLOSED` saying so adds nothing.
                if self.live.get(url).is_some_and(Live::quarantined) {
                    return Ok(());
                }
                self.reconnect_relay(url).await;
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
    // ★★★ AUTO-RECONNECT OFF, AND IT IS A CORRECTNESS REQUIREMENT, NOT A TUNING CHOICE.
    //
    // The default pool supervises the socket itself: on any connection loss it waits a retry interval
    // and reconnects, and `post_connection` then replays every registered subscription. That is the same
    // leak [`clear_registrations`] exists to prevent, through a door we cannot stand in front of — a
    // transition the LIBRARY triggers, not one we do. Emptying the registry when OUR pump handles the
    // `CLOSED` is no defence when the pool has already reconnected on its own, which it will whenever the
    // relay drops us and the pump is a tick behind. A transition we do not own cannot be gated, only
    // removed: with `reconnect(false)` every reconnect goes through
    // [`Participation::reconnect_relay`], which clears the registry first and owes a fresh probe after.
    //
    // ★★ AND IT HANDS US A DUTY THE SDK WAS DISCHARGING. With this off, a lost socket ends `Terminated`
    // and the pool never tries again — silent retries become a dead relay nobody revives, which is a
    // TIMER-LESS PARK, the shape this module already refuses everywhere else. So the pump owns liveness
    // now: [`Participation::revive_dead_sockets`]. Disabling supervision without taking it over would
    // have traded a leak for a silence, which is the worse of the two.
    //
    // `RelayOptions::default()` carries exactly the flags `Client::add_relay` would have set
    // (`READ | WRITE | PING`), so `reconnect` is the only behaviour that changes. `sleep_when_idle` is
    // false by default too, which is why `Sleeping` is not a state this reader can be found in.
    client
        .pool()
        .add_relay(url, RelayOptions::default().reconnect(false))
        .await
        .map_err(|error| format!("add relay {url}: {error}"))?;
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(10)).await;
    Ok(client)
}

/// Whether a relay in this state has a socket that is down with nothing left to bring it back.
///
/// ★ SPLIT FROM THE I/O ON PURPOSE. Every judgement in here is a line someone could reasonably draw
/// elsewhere, and as a predicate over plain values it can be pinned by a table test instead of resting on
/// a fixture that cannot produce most of these states. What the caller does with a `true` is a separate
/// question from which states earn one.
///
/// - `Pending` and `Connecting` are attempts IN FLIGHT, not failures. Calling them dead would kick a socket
///   that is halfway up, forever, at pump frequency.
/// - `Sleeping` recovers by itself — `ensure_awake_for_activity` runs ahead of every send. (Our readers set
///   `sleep_when_idle: false`, so it should not arise at all; it is listed because being wrong about it
///   would be expensive and the cost of naming it is a line.)
/// - `Connected` is the whole point.
/// - ★ `successes == 0` is NOT dead, whatever the status says. A relay that never completed a single
///   connection was never alive to lose, so there is nothing to revive; it is [`Participation::start`]'s
///   probe that judges those, and a second opinion from here could only disagree with it.
fn socket_is_dead(status: RelayStatus, successes: usize) -> bool {
    successes > 0
        && matches!(
            status,
            RelayStatus::Disconnected | RelayStatus::Terminated | RelayStatus::Banned
        )
}

/// Ask the SDK for one relay's state and put the question to [`socket_is_dead`].
async fn reader_socket_is_dead(reader: &Client, url: &str) -> bool {
    match reader.relay(url).await {
        Ok(relay) => socket_is_dead(relay.status(), relay.stats().success()),
        // No such relay on this client: there is no socket to revive, and nothing here can fix that.
        Err(_) => false,
    }
}

/// Install the wake surface — both filters, each resuming from its own stored cursor — and return the
/// channels it actually subscribed.
///
/// ★ THE RETURN VALUE IS THE SETTLEMENT RECEIPT. One call here pays several debts at once — a pending
/// resync, an owed membership re-send, an owed re-send for any channel it covers — and a debt that was
/// paid but not cleared fires again next tick as a duplicate REQ. So the caller is told what was
/// installed rather than left to re-derive it: settle against the ARTIFACT, not against a second guess
/// at what this function probably did.
///
/// On a partial failure this reports NOTHING installed even though the earlier subscribes landed. That
/// direction is deliberate: an unsettled debt costs a duplicate REQ, an over-settled one costs a
/// subscription nobody ever re-sends.
async fn subscribe_wake_surface(
    reader: &Client,
    engine: &Engine,
    me: PublicKey,
) -> Result<Vec<String>, ParticipationError> {
    subscribe_membership(reader, me, engine.membership_cursor()?).await?;
    // Exactly the channels we were in when we stopped — read from the store, never re-derived from
    // the membership feed, which resumes past the notifications that admitted us.
    let channels = engine.channels_to_resume()?;
    for channel_id in &channels {
        let since = engine.channel_cursor(channel_id)?;
        subscribe_channel(reader, channel_id, me, since).await?;
    }
    Ok(channels)
}

/// Drop every subscription the SDK still holds registered for this reader.
///
/// ★★★ A GATE OVER OUR OWN CALL SITES IS NOT A GATE. `nostr_sdk` treats `connect()` as a lifecycle
/// event, not a socket operation: `post_connection` calls `resubscribe()` unconditionally for a readable
/// relay, which re-sends a REQ for every registered subscription. Worse, it is guaranteed rather than
/// merely possible on exactly the path we take — an `auth-required` `CLOSED` makes the SDK mark that
/// subscription `closed`, and `should_resubscribe` returns true immediately for a closed non-auto-closing
/// subscription. So the very frame that told us our access may be revoked is what arms the library to ask
/// again, beneath the quarantine, before the probe. Verified in `nostr-relay-pool-0.44.1`
/// (`relay/inner.rs`: `post_connection`, `resubscribe`, `should_resubscribe`, `subscription_closed`).
///
/// ⇒ **the reconnect has nothing to re-send only if the registry is empty when the socket returns.**
///
/// ★★ ONE ID AT A TIME RATHER THAN `unsubscribe_all()`, and the reachability is worth stating exactly,
/// because the obvious argument for it is the wrong one. The bulk helper removes each id from the registry
/// and then sends that id's `CLOSE`, propagating the send error with `?`, so a failed send abandons the loop
/// and leaves every remaining id REGISTERED. Two ways that send can fail, and they are not equally real:
/// - `ensure_operational` rejecting the relay — `Initialized` or `Banned`; `Sleeping` self-heals because
///   `ensure_awake_for_activity` runs first. **This reader is in none of those states here**, so the
///   argument I first wrote down was not a live one.
/// - `send_client_msgs` doing a `try_send` into a BOUNDED queue, which fails when that queue is FULL. That
///   one needs no unusual relay state at all — only backpressure — so it is reachable in ordinary running.
///
/// Per-id therefore guards something real, but **no test in this suite reddens for it**: the fixture cannot
/// fill the outbound queue, and `unsubscribe_all()` clears the registry in every state a fixture can reach,
/// including a socket already dropped. Treat it as insurance with a reason, not as a proven repair — and do
/// not "simplify" it back on the strength of the first bullet alone. Round 9's isolate-per-item rule,
/// applied to a dependency's loop.
async fn clear_registrations(reader: &Client) {
    for id in reader.subscriptions().await.into_keys() {
        reader.unsubscribe(&id).await;
    }
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
    use crate::seller_node::p_gate_relay_fixture::{PGateRelay, Verdict};
    use crate::seller_node::store::RETRY_EOSE_TIMEOUT_SECS;
    use nostr_sdk::prelude::{EventBuilder as TestEventBuilder, Keys, Kind};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    const TEST_RELAY: &str = "wss://relay.invalid";

    fn test_store(label: &str) -> SellerStore {
        test_store_with_path(label).0
    }

    /// The same store, and the path it lives at — which is what lets a test take a table away.
    fn test_store_with_path(label: &str) -> (SellerStore, std::path::PathBuf) {
        let id = SEQ.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "mobee-participation-lag-{label}-{}-{id}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open store");
        (store, path)
    }

    /// Take away exactly the table one store read needs, from a second connection, so that read fails and
    /// nothing else does.
    ///
    /// ★★★ THE ONLY STORE FAULT THIS SUITE CAN INJECT, and the four lines are why three sites stayed wrong
    /// until round 20: with no way to make a store read fail, "a store failure is not the relay's fault"
    /// was a comment everywhere and a check nowhere — one site said it in words and counted one on the very
    /// next line. `SellerStore::open` sets WAL and `synchronous=FULL` per connection and leaves
    /// `locking_mode` at NORMAL, so a second writer is permitted.
    fn take_away_table(path: &std::path::Path, table: &str) {
        let conn = rusqlite::Connection::open(path).expect("open the store a second time");
        conn.execute(&format!("DROP TABLE {table}"), [])
            .expect("drop the table");
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

    fn joined(store: &SellerStore, relay: &str, channel_id: &str, when: i64) {
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
    }

    fn joined_and_suppressed(store: &SellerStore, relay: &str, channel_id: &str, when: i64) {
        joined(store, relay, channel_id, when);
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
    ///
    /// The channel is joined and NOT suppressed on purpose: a rate-limit on a suppressed channel is a
    /// refusal of our retry and never reaches the re-send lane at all. The matching FLOOR — a hint too
    /// small — is [`a_relay_supplied_retry_hint_is_floored_by_our_own_send_interval`].
    #[tokio::test]
    async fn a_relay_supplied_retry_hint_is_capped_and_never_slept_on() {
        let store = test_store("resend-hint");
        let me = Keys::generate().public_key();
        joined(&store, GOOD_RELAY, "chan-1", now_unix());

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

    /// ★ ROUND 11 / FINDING 1. `reprove_due` has to QUARANTINE the relay, not merely schedule a probe.
    ///
    /// The relay accepts everything, so any lane that runs succeeds and visibly clears its own debt. That
    /// is the whole design of this fixture: with an accepting relay, "the lane was gated" and "the lane
    /// ran and failed" cannot be confused — three debts still owed can only mean nothing was sent.
    #[tokio::test]
    async fn an_unproven_relay_is_asked_for_nothing_by_any_recovery_lane() {
        let store = test_store("quarantine");
        let me = Keys::generate().public_key();
        let long_ago = now_unix() - 86_400;
        joined_and_suppressed(&store, GOOD_RELAY, "chan-sup", long_ago);
        // A SEPARATE, unsuppressed channel carries the re-send debt: an owed re-send for `chan-sup` would
        // be cancelled by the resumability gate whether or not the quarantine works, so using it would
        // make this test pass for the wrong reason.
        joined(&store, GOOD_RELAY, "chan-open", long_ago);

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
            store.clone(),
            me,
            long_ago,
        )
        .await;

        // Tick 1 learns about the auth failure in the batch, and `retry_suppressed_channels` runs AFTER
        // the batch in the same tick — so the suppressed channel is the same-tick hazard.
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");
        assert!(
            participation.reprove_deadline(GOOD_RELAY).is_some(),
            "the auth failure did not put the relay into quarantine, so nothing below is being tested"
        );
        assert_eq!(
            store
                .suppressed_channels_due(GOOD_RELAY, now_unix())
                .expect("due"),
            vec![("chan-sup".to_string(), 1)],
            "a retry REQ went out on a socket whose access is unproven — in the very tick that kicked it"
        );

        // Tick 2: the resync and re-send lanes both fire before the probe would, and a fresh invite is the
        // fourth lane — `apply_event` answers one with a REQ of its own.
        participation.owe_resync(GOOD_RELAY);
        participation.owe_resend(GOOD_RELAY, Some("chan-open"), now_unix());
        let invite = TestEventBuilder::new(Kind::Custom(44100), "")
            .tag(nostr_sdk::prelude::Tag::public_key(me))
            .tag(nostr_sdk::prelude::Tag::parse(["h", "chan-new"]).expect("h tag"))
            .sign_with_keys(&Keys::generate())
            .expect("sign invite");
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Event {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(MEMBERSHIP_SUB)),
                    event: std::borrow::Cow::Owned(invite),
                },
            })
            .expect("send invite");
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            participation.resync_pending(GOOD_RELAY),
            "the resync re-subscribed the whole wake surface on an unproven socket"
        );
        assert!(
            participation
                .resend_deadline(GOOD_RELAY, Some("chan-open"))
                .is_some(),
            "the deferred re-send went out on an unproven socket — and a quarantine is not payment, so \
             the debt must still be owed afterwards"
        );
        assert_eq!(
            store
                .suppressed_channels_due(GOOD_RELAY, now_unix())
                .expect("due"),
            vec![("chan-sup".to_string(), 1)],
            "the retry lane ran on an unproven socket on a later tick"
        );
        // The relay accepts, so an ungated `apply_event` would simply succeed and owe nothing. A debt is
        // therefore the only evidence the invite was DEFERRED rather than answered on an unproven socket.
        assert!(
            participation
                .resend_deadline(GOOD_RELAY, Some("chan-new"))
                .is_some(),
            "a fresh invite was answered with a REQ on an unproven socket — and worse, owing nothing, so \
             if the probe then denies the relay the join is recorded with nothing listening"
        );
    }

    /// ★ ROUND 11 / FINDING 3, the other reset-site. A refusal raises the suppression, after which the
    /// retry lane alone may re-ask — an owed re-send would ask from outside the backoff, and unmarked.
    #[tokio::test]
    async fn a_refusal_cancels_the_resend_it_invalidates() {
        let store = test_store("resend-cancelled-refusal");
        let me = Keys::generate().public_key();
        joined(&store, GOOD_RELAY, "chan-x", now_unix() - 100);

        let (good_tx, good_rx) = broadcast::channel(8);
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Closed {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(channel_sub(
                        "chan-x",
                    ))),
                    message: std::borrow::Cow::Borrowed("restricted: not a channel member"),
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
        // Far enough out that the drain's re-validation cannot be what clears it: this is the cancel at
        // the event, and the two ends are separately mutable.
        participation.owe_resend(GOOD_RELAY, Some("chan-x"), now_unix() + 600);
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            participation
                .resend_deadline(GOOD_RELAY, Some("chan-x"))
                .is_none(),
            "a re-send is still owed for a channel the relay just refused — it will re-ask from outside \
             the suppression backoff, and unmarked, so the EOSE answering it clears nothing"
        );
    }

    /// ★ ROUND 11 / FINDING 2. A rate-limit answering a retry we MARKED is a refusal of that retry.
    ///
    /// Treated as a re-send hint it broke twice: the token stayed armed with the relay having answered, so
    /// it later expired as a silence that never happened; and `retry in 1` handed the relay our send
    /// cadence. Both halves are asserted, and then the EXIT — that an `EOSE` still lifts the suppression —
    /// because a fix that closed the rate hole by breaking the way out would look identical here.
    #[tokio::test]
    async fn a_rate_limit_answering_a_retry_is_a_refusal_not_our_new_send_rate() {
        let store = test_store("retry-rate-limit");
        let me = Keys::generate().public_key();
        let long_ago = now_unix() - 86_400;
        joined_and_suppressed(&store, GOOD_RELAY, "chan-1", long_ago);
        // The state a retry REQ leaves behind: outstanding, so the `CLOSED` below is attributable to us.
        store
            .note_retry_attempt(GOOD_RELAY, "chan-1", now_unix())
            .expect("arm");

        let (good_tx, good_rx) = broadcast::channel(8);
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Closed {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(channel_sub(
                        "chan-1",
                    ))),
                    message: std::borrow::Cow::Borrowed("rate-limited: retry in 1 second"),
                },
            })
            .expect("send closed");

        let mut participation = Participation::for_wire_test_many(
            vec![(GOOD_RELAY, true, good_rx)],
            store.clone(),
            me,
            now_unix(),
        )
        .await;
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            participation
                .resend_deadline(GOOD_RELAY, Some("chan-1"))
                .is_none(),
            "a re-send was queued outside the suppression backoff — `retry in 1` buys the relay one REQ \
             per second on a channel it has just refused, which is our send rate chosen by the peer"
        );
        assert_eq!(
            store
                .suppressed_channels_due(GOOD_RELAY, now_unix() + 900)
                .expect("due"),
            vec![("chan-1".to_string(), 2)],
            "the refusal was not charged to the backoff, so the wait did not get longer"
        );
        assert_eq!(
            store
                .expire_stale_retries(GOOD_RELAY, now_unix() + 61, RETRY_EOSE_TIMEOUT_SECS)
                .expect("expire"),
            0,
            "the retry token was left armed after the relay ANSWERED, so it expires as a silence that \
             did not happen — a second backoff step for one refusal"
        );

        // The exit: the next retry arms the token, and the `EOSE` answering it lifts the suppression.
        store
            .note_retry_attempt(GOOD_RELAY, "chan-1", now_unix())
            .expect("arm again");
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::EndOfStoredEvents(std::borrow::Cow::Owned(
                    SubscriptionId::new(channel_sub("chan-1")),
                )),
            })
            .expect("send eose");
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");
        assert!(
            !store
                .channel_suppressed(GOOD_RELAY, "chan-1")
                .expect("suppressed"),
            "the channel can no longer be served out of suppression — the rate fix closed the way out"
        );
    }

    /// ★ ROUND 11 / FINDING 2, the floor. A ceiling stops a peer STALLING us; only a floor stops one
    /// DRIVING us. Same fault, opposite sign.
    #[tokio::test]
    async fn a_relay_supplied_retry_hint_is_floored_by_our_own_send_interval() {
        let store = test_store("resend-floor");
        let me = Keys::generate().public_key();
        joined(&store, GOOD_RELAY, "chan-1", now_unix());

        let (good_tx, good_rx) = broadcast::channel(8);
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Closed {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(channel_sub(
                        "chan-1",
                    ))),
                    message: std::borrow::Cow::Borrowed("rate-limited: retry in 1 second"),
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
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        let deadline = participation
            .resend_deadline(GOOD_RELAY, Some("chan-1"))
            .expect("the re-send was neither sent nor recorded, so the REQ is simply lost");
        assert!(
            deadline >= before + MIN_RESYNC_INTERVAL_SECS,
            "a 1-second hint was honoured as given: the relay now picks how often we re-ask it, which is \
             the rate the floor exists to keep ours"
        );
    }

    /// ★ ROUND 11 / FINDING 3, the set-site's pair. Leaving the channel is what makes an owed re-send
    /// wrong, and the debt lives in memory where no store gate can see it — so the event cancels it.
    #[tokio::test]
    async fn a_leave_cancels_the_resend_it_invalidates() {
        let store = test_store("resend-cancelled");
        let relay_keys = Keys::generate();
        let me = Keys::generate().public_key();
        joined(&store, GOOD_RELAY, "chan-x", now_unix() - 100);

        let removal = TestEventBuilder::new(Kind::Custom(44101), "")
            .tag(nostr_sdk::prelude::Tag::public_key(me))
            .tag(nostr_sdk::prelude::Tag::parse(["h", "chan-x"]).expect("h tag"))
            .sign_with_keys(&relay_keys)
            .expect("sign removal");
        let (good_tx, good_rx) = broadcast::channel(8);
        good_tx
            .send(RelayPoolNotification::Message {
                relay_url: GOOD_RELAY.parse().expect("url"),
                message: RelayMessage::Event {
                    subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(MEMBERSHIP_SUB)),
                    event: std::borrow::Cow::Owned(removal),
                },
            })
            .expect("send removal");

        let mut participation = Participation::for_wire_test_many(
            vec![(GOOD_RELAY, true, good_rx)],
            store,
            me,
            now_unix(),
        )
        .await;
        // Due well after this pump, so the drain cannot be the thing that clears it — this is about the
        // cancel at the event, not the re-validation in the drain.
        participation.owe_resend(GOOD_RELAY, Some("chan-x"), now_unix() + 600);
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            participation
                .resend_deadline(GOOD_RELAY, Some("chan-x"))
                .is_none(),
            "the re-send is still owed for a channel we have been removed from — the debt outlived its \
             reason, and it will re-subscribe us to a channel the relay already took us out of"
        );
    }

    /// ★ ROUND 11 / FINDING 3, the fire-time half. Record-time truth is not fire-time truth: this lane is
    /// the only one that carries a snapshot instead of re-deriving from the store, so it re-validates.
    ///
    /// The relay does NOT accept, which is what makes the two outcomes distinguishable — a cancelled debt
    /// leaves no fault, an attempted send leaves one. On an accepting relay both look the same.
    #[tokio::test]
    async fn the_resend_drain_refuses_a_channel_that_is_no_longer_resumable() {
        let store = test_store("resend-revalidate");
        let me = Keys::generate().public_key();
        joined(&store, BAD_RELAY, "chan-x", now_unix() - 100);
        // Left in the store WITHOUT the event, so the in-memory cancel cannot be what saves us: this is
        // the state a restart, or an ordering we did not think of, can still produce.
        store
            .record_channel_left(BAD_RELAY, "chan-x", &"b".repeat(64), now_unix(), now_unix())
            .expect("leave");

        let (_bad_tx, bad_rx) = broadcast::channel(8);
        let mut participation = Participation::for_wire_test_many(
            vec![(BAD_RELAY, false, bad_rx)],
            store,
            me,
            now_unix(),
        )
        .await;
        participation.owe_resend(BAD_RELAY, Some("chan-x"), now_unix());
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert_eq!(
            participation.relay_faults(),
            0,
            "the drain tried to re-subscribe a channel we are no longer a member of"
        );
        assert!(
            participation
                .resend_deadline(BAD_RELAY, Some("chan-x"))
                .is_none(),
            "the debt was neither sent nor cancelled, so it will be tried again forever"
        );
    }

    /// ★★★ WHOSE FAULT IS A FAILED STORE READ. Three sites answered "the relay's", and the reason all three
    /// survived four reviews is that nothing here could make a store read fail — so the rule was documented
    /// and never measured.
    ///
    /// Both resend tests drive the drain TWICE: once with the store intact, which must charge the relay for
    /// the WIRE failure, and once with the table gone. The first run is the positive control, because "no
    /// fault was charged" is equally true of a drain that never ran at all.
    #[tokio::test]
    async fn a_store_fault_in_the_resend_drain_is_ours_not_the_relays() {
        let (store, path) = test_store_with_path("resend-store-fault");
        let me = Keys::generate().public_key();
        joined(&store, BAD_RELAY, "chan-x", now_unix() - 100);
        let (_tx, rx) = broadcast::channel(8);
        let mut participation =
            Participation::for_wire_test_many(vec![(BAD_RELAY, false, rx)], store, me, 0).await;

        participation.owe_resend(BAD_RELAY, Some("chan-x"), now_unix());
        let intact = participation.drain_resends(now_unix()).await;
        assert!(
            intact.is_ok(),
            "the drain failed with the store still intact, so the fault below would not be the injected \
             one: {intact:?}"
        );
        assert_eq!(
            participation.relay_faults(),
            1,
            "the drain never reached the wire, so the store half of this test would measure nothing"
        );

        // `is_resumable` returns `StoreError`, so this arm has no wire failure it could be describing.
        take_away_table(&path, "participation_channels");
        participation.owe_resend(BAD_RELAY, Some("chan-x"), now_unix());
        let charged = participation.relay_faults();
        let outcome = participation.drain_resends(now_unix()).await;

        assert!(
            matches!(outcome, Err(ParticipationError::Store(_))),
            "the drain either never ran or swallowed a store fault — it returned {outcome:?}"
        );
        assert_eq!(
            participation.relay_faults(),
            charged,
            "our sqlite failing was charged to the relay, which makes relay_faults accuse every relay of \
             one fault that belongs to none of them"
        );
    }

    /// The same fault one read later: membership succeeds and the CURSOR read fails. This is the site where
    /// the store error was WRAPPED into the send's own `Result`, so `outcome.is_ok()` held "the relay
    /// refused" and "our sqlite is down" in one bucket and answered as the first. The fix deleted the
    /// wrapping rather than testing for it, and this proves the read is still made.
    #[tokio::test]
    async fn a_store_fault_reading_the_resend_cursor_is_ours_too() {
        let (store, path) = test_store_with_path("resend-cursor-fault");
        let me = Keys::generate().public_key();
        joined(&store, BAD_RELAY, "chan-y", now_unix() - 100);
        let (_tx, rx) = broadcast::channel(8);
        let mut participation =
            Participation::for_wire_test_many(vec![(BAD_RELAY, false, rx)], store, me, 0).await;

        participation.owe_resend(BAD_RELAY, Some("chan-y"), now_unix());
        let intact = participation.drain_resends(now_unix()).await;
        assert!(intact.is_ok(), "the drain failed with the store intact: {intact:?}");
        assert_eq!(
            participation.relay_faults(),
            1,
            "the drain never reached the wire, so the store half of this test would measure nothing"
        );

        take_away_table(&path, "participation_cursors");
        participation.owe_resend(BAD_RELAY, Some("chan-y"), now_unix());
        let charged = participation.relay_faults();
        let outcome = participation.drain_resends(now_unix()).await;

        assert!(
            matches!(outcome, Err(ParticipationError::Store(_))),
            "the cursor read no longer happens, or its failure was swallowed — the drain returned \
             {outcome:?}"
        );
        assert_eq!(
            participation.relay_faults(),
            charged,
            "our sqlite failing was charged to the relay as a refused subscribe"
        );
    }

    /// The third site, in the pump itself: `resync_relay` reads three cursors before it sends anything, and
    /// its failure arm swallowed the store error eighteen lines above the `?` that propagates the same error
    /// out of the same helper.
    ///
    /// ★ The positive control is NOT re-run inside this test, and cannot be — the failure arm advances the
    /// rate floor, so a second resync on the same relay is throttled by design.
    /// `a_failed_resync_still_advances_the_rate_floor` IS that control: same fixture, same arm, a wire
    /// failure, exactly one fault charged. Here the `Err` carries the evidence instead, because a resync
    /// that never fired leaves the pump returning `Ok`.
    #[tokio::test]
    async fn a_store_fault_in_the_resync_is_not_charged_to_the_relay() {
        let (store, path) = test_store_with_path("resync-store-fault");
        let (_tx, rx) = broadcast::channel(8);
        let mut participation = Participation::for_wire_test_many(
            vec![(BAD_RELAY, false, rx)],
            store,
            Keys::generate().public_key(),
            0,
        )
        .await;
        // `membership_cursor` is the first thing `subscribe_wake_surface` reads, so the store fails before
        // the wire is touched at all.
        take_away_table(&path, "participation_cursors");
        participation.owe_resync(BAD_RELAY);

        let outcome = participation.pump(Duration::from_millis(1)).await;

        assert!(
            matches!(outcome, Err(ParticipationError::Store(_))),
            "the resync arm either never ran or swallowed a store fault — the pump returned {outcome:?}"
        );
        assert_eq!(
            participation.relay_faults(),
            0,
            "our sqlite failing was charged to the relay, and the relay did nothing"
        );
        assert!(
            participation.resync_pending(BAD_RELAY),
            "the debt was discharged by a resync that never happened"
        );
    }

    /// ★ ROUND 11, found in the sweep rather than reported: the reconnect did not check the gate IT sets.
    ///
    /// An auth-required relay closes EVERY subscription we hold, so one batch carries one of these per
    /// channel plus one for membership. Each kicked the socket again and re-stamped the settle from `now`,
    /// which lets a relay postpone the re-check of its own access by refusing more. The observable is the
    /// deadline; the socket kick rides in the same block.
    #[tokio::test]
    async fn a_second_auth_failure_does_not_restamp_a_settle_already_owed() {
        let store = test_store("reconnect-idempotent");
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
        // A settle already owed, at a deadline far enough out that `drain_reproves` will not fire and near
        // enough to nothing else that only a re-stamp could change it.
        let owed = now_unix() + 600;
        participation.owe_reprove(GOOD_RELAY, owed);
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert_eq!(
            participation.reprove_deadline(GOOD_RELAY),
            Some(owed),
            "a second auth-required CLOSED pushed the settle out again — a relay that keeps refusing can \
             hold off the probe of its own access indefinitely, one refusal at a time"
        );
    }

    /// ★ ROUND 12 / FINDING 1. The quarantine governed our call sites; it did not govern the SDK.
    ///
    /// `nostr_sdk`'s `post_connection` calls `resubscribe()` for a readable relay, and
    /// `should_resubscribe` returns true immediately for a subscription the relay marked `closed` — which
    /// an `auth-required` `CLOSED` does. So the reconnect re-sent the whole wake surface on the unproven
    /// socket, beneath every gate in this file.
    ///
    /// ⚠ WHAT THIS TEST CAN AND CANNOT REACH. The fixture relay never completes a websocket handshake, so
    /// `post_connection` never runs in-process and the library's REQ is NOT observable here. What is
    /// observable is the PRECONDITION that makes it possible: subscriptions still registered when the
    /// socket returns. This asserts the registry is empty, which is the link in the chain we control. The
    /// rest of the chain is read from the SDK source, cited on [`clear_registrations`], and the live legs
    /// are where a real handshake happens.
    #[tokio::test]
    async fn a_reconnect_leaves_the_sdk_nothing_to_resubscribe() {
        let store = test_store("reconnect-registrations");
        let me = Keys::generate().public_key();
        joined(&store, GOOD_RELAY, "chan-1", now_unix() - 100);

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
        participation.install_wake_surface(GOOD_RELAY).await;
        assert_eq!(
            participation.registered_subscriptions(GOOD_RELAY).await,
            2,
            "the wake surface did not register (membership + chan-1), so an empty registry afterwards \
             would prove nothing at all"
        );

        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            participation.reprove_deadline(GOOD_RELAY).is_some(),
            "the auth failure did not trigger the reconnect, so nothing below is being tested"
        );
        assert_eq!(
            participation.registered_subscriptions(GOOD_RELAY).await,
            0,
            "the SDK still holds the wake surface registered across the reconnect, so it re-sends every \
             REQ the instant the socket returns — on the socket whose access is in question, before the \
             probe, and underneath the quarantine entirely"
        );
    }

    /// ★ ROUND 12. The clearing has to work on a socket that is already gone — a relay that refuses our
    /// auth and then drops the connection is the ordinary case, not the exotic one.
    ///
    /// ⚠ WHAT THIS TEST DOES NOT PROVE. It does not discriminate the per-id loop in
    /// [`clear_registrations`] from the SDK's bulk `unsubscribe_all()`: both pass here, because neither way
    /// of stranding the bulk helper's remaining ids — a relay `ensure_operational` rejects, or a full
    /// outbound queue — is a state this fixture can produce. Swapping the loop for the bulk call reddens
    /// NOTHING; that is recorded on `clear_registrations` rather than implied by this test's existence.
    #[tokio::test]
    async fn a_dropped_socket_still_gives_up_every_registration() {
        let store = test_store("reconnect-dead-socket");
        let me = Keys::generate().public_key();
        joined(&store, GOOD_RELAY, "chan-1", now_unix() - 100);
        joined(&store, GOOD_RELAY, "chan-2", now_unix() - 100);

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
        participation.install_wake_surface(GOOD_RELAY).await;
        // Three registrations, so "stopped after the first" and "cleared them all" are distinguishable —
        // with one subscription the bulk helper's early return is indistinguishable from success.
        participation.drop_socket(GOOD_RELAY).await;
        assert_eq!(
            participation.registered_subscriptions(GOOD_RELAY).await,
            3,
            "the registry did not survive the dropped socket, so this test cannot be about giving it up"
        );

        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert_eq!(
            participation.registered_subscriptions(GOOD_RELAY).await,
            0,
            "subscriptions are still registered after the reconnect on a dead socket — the bulk \
             unsubscribe abandoned the loop on the first failed CLOSE, and everything behind it stays \
             armed for the SDK to re-send"
        );
    }

    /// ★ ROUND 12 / FINDING 2, and the dual of round 11: a debt that SURVIVES until its work is done must
    /// also DIE once something else does that work.
    ///
    /// One wake-surface rebuild installs the membership filter and every resumable channel, so it pays a
    /// pending resync AND the re-sends owed for those filters. Clearing only the marker that asked for it
    /// left the rest recorded, and they fired again next tick — the same REQs twice, which on a relay that
    /// rate-limits is a refusal we caused ourselves.
    ///
    /// The deadlines are far in the future so `drain_resends` cannot be what cleared them: settlement is
    /// the only thing that can. And `chan-sup` is the control — the rebuild skips a suppressed channel, so
    /// its debt must SURVIVE. Settlement is scoped to what was installed, not to everything owed.
    #[tokio::test]
    async fn a_wake_surface_rebuild_settles_every_debt_it_paid() {
        let store = test_store("settlement");
        let me = Keys::generate().public_key();
        let long_ago = now_unix() - 86_400;
        joined(&store, GOOD_RELAY, "chan-1", long_ago);
        joined_and_suppressed(&store, GOOD_RELAY, "chan-sup", now_unix());

        let (_good_tx, good_rx) = broadcast::channel(8);
        let mut participation = Participation::for_wire_test_many(
            vec![(GOOD_RELAY, true, good_rx)],
            store,
            me,
            long_ago,
        )
        .await;
        let before = now_unix();
        participation.owe_resync(GOOD_RELAY);
        participation.owe_resend(GOOD_RELAY, None, before + 600);
        participation.owe_resend(GOOD_RELAY, Some("chan-1"), before + 600);
        participation.owe_resend(GOOD_RELAY, Some("chan-sup"), before + 600);

        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        assert!(
            !participation.resync_pending(GOOD_RELAY),
            "the resync did not succeed, so nothing below is about settlement"
        );
        assert!(
            participation.resend_deadline(GOOD_RELAY, None).is_none(),
            "the rebuild installed the membership filter and left the re-send owed anyway — the same REQ \
             goes out a second time on the next tick"
        );
        assert!(
            participation
                .resend_deadline(GOOD_RELAY, Some("chan-1"))
                .is_none(),
            "the rebuild subscribed chan-1 and left its re-send owed anyway — a duplicate REQ, and on a \
             rate-limiting relay a refusal we brought on ourselves"
        );
        assert!(
            participation
                .resend_deadline(GOOD_RELAY, Some("chan-sup"))
                .is_some(),
            "a suppressed channel is NOT in the wake surface, so its debt was not paid — settling it here \
             drops a subscription nothing else will ever re-send"
        );
        assert!(
            participation.last_resync_unix(GOOD_RELAY) >= before,
            "the floor did not move, so the work that just happened does not count against the next ask"
        );
    }

    /// ★ ROUND 13 / THE FINDING ITSELF, against a relay that really accepts sockets.
    ///
    /// Two claims, and neither is reachable with a fixture the SDK cannot complete a handshake with:
    ///
    /// 1. **The library must not reconnect behind us.** With the pool's default supervision, a socket the
    ///    RELAY drops is reconnected by the SDK after its retry interval and `post_connection` replays every
    ///    registered subscription — on a relay that may have just refused our auth, with no quarantine set
    ///    and our pump none the wiser. `connections()` counts accepted sockets, so the library acting alone
    ///    is an observation rather than an inference.
    /// 2. **And then the socket is OURS to revive**, because with supervision off nothing else will.
    ///
    /// ⚠ THE WAIT IS LOAD-BEARING, DO NOT TRIM IT. The SDK retries after `DEFAULT_RETRY_INTERVAL` (10s)
    /// plus up to 3s of jitter, so a shorter window would report "the library did not reconnect" for a
    /// library that simply had not got round to it yet — a pass by impatience. This is the one place in the
    /// module where a peer's clock, not ours, sets the cost.
    #[tokio::test]
    async fn the_sdk_never_reconnects_a_participation_reader_behind_us() {
        let relay = PGateRelay::start(Duration::ZERO).await;
        let url = relay.url();
        let store = test_store("sdk-supervision");
        // The reader authenticates as `me`, so the `#p`-gated wake surface is SERVED rather than refused —
        // a refusal would set `reprove_due` by itself and every assertion below would be about that.
        let keys = Keys::generate();
        let me = keys.public_key();
        joined(&store, &url, "chan-1", now_unix() - 100);

        let mut participation = Participation::for_live_test(&url, store, keys).await;
        // ★ Let NIP-42 finish before the wake surface goes out. A REQ that beats AUTH is refused with
        // `restricted:`, and this module turns a refusal of the MEMBERSHIP filter into a reconnect — which
        // sets `reprove_due` all by itself. Without this, the assertions below measure that refusal instead
        // of the socket dying, and one of them did exactly that until the mutation caught it. The pre-auth
        // race is issue #189's own subject; here it is noise, and the assertion after the install is what
        // turns its absence into a fact rather than a hope.
        tokio::time::sleep(Duration::from_millis(500)).await;
        participation.install_wake_surface(&url).await;
        assert!(
            relay
                .wait_until(Duration::from_secs(5), |records| records.len() >= 2)
                .await,
            "the wake surface never reached the fixture relay, so nothing below is about a REPLAY of it"
        );
        let refusals: Vec<Verdict> = relay
            .reqs()
            .await
            .into_iter()
            .map(|record| record.verdict)
            .filter(|verdict| *verdict != Verdict::Eose)
            .collect();
        assert!(
            refusals.is_empty(),
            "the wake surface was REFUSED rather than served ({refusals:?}), so `reprove_due` is already \
             owed for a reason that has nothing to do with a dead socket — every assertion below would be \
             measuring that refusal"
        );
        // TWO sockets, and both are ours by construction: the publisher — which this seam now connects
        // exactly like production's — and the reader under test. The exact number is a positive control
        // on the fixture: if anything else were opening sockets, a later count could not mean a
        // reconnect. It read 1 while the seam's publisher was an unconnected `Client` that reached
        // nothing.
        let settled = relay.connections();
        assert_eq!(
            settled, 2,
            "expected exactly the publisher plus the reader; a different baseline means something else \
             is connecting, and then a later count cannot mean a reconnect"
        );
        let sent_before = relay.reqs().await.len();

        // The relay drops us — the transition the LIBRARY reacts to on its own.
        relay.drop_socket_now().await;
        tokio::time::sleep(Duration::from_secs(14)).await;

        // Compared against the BASELINE, not a literal: the property is "unchanged", and spelling it
        // as a number would silently become a different claim the next time the seam legitimately
        // opens one more socket — which is exactly what just happened to the literal above.
        assert_eq!(
            relay.connections(),
            settled,
            "the SDK reconnected this reader by itself — and `post_connection` replays every registered \
             subscription, so the whole wake surface goes back out on a socket our quarantine never saw"
        );
        assert_eq!(
            relay.reqs().await.len(),
            sent_before,
            "REQs reached the relay that our code never sent — the library replayed the registry"
        );

        // Now the pump: with nobody else supervising, reviving it is ours.
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");
        assert!(
            participation.reprove_deadline(&url).is_some(),
            "a socket that died was left dead — with SDK supervision off nothing else revives it, so \
             participation on this relay stops silently and nothing is owed"
        );
        assert_eq!(
            participation.relay_faults(),
            1,
            "the dead socket was not counted, so a relay that keeps dying looks exactly like a healthy one"
        );
        assert_eq!(
            participation.registered_subscriptions(&url).await,
            0,
            "the registry survived OUR reconnect too, so the socket comes back with the wake surface \
             already armed to replay"
        );

        // ★ And the lane checks the gate IT sets. Kill the freshly revived socket: the relay is now BOTH
        // dead and quarantined, and a revive that ignored the quarantine would kick it again and re-stamp
        // the settle — a relay whose socket keeps dying deciding when its own access gets re-checked.
        assert!(
            tokio::time::timeout(Duration::from_secs(5), async {
                while relay.connections() < settled + 1 {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .is_ok(),
            "our own reconnect never reached the relay, so there is no revived socket to re-kill"
        );
        relay.drop_socket_now().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");
        // Baseline plus EXACTLY ONE revive — expressed relative to `settled` for the same reason as
        // above: a literal here silently becomes a different claim whenever the seam's own socket count
        // changes, and it just did.
        assert_eq!(
            relay.connections(),
            settled + 1,
            "a quarantined relay was reconnected again while its probe was still pending — the settle \
             restarts every tick the socket stays down, so the access re-check never arrives"
        );
        assert_eq!(
            participation.relay_faults(),
            1,
            "the same dead socket was counted twice, so one flapping relay inflates the fault count \
             without anything new having failed"
        );
    }

    /// ★★ THE ONE THING THE ROSTER'S OWN TESTS CANNOT SAY: THAT ANYTHING EVER CALLS IT.
    ///
    /// [`RelayRoster`] knows exactly when a quarantine comes due, and unit tests pin that arithmetic to
    /// the second. But a backoff nobody reads is a label rather than a retry —
    /// [`RelayRoster::relays_to_probe`] was for a long while consulted only at boot, so a relay
    /// quarantined at runtime waited on a caller that never came and was lost until the process
    /// restarted. The property here is therefore the WIRING: the pump, driven normally, goes back to
    /// the wire for a relay whose backoff has elapsed — and does not before it has, nor after its
    /// attempts are spent.
    ///
    /// The walk is the production one, end to end. The relay goes silent on a re-prove, which is the
    /// path that quarantines a relay we are ALREADY SERVING and takes its socket away with it; the tick
    /// then has to open a fresh one to ask again. `connections()` on the fixture is what makes that an
    /// observation about the wire instead of an inference from our own bookkeeping — the roster could
    /// record a probe that never left the process, and did, before the seam was rebuilt.
    ///
    /// ⚠ THE FIXTURE ACKNOWLEDGES AN `EVENT` WITHOUT STORING IT and answers every read with an empty
    /// `EOSE`, so a probe through it can only ever resolve `EchoMissing`. That is why this test is about
    /// the ATTEMPT and never about re-admission: `EchoObserved` is not reachable from any in-process
    /// fixture we have, and an assertion about a state we cannot produce is an assertion about a path
    /// that never runs.
    #[tokio::test]
    async fn the_tick_re_probes_a_quarantined_relay_once_its_backoff_has_elapsed() {
        use super::super::relays::{MAX_PROBE_ATTEMPTS, QUARANTINE_BACKOFF_BASE_SECS};

        let relay = PGateRelay::start(Duration::ZERO).await;
        let url = relay.url();
        let store = test_store("quarantine-tick");
        let keys = Keys::generate();

        let mut participation = Participation::for_live_test(&url, store, keys).await;
        // The publisher and the reader, and nothing else — the same positive control the supervision
        // test above rests on. Every count below is read against this, so a third party opening sockets
        // is caught here rather than misread as a probe.
        let settled = relay.connections();
        assert_eq!(
            settled, 2,
            "expected exactly the publisher plus the reader; a different baseline means something else \
             is connecting, and then a later count cannot mean a probe"
        );

        // ── The relay goes silent on a re-prove. This is the path that quarantines a live relay, and
        //    the one that removes its socket — which is what leaves the retry with no caller at all
        //    unless the tick provides one.
        participation.owe_reprove(&url, now_unix());
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");

        let entry = participation
            .roster
            .get(&url)
            .expect("the roster still holds the relay it just judged");
        assert!(
            matches!(entry.state, AccessState::Quarantined { .. }),
            "a relay that told us NOTHING landed in {:?}; the re-prove path has stopped producing the \
             state whose retry this test is about, so everything below measures a different bug",
            entry.state
        );
        assert!(
            !participation.is_live(),
            "the quarantined socket was kept, so `probe_one_due` would skip this relay for having one \
             — and every count below would then mean nothing about its backoff"
        );
        // ★ THE FIRST NEGATIVE CONTROL, and it costs nothing: the very tick that quarantined this relay
        // must not turn round and probe it. A pass that came from probing everything every tick is the
        // busy-poll the backoff exists to prevent, and it would satisfy the positive assertion below
        // just as well.
        //
        // ⚠ IT IS ORDERED AHEAD OF THE ATTEMPT COUNT DELIBERATELY. Both catch a backoff that is written
        // and then ignored — the extra probe shows up as a socket AND as a second tally — but only this
        // one NAMES it. Read the count first and the failure reports a silence that "was not tallied",
        // which is the opposite of what happened and sends the next reader looking in the wrong place.
        assert_eq!(
            relay.connections(),
            settled,
            "the tick that quarantined this relay probed it again in the same pass — the backoff is \
             being recorded and then ignored"
        );
        assert_eq!(
            entry.probe_attempts, 1,
            "one silence was not tallied exactly once, so the ceiling that ends this retry does not \
             count what it is counting"
        );

        // ── Age the quarantine rather than sleeping through it. A second silence, recorded as if it had
        //    happened a second before its backoff would have expired, is exactly the roster a node
        //    running that long would hold. The second quarantine is one doubling, hence `2 *`; deriving
        //    it from the constant means a change to the base cannot leave this test quietly waiting on a
        //    deadline that has not arrived.
        let aged = now_unix() - (2 * QUARANTINE_BACKOFF_BASE_SECS + 1);
        participation
            .roster
            .record_probe(&url, ProbeOutcome::EchoMissing, aged);
        // ★ THE PRECONDITION, STATED. Without this, a tick that never probes anything would pass the
        // assertion below by agreeing with a roster that considered nothing due — the wiring would go
        // unmeasured and the test would report that as a green.
        assert!(
            participation
                .roster
                .relays_to_probe(now_unix())
                .any(|entry| entry.url == url),
            "the roster does not consider this relay due, so the pump has nothing to be right or wrong \
             about and the wiring is not under test"
        );

        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");
        assert_eq!(
            relay.connections(),
            settled + 1,
            "the pump never went back to a relay whose backoff had elapsed: nothing reaches \
             `probe_one_due` from the tick, so a quarantine is a label and a briefly-silent relay is \
             lost until the process restarts"
        );

        let entry = participation
            .roster
            .get(&url)
            .expect("the roster still holds the relay the tick just probed");
        assert_eq!(
            entry.probe_attempts, MAX_PROBE_ATTEMPTS,
            "the probe the tick opened a socket for was never folded back into the roster, so the \
             attempts never run out and the retry has no end"
        );
        assert!(
            matches!(entry.state, AccessState::Denied { .. }),
            "three silences did not spend the ceiling ({:?}) — this walk retried until the budget was \
             gone, and if that does not end in denial the bound is not the one the roster advertises",
            entry.state
        );

        // ── And the bound holds ON THE WIRE, not only in the roster's arithmetic. A fourth pump must go
        //    nowhere: a tick that kept asking a relay which has told us nothing three times is the
        //    timer-less poll this module refuses everywhere else.
        participation
            .pump(Duration::from_millis(1))
            .await
            .expect("pump");
        assert_eq!(
            relay.connections(),
            settled + 1,
            "the tick probed a relay whose attempts are spent — the ceiling holds in the roster but not \
             in the caller, so the backoff ends in a hot loop instead of a denial"
        );
    }

    /// ★ ROUND 13. Every relay state, named, with the verdict that decides whether we kick the socket.
    ///
    /// This is a table rather than a couple of examples because the lane it feeds cannot be driven in
    /// process — no fixture relay ever completes a connection, so none can be found dead. The judgement is
    /// therefore the only part a test can hold, and it holds all of it: get `Pending` wrong and a socket
    /// halfway up is kicked every tick; get `success == 0` wrong and a relay `start` already judged gets
    /// revived by a lane with no way to reach a different verdict.
    #[test]
    fn only_a_socket_that_was_alive_and_is_now_down_counts_as_dead() {
        // Every variant of `RelayStatus`, so a state added upstream cannot slip through unjudged. The
        // reason each one falls where it does is on [`socket_is_dead`] rather than repeated here.
        let alive_once = 1;
        for (status, dead) in [
            (RelayStatus::Initialized, false),
            (RelayStatus::Pending, false),
            (RelayStatus::Connecting, false),
            (RelayStatus::Connected, false),
            (RelayStatus::Disconnected, true),
            (RelayStatus::Terminated, true),
            (RelayStatus::Banned, true),
            (RelayStatus::Sleeping, false),
        ] {
            assert_eq!(
                socket_is_dead(status, alive_once),
                dead,
                "a relay in {status} must {} be kicked and re-proven — see `socket_is_dead` for why",
                if dead { "" } else { "NOT" }
            );
            // ★ The never-alive guard holds for EVERY status, not just the obvious ones — a relay that
            // never connected is `start`'s to judge whatever state it ended in.
            assert!(
                !socket_is_dead(status, 0),
                "{status} with zero successful connections was called dead — it was never alive to lose, \
                 so reviving it second-guesses the probe that already denied it"
            );
        }
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
