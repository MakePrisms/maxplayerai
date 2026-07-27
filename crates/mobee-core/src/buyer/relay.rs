//! The buyer's one long-lived relay connection.
//!
//! Every buyer write rides this single authenticated socket, and the delivery watcher's
//! subscriptions will ride it too. Before it, the buyer had no persistent relay presence at all:
//! each publish, each `has_award`, each job-view fetch built a fresh `Client` (fresh
//! `MemoryDatabase`, fresh WebSocket, fresh NIP-42 handshake), did one thing, and disconnected —
//! which also made `get_job(wait_for=…)` a reconnect-per-iteration poll where a subscription is the
//! correct primitive (#175).
//!
//! ## Why an actor, and not a shared `Client`
//!
//! The money write does not run on the daemon's runtime. `CdkPaymentEffects` builds a
//! `new_current_thread` runtime on its own OS thread and awaits the payment gift-wrap there, so a
//! `Client` owned by the daemon's multi-thread runtime cannot simply be handed to it. Channels
//! cross runtimes; `Client`s do not. So the client lives in one task on the daemon runtime and
//! every caller reaches it through [`RelayHandle`] — plain `mpsc` + `oneshot`, runtime-agnostic.
//!
//! Both legs of that round-trip are bounded, for the same reason the signer's are: a timer-less
//! await is the one thing that can park a caller permanently and silently, and this handle is
//! reached from tasks that also own the trade loop. An unbounded write actor would reintroduce the
//! signer-park class at a brand-new site.
//!
//! ## What this layer does NOT do
//!
//! It does not retry an `auth-required:` rejection. `Relay::send_event` already waits for
//! authentication and resends (`nostr-relay-pool` `relay/mod.rs:434-470`), bounded by
//! `WAIT_FOR_AUTHENTICATION_TIMEOUT` + `WAIT_FOR_OK_TIMEOUT`. Layering our own retry on top would
//! make ours the third publish of the same event and blur which failure the caller sees.
//!
//! That SDK resend is, however, silently conditional on `is_auto_authentication_enabled() &&
//! has_signer()` — nothing errors or logs if either goes false. So [`spawn`] sets
//! `automatic_authentication(true)` explicitly as a drift guard, and a tooth pins the contract
//! itself: a write refused with `auth-required:` must still land once auth completes.

use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::{
    Client, Event, Filter, Keys, Kind, RelayOptions, RelayPoolNotification, RelayUrl,
    SubscriptionId,
};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::relay_auth::{self, AuthWait};

/// How long to wait for the socket and for the NIP-42 handshake on it.
const CONNECT_WAIT: Duration = Duration::from_secs(20);

/// Outer bound on one publish.
///
/// Sized to sit ABOVE the SDK's own worst case rather than to cut it short: a first `OK` wait
/// (`WAIT_FOR_OK_TIMEOUT`, 10s) + an `auth-required:` re-auth (`WAIT_FOR_AUTHENTICATION_TIMEOUT`,
/// 7s) + the resend's `OK` wait (10s) = 27s of legitimate work. A tighter bound here would abandon
/// writes the SDK was about to land, and on the money path that manufactures exactly the ambiguity
/// (sent? not sent?) this daemon exists to avoid. It is a liveness backstop, not a latency budget.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(45);

/// How long the liveness probe waits for its `EOSE`. A `limit(0)` REQ is answered in milliseconds
/// by a healthy relay.
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on one round-trip to the relay actor.
///
/// Strictly greater than [`PUBLISH_TIMEOUT`]: if the handle gave up first, the actor would still be
/// completing the write while the caller had already been told it failed — the reply would land in
/// a dropped channel and a possibly-accepted write would be reported as a failure. The handle bound
/// exists to catch an actor that is GONE, not to race the work it was asked to do.
const RELAY_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Stable id for the liveness REQ, so a relay `CLOSED` names it.
const LIVENESS_PROBE_SUB_ID: &str = "mobee-buyer-liveness";

/// Stable id for the buyer's job-event REQ, so a relay `CLOSED` names which subscription died.
const JOB_EVENTS_SUB_ID: &str = "mobee-buyer-jobs";

/// Depth of the fan-out channel carrying job events to waiters. A waiter that falls behind sees
/// `Lagged`, which is a re-check signal and NOT an error — see [`RelayHandle::subscribe_events`].
const JOB_EVENT_CHANNEL_DEPTH: usize = 256;

enum Command {
    Publish {
        event: Box<Event>,
        reply: oneshot::Sender<Result<PublishReceipt, PublishError>>,
    },
    /// Ask the relay to serve one trivial REQ on the CURRENT session.
    Probe {
        reply: oneshot::Sender<bool>,
    },
    /// Drop the socket and bring a fresh authenticated one up.
    Reconnect {
        reply: oneshot::Sender<Result<AuthWait, String>>,
    },
}

/// A write the relay accepted.
#[derive(Debug, Clone)]
pub struct PublishReceipt {
    pub event_id: String,
    pub relay: String,
}

/// A write that did not land. Never conflated with success: an empty accepted-set is a failure
/// here, so a caller can only read "published" from a relay that actually said so.
#[derive(Debug, Clone)]
pub enum PublishError {
    /// The relay answered and refused. Carries the relay's own reason verbatim.
    Refused(String),
    /// The write did not complete within [`PUBLISH_TIMEOUT`].
    TimedOut,
    /// The write could not be attempted (relay not added, bad url, database error).
    Transport(String),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) => write!(formatter, "relay refused the write: {reason}"),
            Self::TimedOut => write!(
                formatter,
                "relay write did not complete within {PUBLISH_TIMEOUT:?}"
            ),
            Self::Transport(error) => write!(formatter, "relay write could not be sent: {error}"),
        }
    }
}

impl std::error::Error for PublishError {}

/// The relay actor exited, or failed to answer within [`RELAY_CALL_TIMEOUT`]. Names the call and
/// the leg so an operator sees which round-trip stalled.
#[derive(Debug)]
pub struct RelayActorGone {
    call: &'static str,
    cause: &'static str,
}

impl std::fmt::Display for RelayActorGone {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "relay round-trip `{}` did not complete: {}",
            self.call, self.cause
        )
    }
}

impl std::error::Error for RelayActorGone {}

/// The relay connection could not be brought up at all.
#[derive(Debug)]
pub struct RelayBootError(pub String);

impl std::fmt::Display for RelayBootError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for RelayBootError {}

/// A cheap, cloneable handle to the relay actor. Safe to hold from any runtime.
#[derive(Clone)]
pub struct RelayHandle {
    tx: mpsc::Sender<Command>,
    relay_url: String,
    events: broadcast::Sender<Arc<Event>>,
}

impl RelayHandle {
    /// The relay this buyer is bound to.
    pub fn relay_url(&self) -> &str {
        &self.relay_url
    }

    /// Live seller-authored events for this buyer's jobs (claims, results, feedback).
    ///
    /// Synchronous and cheap by design — no round-trip to the actor. That matters because a waiter
    /// MUST subscribe BEFORE it does its catch-up fetch: an event landing in the gap between the
    /// fetch and the subscribe is gone, and the waiter then sleeps until its deadline holding a
    /// view that was already stale when it read it. Making this a plain call keeps the correct
    /// order the natural one to write.
    ///
    /// `RecvError::Lagged` is NOT an error. It means this receiver fell behind and events were
    /// dropped for it — i.e. *something happened*. A waiter must treat it as a re-check signal;
    /// treating it as a failure turns a busy relay into spurious timeouts.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }

    /// Send one command and await its reply, with BOTH legs bounded — see the module doc.
    async fn round_trip<T>(
        &self,
        call: &'static str,
        command: Command,
        rx: oneshot::Receiver<T>,
    ) -> Result<T, RelayActorGone> {
        match tokio::time::timeout(RELAY_CALL_TIMEOUT, self.tx.send(command)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(RelayActorGone {
                    call,
                    cause: "actor exited",
                })
            }
            Err(_) => {
                return Err(RelayActorGone {
                    call,
                    cause: "queue stayed full (actor not draining)",
                })
            }
        }
        match tokio::time::timeout(RELAY_CALL_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(RelayActorGone {
                call,
                cause: "actor dropped the reply",
            }),
            Err(_) => Err(RelayActorGone {
                call,
                cause: "actor never answered",
            }),
        }
    }

    /// Publish one signed event on the buyer's authenticated session.
    pub async fn publish(&self, event: Event) -> Result<Result<PublishReceipt, PublishError>, RelayActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip(
            "publish",
            Command::Publish {
                event: Box::new(event),
                reply,
            },
            rx,
        )
        .await
    }

    /// True when the relay is serving OUR subscriptions on THIS authenticated session.
    pub async fn probe(&self) -> Result<bool, RelayActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip("probe", Command::Probe { reply }, rx).await
    }

    /// Rebuild the session: drop the socket and re-authenticate a fresh one.
    pub async fn reconnect(&self) -> Result<Result<AuthWait, String>, RelayActorGone> {
        let (reply, rx) = oneshot::channel();
        self.round_trip("reconnect", Command::Reconnect { reply }, rx)
            .await
    }
}

/// Register the buyer's relay and spawn the actor that owns the session.
///
/// Returns as soon as the relay is REGISTERED — it does not wait for the socket or the handshake.
/// That is load-bearing, not an optimisation: the daemon binds its Unix socket after bootstrap, and
/// `mobee`'s connect-or-spawn gives a cold daemon only `SPAWN_READY_TIMEOUT` (10s) to appear
/// (`crates/mobee/src/daemon.rs:21`). Waiting here for a handshake bounded at 20s would let an
/// unreachable — or merely lazily-challenging — relay push the socket past that deadline, and every
/// MCP call would report a daemon that failed to start while it was in fact coming up fine.
///
/// So the session is brought up by the actor as its FIRST action, before it serves any command: the
/// daemon is responsive immediately, and a caller that publishes during boot simply queues behind
/// the handshake rather than racing it.
///
/// A relay that cannot be reached is NOT fatal — the daemon must serve `status` with the network
/// down. Only a malformed url or a relay the pool refuses to register fails here; those are
/// configuration errors, not weather.
pub async fn spawn(keys: Keys, relay_url: &str) -> Result<RelayHandle, RelayBootError> {
    let client = Client::new(keys.clone());
    // Explicit, not inherited. The SDK's `auth-required:` resend is conditional on this being
    // true, and it fails SILENTLY if it is not — see the module doc.
    client.automatic_authentication(true);
    client
        .pool()
        .add_relay(relay_url, RelayOptions::default().reconnect(true))
        .await
        .map_err(|error| RelayBootError(format!("buyer relay add_relay: {error}")))?;
    let parsed = RelayUrl::parse(relay_url)
        .map_err(|error| RelayBootError(format!("buyer relay url: {error}")))?;
    let relay = client
        .relays()
        .await
        .get(&parsed)
        .cloned()
        .ok_or_else(|| RelayBootError("buyer relay missing after add_relay".into()))?;

    let public_key = keys.public_key();
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    let (events, _) = broadcast::channel::<Arc<Event>>(JOB_EVENT_CHANNEL_DEPTH);
    let owned_url = relay_url.to_owned();

    // The session task: bring the socket up, subscribe, then FAN OUT events for the rest of its
    // life. Commands are served by a sibling task rather than this one, because a publish may
    // legitimately occupy PUBLISH_TIMEOUT — and if that ran here, no job event would be forwarded
    // for the duration. A delivery arriving during a slow write would reach waiters late or, past
    // the channel depth, not at all.
    let command_client = client.clone();
    let event_sender = events.clone();
    tokio::spawn(async move {
        connect_and_authenticate(&client, &relay, "boot").await;
        // AFTER auth, never before: an unauthenticated `#p` REQ can be CLOSED with `restricted:`,
        // which nostr-sdk treats as Remove — the subscription is dropped and no later
        // `resubscribe()` brings it back. Ordering here is the difference between a live
        // subscription and a permanently deaf one that still looks connected.
        subscribe_job_events(&client, public_key).await;

        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    Command::Publish { event, reply } => {
                        let _ =
                            reply.send(publish_bounded(&command_client, &owned_url, &event).await);
                    }
                    Command::Probe { reply } => {
                        let _ = reply.send(
                            probe_relay_serves_our_reqs(
                                &command_client,
                                public_key,
                                LIVENESS_PROBE_TIMEOUT,
                            )
                            .await,
                        );
                    }
                    Command::Reconnect { reply } => {
                        let outcome = reconnect_and_authenticate(&command_client, &relay).await;
                        if let Ok(wait) = &outcome {
                            report_auth_wait("reconnect", Ok(*wait));
                            // Re-assert the job REQ on the NEW session rather than trusting the
                            // pool to replay it, and only after auth has completed — same ordering
                            // rule as boot. The stable id makes this idempotent.
                            subscribe_job_events(&command_client, public_key).await;
                        }
                        let _ = reply.send(outcome.map_err(|error| error.to_string()));
                    }
                }
            }
        });

        forward_job_events(&client, event_sender).await;
    });

    Ok(RelayHandle {
        tx,
        relay_url: relay_url.to_owned(),
        events,
    })
}

/// Subscribe to every seller-authored event addressed to this buyer.
///
/// ONE STATIC FILTER covers every job this buyer has or will ever have, because claims, results and
/// feedback all `p`-tag the buyer (`gateway::claim_draft` / `result_draft` / `error_draft`). So
/// there is no per-job subscription to open when a job is posted, none to close when it settles,
/// and no filter to rebuild on reconnect — a dynamic `#e`-list would be a subscription-lifecycle
/// problem taken on for no benefit.
///
/// The `#t=mobee` guard keeps a foreign event squatting these kinds from ever being delivered.
async fn subscribe_job_events(client: &Client, buyer_pubkey: nostr_sdk::PublicKey) {
    let filter = Filter::new()
        .kinds([
            Kind::Custom(crate::kinds::JOB_CLAIM_KIND),
            Kind::Custom(crate::kinds::JOB_RESULT_KIND),
            Kind::Custom(crate::kinds::JOB_FEEDBACK_KIND),
        ])
        .hashtag(crate::gateway::MOBEE_TAG)
        .pubkey(buyer_pubkey);
    if let Err(error) = client
        .subscribe_with_id(SubscriptionId::new(JOB_EVENTS_SUB_ID), filter, None)
        .await
    {
        eprintln!(
            "buyer relay: job-event subscription could not be opened ({error}); waiters fall back \
             to their safety re-check until a reconnect restores it"
        );
    }
}

/// Fan out job events to every waiter for the life of the session.
///
/// Nothing here interprets an event: an arrival means only "something changed for some job", and
/// the waiter re-reads the authoritative view. The subscription is the WAKE; the fetch is the TRUTH.
/// Assembling a view from this stream would duplicate view-assembly and force us to trust a stream
/// that may be partial.
///
/// No own-echo hazard exists on this path and it must stay that way: every kind here is
/// SELLER-authored, so the buyer never needs to observe its own event — which a single client
/// cannot do anyway. A future "did my own publish land" check must NOT be built on this stream.
async fn forward_job_events(client: &Client, events: broadcast::Sender<Arc<Event>>) {
    let mut notifications = client.notifications();
    loop {
        match notifications.recv().await {
            Ok(RelayPoolNotification::Message {
                message: nostr_sdk::RelayMessage::Closed { subscription_id, message },
                ..
            }) if subscription_id.to_string() == JOB_EVENTS_SUB_ID => {
                // A CLOSED while the socket is up is the silent-deafness case: connected, and
                // serving nothing. Name it — a waiter's safety re-check will carry the load until
                // a reconnect re-subscribes, but an operator needs to see why waits got slow.
                eprintln!(
                    "buyer relay: the relay CLOSED our job-event subscription ({message}); waits \
                     degrade to the safety re-check until a reconnect re-subscribes"
                );
            }
            Ok(RelayPoolNotification::Event {
                subscription_id,
                event,
                ..
            }) if subscription_id.to_string() == JOB_EVENTS_SUB_ID => {
                // A send failure means only that nobody is waiting right now — not a fault.
                let _ = events.send(Arc::new(*event));
            }
            Ok(_) => continue,
            // The notification stream ending means the pool is gone; so is this session.
            Err(broadcast::error::RecvError::Closed) => return,
            // We fell behind the relay. We cannot know what we missed, so say so: every waiter's
            // safety re-check is what recovers the state we dropped.
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                eprintln!(
                    "buyer relay: fell behind the relay notification stream, {missed} dropped; \
                     waiters recover via their safety re-check"
                );
            }
        }
    }
}

/// Bring the session up on a socket that is not yet connected, and report the handshake.
///
/// The notification receiver is taken BEFORE `connect` because `Authenticated` is emitted once and
/// is not re-emitted — subscribing after the connect can miss it entirely on a fast relay.
async fn connect_and_authenticate(
    client: &Client,
    relay: &nostr_sdk::prelude::Relay,
    phase: &str,
) {
    let mut relay_notifications = relay.notifications();
    client.connect().await;
    client.wait_for_connection(CONNECT_WAIT).await;
    report_auth_wait(
        phase,
        relay_auth::wait_for_nip42_auth(&mut relay_notifications, CONNECT_WAIT).await,
    );
}

/// Log the NIP-42 outcome. UNCONDITIONAL and named: a degraded session is an operator signal, and
/// a path that is silent in its uninteresting case is invisible in its interesting one.
fn report_auth_wait(phase: &str, outcome: Result<AuthWait, relay_auth::RelayAuthError>) {
    match outcome {
        Ok(AuthWait::Authenticated) => {
            eprintln!("buyer relay [{phase}]: NIP-42 authenticated");
        }
        Ok(AuthWait::NoChallenge) => {
            eprintln!(
                "buyer relay [{phase}] DEGRADED: no NIP-42 challenge within {CONNECT_WAIT:?}; \
                 proceeding unauthenticated. An auth-gated write will be refused once with \
                 `auth-required:` and resent by the SDK after the challenge lands."
            );
        }
        Err(error) => {
            eprintln!(
                "buyer relay [{phase}] DEGRADED: NIP-42 did not complete ({error}); writes may be \
                 refused until a reconnect re-authenticates"
            );
        }
    }
}

/// One publish, bounded, with the relay's own answer preserved.
///
/// An empty accepted-set is a FAILURE, never a quiet success — the caller must not be able to read
/// "published" out of a relay that never said so.
async fn publish_bounded(
    client: &Client,
    relay_url: &str,
    event: &Event,
) -> Result<PublishReceipt, PublishError> {
    let sent = tokio::time::timeout(PUBLISH_TIMEOUT, client.send_event_to([relay_url], event)).await;
    let output = match sent {
        Err(_) => return Err(PublishError::TimedOut),
        Ok(Err(error)) => return Err(PublishError::Transport(error.to_string())),
        Ok(Ok(output)) => output,
    };
    if output.success.is_empty() {
        let reason = output
            .failed
            .iter()
            .map(|(url, reason)| format!("{url}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(PublishError::Refused(if reason.is_empty() {
            "no relay accepted the write and none gave a reason".to_owned()
        } else {
            reason
        }));
    }
    Ok(PublishReceipt {
        event_id: output.val.to_string(),
        relay: relay_url.to_owned(),
    })
}

/// Ask the relay to serve one trivial REQ on the CURRENT session and wait for its `EOSE`.
///
/// The `EOSE` is a RESPONSE the relay OWES us for a request we made, which is the whole point: a
/// broadcast we merely hope to receive proves nothing by its absence, because the relay may
/// legitimately decline to send it. And it must not be built on observing our own published event
/// — a single `Client` can never be delivered its own echo (the publish saves into the client's own
/// database, and the inbound handler drops anything already present), so such a probe can never
/// succeed at all.
async fn probe_relay_serves_our_reqs(
    client: &Client,
    buyer_pubkey: nostr_sdk::PublicKey,
    timeout: Duration,
) -> bool {
    // Receiver BEFORE the REQ — an EOSE that lands first would otherwise be missed.
    let mut notifications = client.notifications();
    let probe_id = SubscriptionId::new(LIVENESS_PROBE_SUB_ID);
    // `limit(0)` asks for zero stored events, so the relay's only work is the EOSE. Scoped to our
    // own offers so the filter is narrow and unambiguous even if it ever did match.
    let probe = Filter::new()
        .kind(Kind::Custom(crate::kinds::JOB_OFFER_KIND))
        .author(buyer_pubkey)
        .limit(0);
    if let Err(error) = client.subscribe_with_id(probe_id, probe, None).await {
        eprintln!("buyer relay liveness probe: REQ could not be sent ({error})");
        return false;
    }
    tokio::time::timeout(timeout, async {
        loop {
            match notifications.recv().await {
                Ok(RelayPoolNotification::Message {
                    message: nostr_sdk::RelayMessage::EndOfStoredEvents(id),
                    ..
                }) if id.to_string() == LIVENESS_PROBE_SUB_ID => return true,
                Ok(_) => continue,
                // The stream ending is itself a loss of liveness.
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Drop the live socket and bring a fresh authenticated one up, returning once NIP-42 has completed
/// on the NEW connection.
///
/// ORDER IS LOAD-BEARING. `Relay::disconnect` emits `RelayNotification::Shutdown` on the relay's own
/// notification channel; a receiver taken BEFORE the disconnect inherits that Shutdown and the auth
/// wait reads it as "relay shutdown before NIP-42 authentication" — on a socket that in fact
/// authenticated fine. A `broadcast::Receiver` only observes sends made after it subscribes, so
/// taking it AFTER the disconnect cannot inherit our own teardown, while still taking it BEFORE
/// `connect` so the one-shot `Authenticated` cannot be missed. Both halves are required.
async fn reconnect_and_authenticate(
    client: &Client,
    relay: &nostr_sdk::prelude::Relay,
) -> Result<AuthWait, relay_auth::RelayAuthError> {
    client.disconnect().await;
    let mut relay_notifications = relay.notifications();
    client.connect().await;
    client.wait_for_connection(CONNECT_WAIT).await;
    relay_auth::wait_for_nip42_auth(&mut relay_notifications, CONNECT_WAIT).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // TOOTH — the DEPENDENCY CONTRACT the buyer's write path rests on.
    //
    // A write refused with `auth-required:` must still land once NIP-42 completes. We do not
    // implement that resend: `Relay::send_event` does (nostr-relay-pool relay/mod.rs:434-470). But
    // it is conditional on `is_auto_authentication_enabled() && has_signer()`, and NOTHING errors or
    // logs if either goes false — a "cleanup" that drops `automatic_authentication(true)` from client
    // construction would delete the resend silently, and the symptom would surface far from that
    // diff, on the money path. So the contract is pinned here rather than assumed.
    //
    // Both arms are required, and the second is what gives the first meaning:
    //   1. built the way `spawn` builds it  ⇒ the write LANDS (proving the SDK resend carried it).
    //   2. built with auto-auth OFF         ⇒ the write is REFUSED (proving arm 1 was not just an
    //      unauthenticated relay letting everything through — i.e. that the gate is real).
    //
    // The fixture is `RelayBuilderNip42Mode::Both`, which refuses an EVENT from an unauthenticated
    // session with `MachineReadablePrefix::AuthRequired` (nostr-relay-builder local/inner.rs:420-438)
    // — the exact wire condition mobee-relay produces. A fixture that served writes unauthenticated
    // would make this whole test decorative.
    //
    // BITE: set `automatic_authentication(false)` in `spawn`, or bump to an SDK whose `send_event`
    // no longer resends — either way arm 1 goes red.
    //
    // Note what the explicit `automatic_authentication(true)` in `spawn` does and does not buy:
    // the SDK default is ALREADY true (`pool/options.rs:21`), so merely DELETING that line changes
    // nothing today and this tooth would stay green. It is a guard against an upstream default
    // flip and against a future caller disabling it — not against local deletion. Arm 2 is what
    // demonstrates the option is load-bearing at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_auth_required_refusal_is_resent_after_auth_and_only_when_auto_auth_is_on() {
        use nostr_relay_builder::prelude::{
            LocalRelay, RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode,
        };
        use nostr_sdk::prelude::EventBuilder;

        let relay_fixture = LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42 {
            mode: RelayBuilderNip42Mode::Both,
        }));
        relay_fixture.run().await.expect("fixture relay run");
        let relay_url = relay_fixture.url().await.to_string();

        let buyer = Keys::generate();
        let handle = spawn(buyer.clone(), &relay_url)
            .await
            .expect("buyer relay spawn");

        let note = EventBuilder::text_note("mobee buyer write-path contract")
            .sign(&buyer)
            .await
            .expect("sign");
        let note_id = note.id;
        let receipt = handle
            .publish(note)
            .await
            .expect("actor answered")
            .expect("the write must land: the relay refuses it once with auth-required, and the \
                     SDK is expected to authenticate and resend it");
        assert_eq!(receipt.event_id, note_id.to_string());

        // Arm 2 — the drift case. Same fixture, same key, auto-auth OFF: the refusal must STAND.
        // Without this arm, arm 1 would still pass against a relay that never enforced anything.
        let drifted = Client::new(buyer.clone());
        drifted.automatic_authentication(false);
        drifted
            .pool()
            .add_relay(&relay_url, RelayOptions::default().reconnect(true))
            .await
            .expect("add relay");
        drifted.connect().await;
        drifted.wait_for_connection(CONNECT_WAIT).await;
        let drifted_note = EventBuilder::text_note("mobee buyer write-path drift arm")
            .sign(&buyer)
            .await
            .expect("sign");
        let refused = publish_bounded(&drifted, &relay_url, &drifted_note).await;
        assert!(
            matches!(refused, Err(PublishError::Refused(_))),
            "with automatic_authentication(false) the auth-gated write must stay refused — if this \
             passes, the fixture is not enforcing NIP-42 and arm 1 proves nothing, got {refused:?}"
        );
    }

    // TOOTH — an empty accepted-set is a FAILURE, never a quiet success.
    //
    // `send_event_to` returns `Ok(output)` even when the relay refused: the reason goes into
    // `output.failed` and `output.success` is left empty. Reading only the outer `Result` would
    // report a refused write as published — and on the money path "we think we sent it" is the
    // ambiguity that costs real sats. Covered by arm 2 above at the integration level; asserted here
    // on the classifier itself so the rule survives a refactor of the caller.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_relay_that_accepts_nothing_is_reported_as_refused() {
        use nostr_relay_builder::prelude::{
            LocalRelay, RelayBuilder, RelayBuilderNip42, RelayBuilderNip42Mode,
        };
        use nostr_sdk::prelude::EventBuilder;

        let relay_fixture = LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42 {
            mode: RelayBuilderNip42Mode::Both,
        }));
        relay_fixture.run().await.expect("fixture relay run");
        let relay_url = relay_fixture.url().await.to_string();

        let buyer = Keys::generate();
        let client = Client::new(buyer.clone());
        client.automatic_authentication(false);
        client
            .pool()
            .add_relay(&relay_url, RelayOptions::default().reconnect(true))
            .await
            .expect("add relay");
        client.connect().await;
        client.wait_for_connection(CONNECT_WAIT).await;

        let note = EventBuilder::text_note("refused")
            .sign(&buyer)
            .await
            .expect("sign");
        match publish_bounded(&client, &relay_url, &note).await {
            Err(PublishError::Refused(reason)) => assert!(
                !reason.is_empty(),
                "a refusal must carry the relay's own reason, or an operator cannot triage it"
            ),
            other => panic!("a refused write must not read as published, got {other:?}"),
        }
    }

    // TOOTH — `spawn` must NOT wait for the relay handshake, even when the relay is unreachable.
    //
    // The daemon binds its Unix socket after bootstrap, and connect-or-spawn gives a cold daemon
    // only SPAWN_READY_TIMEOUT (10s, crates/mobee/src/daemon.rs:21) to appear. If the session came
    // up inside `spawn`, an unreachable relay would hold the handshake for CONNECT_WAIT (20s), the
    // socket would miss that deadline, and EVERY MCP call would report a daemon that failed to
    // start — while it was in fact coming up fine. The failure would look like a daemon bug and the
    // cause would be a relay one layer away.
    //
    // 5s is deliberately loose: registration is sub-millisecond, and the regression it guards
    // against is 20s. Anything in between means someone moved the handshake back into `spawn`.
    //
    // BITE: await `connect_and_authenticate` inside `spawn` instead of at the top of the actor, and
    // this goes red against the dead port below.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_returns_immediately_when_the_relay_is_unreachable() {
        // Port 1 is privileged and nothing listens there; the dial cannot succeed.
        let unreachable = "ws://127.0.0.1:1";
        let handle = tokio::time::timeout(
            Duration::from_secs(5),
            spawn(Keys::generate(), unreachable),
        )
        .await
        .expect(
            "spawn must not block on the relay handshake — a daemon whose socket waits on an \
             unreachable relay misses connect-or-spawn's 10s readiness deadline",
        )
        .expect("registering an unreachable relay is not a configuration error");
        assert_eq!(handle.relay_url(), unreachable);
    }

    // TOOTH (#173 class, new site) — the relay handle's round-trip is BOUNDED.
    //
    // This handle is reached from the payment worker's runtime and from daemon tasks that own the
    // trade loop. An unbounded round-trip here would park those callers permanently and silently,
    // exactly as the signer's did — a new site for a class we already closed once, which is why it
    // is toothed at BOTH sites rather than trusted to the pattern.
    //
    // Time is paused, so the production bound elapses instantly; the OUTER timeout is what makes a
    // revert fail cleanly instead of hanging the suite.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_relay_actor_cannot_park_the_caller() {
        let (tx, mut rx) = mpsc::channel::<Command>(8);
        // The stalled actor: receive, then hold. Never answers, never drops a reply sender.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(command) = rx.recv().await {
                held.push(command);
            }
        });
        let handle = RelayHandle {
            tx,
            relay_url: "wss://example.invalid".to_owned(),
            events: broadcast::channel(1).0,
        };

        let outer = Duration::from_secs(600);
        for attempt in 1..=2 {
            let call = tokio::time::timeout(outer, handle.probe());
            let outcome = call.await.unwrap_or_else(|_| {
                panic!(
                    "attempt {attempt}: the relay round-trip never returned — an unbounded \
                     timer-less await here parks the calling task permanently and silently"
                )
            });
            let error = outcome.expect_err("a stalled actor cannot answer a probe");
            assert!(
                error.to_string().contains("probe") && error.to_string().contains("never answered"),
                "attempt {attempt}: the failure must NAME the call and the leg, got {error}"
            );
        }
    }
}
