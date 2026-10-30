//! Shared NIP-42 relay-auth handshake, neutral to any single consumer.
//!
//! maxplayer-relay requires NIP-42 AUTH for the p-gated kind-1059 subscribe AND for all writes, and the
//! handshake shape is identical on the seller receive path and the buyer receipt-publish path. This
//! module owns the one `wait_for_nip42_auth` both use, so neither depends on the other's error type
//! or lifecycle. Callers map the outcome to their own gate: the seller degrades on `NoChallenge`,
//! the buyer fails closed on anything but `Authenticated`.

use std::time::Duration;

/// Outcome of [`wait_for_nip42_auth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthWait {
    /// The relay issued a NIP-42 challenge and `automatic_authentication` completed it.
    Authenticated,
    /// The relay issued NO challenge within the window. NOT a failure (see the fn doc).
    NoChallenge,
}

/// A fatal NIP-42 handshake failure (active rejection, relay shutdown, or channel closed). Distinct
/// from `NoChallenge`, which is non-fatal. Neutral so no consumer's error type leaks here; callers
/// map it into their own (`DaemonError`, the buyer gate, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAuthError(pub String);

impl std::fmt::Display for RelayAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for RelayAuthError {}

/// Drain `notifications` until the relay's NIP-42 AUTH completes, the relay actively rejects auth
/// (fatal), or the window elapses with no challenge (`NoChallenge`, non-fatal).
///
/// Caller must subscribe `relay.notifications()` **before** `connect` so the `Authenticated` event
/// cannot be missed.
///
/// maxplayer-relay p-gates kind-1059: unauthenticated `REQ kinds:[1059] #p:self` is `CLOSED` with
/// `restricted:` (not `auth-required:`). nostr-sdk 0.44 treats `restricted:` as `Remove` — the sub
/// is dropped, so a post-auth `resubscribe()` never restores it. Auth **before** the 1059 subscribe
/// is therefore load-bearing for seller receive, and maxplayer-relay challenges on connect so
/// `Authenticated` arrives in milliseconds.
///
/// A window with NO challenge is reported as `NoChallenge` rather than a fatal error: a relay that
/// challenges only lazily (on the first `REQ`/`EVENT`, e.g. the in-process test relay) will
/// challenge when the caller subscribes, and `automatic_authentication` completes auth then. The
/// caller logs the degrade loudly. An ACTIVE rejection (`AuthenticationFailed`) or a relay shutdown
/// stays fatal (fail-closed), unchanged.
pub async fn wait_for_nip42_auth(
    notifications: &mut tokio::sync::broadcast::Receiver<nostr_sdk::pool::RelayNotification>,
    timeout: Duration,
) -> Result<AuthWait, RelayAuthError> {
    use nostr_sdk::pool::RelayNotification;

    let mut sign_in = SignInState::default();
    let within_window = tokio::time::timeout(timeout, async {
        loop {
            let notification = notifications.recv().await;
            // #1228: buzz refuses an unregistered key and closes the socket, and nostr-sdk then
            // never emits `AuthenticationFailed` (observed: challenge, `OK false`, `Disconnected`).
            // Without this the wait ran out and reported `NoChallenge`, so a refused seat booted
            // "degraded" and kept advertising.
            if let Ok(notification) = &notification
                && matches!(notification, RelayNotification::RelayStatus { .. })
                && let Some(refusal) = sign_in.observe(notification)
            {
                return Err(RelayAuthError(crate::relay_refusal::with_hint(format!(
                    "NIP-42 sign-in refused: {refusal}"
                ))));
            } else if let Ok(notification) = &notification {
                sign_in.observe(notification);
            }
            match notification {
                Ok(RelayNotification::Authenticated) => return Ok(AuthWait::Authenticated),
                Ok(RelayNotification::AuthenticationFailed) => {
                    return Err(RelayAuthError(crate::relay_refusal::with_hint(
                        "NIP-42 authentication failed (required for kind-1059 p-gated receive)",
                    )));
                }
                Ok(RelayNotification::Shutdown) => {
                    return Err(RelayAuthError(
                        "relay shutdown before NIP-42 authentication".into(),
                    ));
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(RelayAuthError(
                        "relay notification channel closed before NIP-42 authentication".into(),
                    ));
                }
            }
        }
    })
    .await;

    // Elapsed with no challenge → NoChallenge (non-fatal). Within the window → the loop's result
    // (Authenticated, or a fatal active failure).
    within_window.unwrap_or(Ok(AuthWait::NoChallenge))
}

/// Re-send every long-lived subscription on `relay` each time the relay completes NIP-42 sign-in.
///
/// A sign-in-required relay (relay.maxplayer.ai, buzz with open reads) answers a REQ sent before
/// sign-in with stored history and EOSE, and never delivers live events to it. nostr-sdk 0.44
/// re-sends a subscription after sign-in only if the relay CLOSED it or it predates the current
/// socket, so one that got EOSE on this socket stays deaf forever. Ephemeral kinds (the mint's
/// 23410/23411, trade's 23412) are never stored, so such a subscription receives nothing at all.
///
/// Subscribing right away and re-sending the same REQ (same id, which replaces it on the relay)
/// once signed in keeps relays that never ask for sign-in exactly as fast as before, and covers
/// every reconnect: the relay challenges again on each new socket. Call this BEFORE
/// `Client::connect` so the first `Authenticated` is not missed. A failed sign-in is logged as an
/// auth error under `label`.
///
/// The task ends on `Relay::disconnect()`/`Client::disconnect()` as well as on shutdown (both
/// send `Shutdown`): call it again after reconnecting a disconnected client, and for every relay
/// added later. Subscriptions are re-sent with their original filters (including `since`), so it
/// is meant for ephemeral or low-volume filters. Use auto-closing subscriptions for one-shot
/// queries: they are not re-sent. Not safe against a concurrent `unsubscribe` of a long-lived
/// subscription: its CLOSE can go out before the re-sent REQ and leave an orphan on the relay.
pub fn resubscribe_on_auth(
    relay: nostr_sdk::prelude::Relay,
    label: &'static str,
) -> tokio::task::JoinHandle<()> {
    use nostr_sdk::pool::RelayNotification;
    use nostr_sdk::prelude::ClientMessage;
    use tokio::sync::broadcast::error::RecvError;

    let mut notifications = relay.notifications();
    tokio::spawn(async move {
        loop {
            match notifications.recv().await {
                // A lagged receiver may have missed an `Authenticated`: re-sending is harmless.
                Ok(RelayNotification::Authenticated) | Err(RecvError::Lagged(_)) => {
                    for (id, filters) in relay.subscriptions().await {
                        // Skip one that was closed since the snapshot: re-sending it would
                        // leave a subscription on the relay that the client no longer tracks.
                        if filters.is_empty() || relay.subscription(&id).await.is_none() {
                            continue;
                        }
                        let _ = relay.send_msg(ClientMessage::req(id, filters));
                    }
                }
                Ok(RelayNotification::AuthenticationFailed) => eprintln!(
                    "{label}: relay {}: NIP-42 sign-in failed (auth error); no live events from \
                     this relay until sign-in succeeds ({})",
                    relay.url(),
                    // The mint server signs with its own key, so it gets the mint wording.
                    if label.starts_with("maxplayer-mint") {
                        crate::relay_refusal::MINT_LINK_HINT
                    } else {
                        crate::relay_refusal::LINK_HINT
                    }
                ),
                Ok(RelayNotification::Shutdown) | Err(RecvError::Closed) => return,
                Ok(_) => {}
            }
        }
    })
}

/// Short-lived signed reads/writes: register notifications before connecting, authenticate
/// when challenged, and leave lazy-auth relays to the SDK's auth-required retry path.
pub async fn connect_authenticated(
    keys: &nostr_sdk::Keys,
    url: &str,
) -> Result<nostr_sdk::Client, RelayAuthError> {
    let client = nostr_sdk::Client::new(keys.clone());
    client.automatic_authentication(true);
    client
        .add_relay(url)
        .await
        .map_err(|_| RelayAuthError("could not add relay".into()))?;
    let relay = client
        .relays()
        .await
        .into_values()
        .next()
        .ok_or_else(|| RelayAuthError("relay missing after add".into()))?;
    let mut notifications = relay.notifications();
    client.connect().await;
    if let Err(error) = wait_for_nip42_auth(&mut notifications, Duration::from_secs(3)).await {
        client.disconnect().await;
        return Err(error);
    }
    Ok(client)
}

/// The relay's answer to this connection's most recent refused NIP-42 sign-in (issue #1228).
///
/// nostr-sdk reports a refused sign-in only as `RelayNotification::AuthenticationFailed`, and a
/// write that was waiting for the sign-in then fails with a bare `timeout`. The relay's refusal
/// text (`restricted: not a relay member` from buzz) did arrive, as the `OK false` answer to the
/// AUTH event. This keeps that text so a write's failure can say why
/// ([`crate::relay_refusal::annotate_refused_sign_in`]).
///
/// buzz closes the socket right after refusing, and the SDK's `AuthenticationFailed` is sent by a
/// task that the close can cancel first, so it may never come. A socket that was challenged, got
/// an `OK false`, and closed before signing in is therefore read as a refused sign-in too.
///
/// Cleared by the next successful sign-in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignInState {
    /// The relay sent a challenge on this socket and the sign-in has not succeeded.
    challenged: bool,
    /// The last `OK false` text seen on this socket since the challenge.
    last_ok_false: Option<String>,
    refusal: Option<String>,
}

impl SignInState {
    /// Feed one relay notification. Returns the refusal when this notification ended a sign-in
    /// as refused (so a caller can log it once per refusal).
    pub fn observe(
        &mut self,
        notification: &nostr_sdk::pool::RelayNotification,
    ) -> Option<&str> {
        use nostr_sdk::RelayMessage;
        use nostr_sdk::pool::RelayNotification;
        match notification {
            RelayNotification::Message {
                message: RelayMessage::Auth { .. },
            } => {
                self.challenged = true;
                self.last_ok_false = None;
                None
            }
            RelayNotification::Message {
                message:
                    RelayMessage::Ok {
                        status: false,
                        message,
                        ..
                    },
            } => {
                // buzz's answer to an EVENT sent before sign-in. Never an answer to an AUTH, so it
                // must not stand in for the sign-in refusal.
                if self.challenged && !message.starts_with("auth-required: not authenticated") {
                    self.last_ok_false = Some(message.to_string());
                }
                None
            }
            RelayNotification::AuthenticationFailed => {
                let text = self
                    .last_ok_false
                    .take()
                    .filter(|text| !text.is_empty())
                    // No text reached us: the SDK's own words.
                    .unwrap_or_else(|| "authentication failed".to_owned());
                self.challenged = false;
                self.refusal = Some(text);
                self.refusal.as_deref()
            }
            RelayNotification::Authenticated => {
                *self = Self::default();
                None
            }
            RelayNotification::RelayStatus { status }
                if *status != nostr_sdk::prelude::RelayStatus::Connected =>
            {
                // Challenged, answered `OK false`, then closed without signing in: buzz's refusal.
                let refused = self.challenged.then(|| self.last_ok_false.take()).flatten();
                self.challenged = false;
                self.last_ok_false = None;
                match refused.filter(|text| !text.is_empty()) {
                    Some(text) => {
                        self.refusal = Some(text);
                        self.refusal.as_deref()
                    }
                    None => None,
                }
            }
            _ => None,
        }
    }

    /// The relay's refusal of the most recent sign-in, if it was refused and none succeeded since.
    pub fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }
}

/// [`SignInState`] for one relay, fed from that relay's own notification stream on demand.
///
/// Create it BEFORE `connect` (like [`wait_for_nip42_auth`]'s receiver) so the refusal on the
/// first socket is not missed. Reading drains what has arrived since the last read, so the answer
/// is current the moment a send returns: the SDK broadcasts `AuthenticationFailed` to every
/// receiver at once, so a send that ended on it has already put it in this receiver's queue.
pub struct SignInWatch {
    notifications: std::sync::Mutex<
        tokio::sync::broadcast::Receiver<nostr_sdk::pool::RelayNotification>,
    >,
    state: std::sync::Mutex<SignInState>,
    /// Refused periods seen so far (a successful sign-in ends one), so [`Self::track`] logs each
    /// once whoever drained it.
    refused: std::sync::atomic::AtomicU64,
}

impl SignInWatch {
    pub fn new(relay: &nostr_sdk::prelude::Relay) -> Self {
        Self {
            notifications: std::sync::Mutex::new(relay.notifications()),
            state: std::sync::Mutex::new(SignInState::default()),
            refused: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A watch on `url` as registered in `client`, or `None` when it is not registered (the send
    /// then fails on its own with a clearer error than this could give).
    pub async fn for_client(client: &nostr_sdk::Client, url: &str) -> Option<Self> {
        let relay = client.relay(url).await.ok()?;
        Some(Self::new(&relay))
    }

    /// For a long-lived client: also keep the state current in the background, so a refusal is
    /// not lost to a lagged receiver between two writes. `on_refused` is called once when the
    /// sign-in starts being refused, and again only after a sign-in succeeded in between. Call
    /// before `connect`. The task ends with the relay.
    pub fn track(
        self: &std::sync::Arc<Self>,
        relay: &nostr_sdk::prelude::Relay,
        on_refused: impl Fn(&str) + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        use nostr_sdk::pool::RelayNotification;
        use tokio::sync::broadcast::error::RecvError;
        use std::sync::atomic::Ordering;
        let mut wake = relay.notifications();
        let watch = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let mut logged = 0;
            loop {
                let ended = matches!(
                    wake.recv().await,
                    Ok(RelayNotification::Shutdown) | Err(RecvError::Closed)
                );
                watch.drain();
                let seen = watch.refused.load(Ordering::SeqCst);
                if seen > logged {
                    logged = seen;
                    if let Some(refusal) = watch.current() {
                        on_refused(&refusal);
                    }
                }
                if ended {
                    return;
                }
            }
        })
    }

    /// Feed every notification that has arrived since the last read.
    fn drain(&self) {
        use tokio::sync::broadcast::error::TryRecvError;
        let mut notifications = self
            .notifications
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
        loop {
            match notifications.try_recv() {
                Ok(notification) => {
                    // Counted once per refused period: the SDK reconnects a refused socket every
                    // few seconds, and each attempt is refused again with the same text.
                    let was_refused = state.refusal().is_some();
                    if state.observe(&notification).is_some() && !was_refused {
                        self.refused
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                // Dropped notifications are older than the ones still queued: keep reading.
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
    }

    fn current(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .refusal()
            .map(str::to_owned)
    }

    /// The relay's refusal of this connection's most recent sign-in, if it was refused and has
    /// not succeeded since.
    pub fn refusal(&self) -> Option<String> {
        self.drain();
        self.current()
    }

    /// `reason` with the sign-in refusal named, when there was one (see
    /// [`crate::relay_refusal::annotate_refused_sign_in`]).
    pub fn annotate(&self, reason: &str) -> String {
        crate::relay_refusal::annotate_refused_sign_in(reason, self.refusal().as_deref())
    }
}

/// `output.failed` as `url: reason; …`, each reason naming a refused sign-in when `sign_in` saw
/// one (issue #1228). Pass the result through [`crate::relay_refusal::with_hint`].
pub fn failed_reasons<T: std::fmt::Debug>(
    output: &nostr_sdk::prelude::Output<T>,
    sign_in: Option<&SignInWatch>,
) -> String {
    output
        .failed
        .iter()
        .map(|(url, reason)| {
            let reason = match sign_in {
                Some(sign_in) => sign_in.annotate(reason),
                None => reason.clone(),
            };
            format!("{url}: {reason}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod sign_in_tests {
    use super::*;
    use nostr_sdk::RelayMessage;
    use nostr_sdk::pool::RelayNotification;
    use nostr_sdk::prelude::EventId;
    use std::borrow::Cow;

    fn ok_false(text: &'static str) -> RelayNotification {
        RelayNotification::Message {
            message: RelayMessage::Ok {
                event_id: EventId::all_zeros(),
                status: false,
                message: Cow::Borrowed(text),
            },
        }
    }

    fn challenge() -> RelayNotification {
        RelayNotification::Message {
            message: RelayMessage::Auth {
                challenge: Cow::Borrowed("c"),
            },
        }
    }

    fn disconnected() -> RelayNotification {
        RelayNotification::RelayStatus {
            status: nostr_sdk::prelude::RelayStatus::Disconnected,
        }
    }

    #[test]
    fn a_refused_sign_in_keeps_the_relay_text_until_one_succeeds() {
        let mut state = SignInState::default();
        state.observe(&challenge());
        assert_eq!(state.observe(&ok_false("restricted: not a relay member")), None);
        // An EVENT refusal for the unauthenticated session in between is not the AUTH answer.
        assert_eq!(state.observe(&ok_false("auth-required: not authenticated")), None);
        assert_eq!(
            state.observe(&RelayNotification::AuthenticationFailed),
            Some("restricted: not a relay member")
        );
        assert_eq!(state.refusal(), Some("restricted: not a relay member"));
        // The reconnect is challenged and signs in: the refusal is over.
        state.observe(&disconnected());
        state.observe(&challenge());
        state.observe(&RelayNotification::Authenticated);
        assert_eq!(state.refusal(), None);
    }

    /// buzz closes the socket right after the refusal, which can cancel the SDK's
    /// `AuthenticationFailed`: challenge, `OK false`, close is the same refusal.
    #[test]
    fn a_socket_closed_after_an_unanswered_challenge_and_ok_false_is_a_refusal() {
        let mut state = SignInState::default();
        state.observe(&challenge());
        state.observe(&ok_false("restricted: not a relay member"));
        assert_eq!(
            state.observe(&disconnected()),
            Some("restricted: not a relay member")
        );
        assert_eq!(state.refusal(), Some("restricted: not a relay member"));
    }

    #[test]
    fn a_plain_disconnect_or_unchallenged_refusal_is_not_a_sign_in_refusal() {
        let mut state = SignInState::default();
        // No challenge: an `OK false` is about an event, not a sign-in.
        state.observe(&ok_false("invalid: stale event"));
        assert_eq!(state.observe(&disconnected()), None);
        // Challenged, then the socket dropped with no answer: a network failure, not a refusal.
        state.observe(&challenge());
        assert_eq!(state.observe(&disconnected()), None);
        assert_eq!(state.refusal(), None);
        // The SDK says it failed but no text arrived: its own words.
        state.observe(&challenge());
        assert_eq!(
            state.observe(&RelayNotification::AuthenticationFailed),
            Some("authentication failed")
        );
    }
}
