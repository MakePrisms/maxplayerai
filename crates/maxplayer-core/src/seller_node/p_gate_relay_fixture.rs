//! An in-process relay that reproduces maxplayer-relay's `#p`-gate wire behaviour, and records what the
//! client actually sent.
//!
//! The `nostr-relay-builder` fixture used elsewhere in this crate cannot express the failure in #189.
//! Its NIP-42 read gate answers an unauthenticated `REQ` with the `auth-required:` prefix
//! (`local/inner.rs:961-989`), which nostr-sdk classifies as `MarkAsClosed` — the subscription stays
//! in the registry and the post-auth `resubscribe()` restores it automatically. The bug under test is
//! precisely what happens when the relay says `restricted:` instead: nostr-sdk classifies that as
//! `Remove` (`relay/inner.rs:1028`), deletes the subscription, and nothing ever restores it. A
//! fixture that self-heals would report every ordering as green.
//!
//! So this speaks the deployed relay's rule directly: a `REQ` carrying a `#p` filter is refused with
//! `restricted:` unless the session has completed NIP-42 **and** the `#p` value is the authenticated
//! pubkey. That one rule covers both cases the teeth need to tell apart — the pre-auth race (right
//! `#p`, no auth yet) and a genuine gate violation (someone else's `#p`, fully authed).
//!
//! Every `REQ` is recorded with the session's auth state at the moment it arrived, which is the
//! property #189 is actually about: a REQ that reaches the relay before AUTH does.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// What the relay answered a `REQ` with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Served — the end-of-stored-events the client waits for.
    Eose,
    /// Refused, with the full reason string (prefix included).
    Closed(String),
}

/// One `REQ` as the relay saw it.
#[derive(Debug, Clone)]
pub(super) struct ReqRecord {
    pub subscription_id: String,
    /// Whether NIP-42 had completed on this socket when the `REQ` arrived. `false` here on a
    /// `#p`-pinned subscription IS the #189 bug — the client let a p-gated REQ out before auth.
    pub authenticated: bool,
    /// Whether any filter pinned `#p`.
    pub p_pinned: bool,
    /// How many filters rode this one `REQ`. The grouped offer REQ carries 2 (targeted + open-pool);
    /// the degraded targeted-only shape carries 1.
    pub filter_count: usize,
    /// Whether at least one filter carried NO `#p` — i.e. the un-pinned open-pool half is present.
    pub has_unpinned_filter: bool,
    pub verdict: Verdict,
}

/// One EVENT frame the client published, as the relay saw it. Recorded by KIND because "advertised"
/// is a claim about which kinds reached the wire — the pre-advertise gate (#357) is exactly a test of
/// what a publish path put here, so the fixture that used to reply OK and DROP the event now keeps it.
#[derive(Debug, Clone)]
pub(super) struct PublishedEvent {
    pub kind: u64,
}

/// A refusal the test has armed, spent on the next `REQ` for this subscription that carries an
/// un-pinned filter.
///
/// Matching only the un-pinned shape is load-bearing: a refusal armed for the open-pool half would
/// otherwise be spent on the targeted-only re-subscribe that immediately follows a rejection, and
/// the backoff tooth would be measuring the wrong REQ.
#[derive(Debug, Clone)]
struct ForcedClose {
    subscription_id: String,
    reason: String,
}

#[derive(Debug, Default)]
struct Controls {
    /// Refusals queued by the test, consumed one per matching `REQ`.
    forced: Mutex<VecDeque<ForcedClose>>,
    /// Writer for the most recently accepted socket, so a test can push an UNSOLICITED `CLOSED` —
    /// which is what the deployed relay does, and what neither a REQ nor a policy hook can model.
    live: Mutex<Option<Arc<Mutex<Writer>>>>,
    /// NIP-42 auth GENERATION (#429). Bumped by [`PGateRelay::roll_challenge`] to model a live-socket
    /// auth re-challenge: a socket authenticated under an OLDER generation is now behind the current
    /// epoch, so its `#p`-pinned REQs read STALE (closed `auth-required:`) until it answers the new
    /// challenge and catches up — which is the in-place re-auth the fix re-issues its subs on.
    auth_generation: std::sync::atomic::AtomicU64,
}

/// A running fixture relay. Dropping it stops accepting new connections.
pub(super) struct PGateRelay {
    url: String,
    transcript: Arc<Mutex<Vec<ReqRecord>>>,
    /// Every EVENT the client published, in arrival order — the wire record the pre-advertise gate is
    /// asserted against.
    events: Arc<Mutex<Vec<PublishedEvent>>>,
    controls: Arc<Controls>,
    /// Sockets accepted so far. A reconnect is the only thing that increments this, which makes
    /// "no reconnect was required" an observable rather than an inference.
    connections: Arc<std::sync::atomic::AtomicUsize>,
    _accept: tokio::task::JoinHandle<()>,
}

impl PGateRelay {
    /// Bind on an ephemeral port and start serving.
    ///
    /// `auth_delay` is how long the relay withholds its NIP-42 challenge after a socket opens. The
    /// deployed relay challenges immediately, but a client that fires REQs on socket-up loses the
    /// race regardless of how fast the challenge is; holding it open makes that deterministic
    /// instead of timing-dependent, so the tooth cannot pass by being lucky.
    pub(super) async fn start(auth_delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture relay");
        let addr: SocketAddr = listener.local_addr().expect("fixture relay addr");
        let transcript: Arc<Mutex<Vec<ReqRecord>>> = Arc::new(Mutex::new(Vec::new()));
        let events: Arc<Mutex<Vec<PublishedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let controls: Arc<Controls> = Arc::new(Controls::default());

        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept = tokio::spawn({
            let transcript = Arc::clone(&transcript);
            let events = Arc::clone(&events);
            let controls = Arc::clone(&controls);
            let connections = Arc::clone(&connections);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let transcript = Arc::clone(&transcript);
                    let events = Arc::clone(&events);
                    let controls = Arc::clone(&controls);
                    tokio::spawn(async move {
                        let _ =
                            serve_connection(stream, auth_delay, transcript, events, controls).await;
                    });
                }
            }
        });

        Self {
            url: format!("ws://{addr}"),
            transcript,
            events,
            controls,
            connections,
            _accept: accept,
        }
    }

    /// How many sockets this relay has accepted.
    pub(super) fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Push an UNSOLICITED `CLOSED` for `subscription_id` down the live socket — the deployed relay's
    /// behaviour when it drops a subscription out from under a client that is otherwise healthy.
    pub(super) async fn close_now(&self, subscription_id: &str, reason: &str) {
        let live = self.controls.live.lock().await.clone();
        if let Some(writer) = live {
            let _ = send(&writer, json!(["CLOSED", subscription_id, reason])).await;
        }
    }

    /// Model a NIP-42 auth RE-CHALLENGE on a LIVE socket (#429): bump the relay's auth generation (so
    /// every open socket's last auth is now behind the current epoch), OPTIONALLY push an unsolicited
    /// `auth-required: not authenticated` CLOSED for each id in `close_subs` — the frame the field
    /// showed, with the socket STAYING UP and no reconnect — then re-issue an `AUTH` challenge so the
    /// client re-authenticates IN PLACE. The client catching up (answering the challenge) is the
    /// incidental completed-auth the fix keys its resubscribe off.
    ///
    /// `close_subs` is EMPTY for the deterministic red-prove and the money legs for the confirmation
    /// tooth — see [`long_lived_subs_resubscribe_on_auth_after_a_challenge_roll`] for why a faithful
    /// CLOSE makes the red-prove vacuous (nostr-sdk's own post-auth `resubscribe()` re-sends a
    /// `closed==true` sub on the re-auth and self-heals it), so the non-vacuous prover withholds the
    /// CLOSE and lets the STALE generation alone deafen the leg.
    pub(super) async fn roll_challenge(&self, close_subs: &[&str]) {
        let generation = self
            .controls
            .auth_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let live = self.controls.live.lock().await.clone();
        if let Some(writer) = live {
            for id in close_subs {
                let _ = send(
                    &writer,
                    json!(["CLOSED", id, "auth-required: not authenticated"]),
                )
                .await;
            }
            let _ =
                send(&writer, json!(["AUTH", format!("maxplayer-rechallenge-{generation}")])).await;
        }
    }

    pub(super) fn url(&self) -> String {
        self.url.clone()
    }

    /// Refuse the next `count` `REQ`s for `subscription_id` that carry an un-pinned filter — i.e. a
    /// relay that keeps rejecting the open-pool half while serving the targeted one.
    pub(super) async fn refuse_unpinned(&self, subscription_id: &str, count: usize, reason: &str) {
        let mut forced = self.controls.forced.lock().await;
        for _ in 0..count {
            forced.push_back(ForcedClose {
                subscription_id: subscription_id.to_string(),
                reason: reason.to_string(),
            });
        }
    }

    /// Every `REQ` the relay has seen, in arrival order.
    pub(super) async fn reqs(&self) -> Vec<ReqRecord> {
        self.transcript.lock().await.clone()
    }

    /// Every `REQ` seen for one subscription id.
    pub(super) async fn reqs_for(&self, subscription_id: &str) -> Vec<ReqRecord> {
        self.reqs()
            .await
            .into_iter()
            .filter(|record| record.subscription_id == subscription_id)
            .collect()
    }

    /// Every EVENT the relay has received, in arrival order.
    pub(super) async fn events(&self) -> Vec<PublishedEvent> {
        self.events.lock().await.clone()
    }

    /// How many EVENTs of `kind` the relay received — what a publish path actually put on the wire.
    pub(super) async fn event_kind_count(&self, kind: u64) -> usize {
        self.events()
            .await
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }

    /// Wait until `predicate` holds over the transcript, or give up. Returns whether it held —
    /// the caller asserts, so a timeout reads as the failure it is rather than a hang.
    pub(super) async fn wait_until<F>(&self, timeout: Duration, predicate: F) -> bool
    where
        F: Fn(&[ReqRecord]) -> bool,
    {
        tokio::time::timeout(timeout, async {
            loop {
                if predicate(&self.reqs().await) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .is_ok()
    }
}

/// Serve one client socket for its whole life.
async fn serve_connection(
    stream: tokio::net::TcpStream,
    auth_delay: Duration,
    transcript: Arc<Mutex<Vec<ReqRecord>>>,
    events: Arc<Mutex<Vec<PublishedEvent>>>,
    controls: Arc<Controls>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (writer, mut reader) = ws.split();
    let writer = Arc::new(Mutex::new(writer));
    *controls.live.lock().await = Some(Arc::clone(&writer));

    // The auth GENERATION this socket is currently authenticated under (#429). A `roll_challenge`
    // bumps the relay's generation and re-challenges auth on the SAME socket (no reconnect); until
    // the client answers the new challenge this value stays behind, and a `#p`-pinned REQ arriving
    // meanwhile reads STALE. It is advanced on every completed AUTH below — that live-socket re-auth
    // is exactly what the fix keys its resubscribe off.
    let mut authed_gen = controls
        .auth_generation
        .load(std::sync::atomic::Ordering::SeqCst);

    // The boot challenge goes out on its own task so the delay never blocks reading: the REQs we are
    // here to observe arrive DURING that window.
    let boot_challenge = "maxplayer-recoveryfix-challenge";
    tokio::spawn({
        let writer = Arc::clone(&writer);
        async move {
            tokio::time::sleep(auth_delay).await;
            let _ = send(&writer, json!(["AUTH", boot_challenge])).await;
        }
    });

    // Per-session NIP-42 state. `None` until the client answers a challenge.
    let mut authed_pubkey: Option<String> = None;

    while let Some(message) = reader.next().await {
        let message = message?;
        let text = match message {
            tokio_tungstenite::tungstenite::Message::Text(text) => text,
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };
        let Ok(frame) = serde_json::from_str::<Vec<Value>>(&text) else {
            continue;
        };
        match frame.first().and_then(Value::as_str) {
            Some("AUTH") => {
                let Some(event) = frame.get(1) else { continue };
                // Kind 22242 is the NIP-42 auth event. Checking it keeps the fixture honest: a client
                // that authenticated with something else has not authenticated.
                if event.get("kind").and_then(Value::as_u64) == Some(22242) {
                    authed_pubkey = event
                        .get("pubkey")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    // A completed auth binds this socket to the CURRENT generation — a live-socket
                    // re-auth (the client answering a re-challenge without reconnecting) catches the
                    // socket up, which is the exact moment the fix re-issues the subs.
                    authed_gen = controls
                        .auth_generation
                        .load(std::sync::atomic::Ordering::SeqCst);
                }
                let id = event.get("id").and_then(Value::as_str).unwrap_or_default();
                send(&writer, json!(["OK", id, authed_pubkey.is_some(), ""])).await?;
            }
            Some("EVENT") => {
                let Some(event) = frame.get(1) else { continue };
                let id = event.get("id").and_then(Value::as_str).unwrap_or_default();
                // Record what was published, by kind, BEFORE the OK: what reaches the wire is exactly
                // the property the pre-advertise gate is asserted against (#357).
                if let Some(kind) = event.get("kind").and_then(Value::as_u64) {
                    events.lock().await.push(PublishedEvent { kind });
                }
                send(&writer, json!(["OK", id, true, ""])).await?;
            }
            Some("REQ") => {
                let Some(subscription_id) =
                    frame.get(1).and_then(Value::as_str).map(str::to_string)
                else {
                    continue;
                };
                let filters: Vec<&Value> = frame.iter().skip(2).collect();
                let pinned: Vec<Option<&str>> = filters
                    .iter()
                    .map(|filter| {
                        filter
                            .get("#p")
                            .and_then(Value::as_array)
                            .and_then(|values| values.first())
                            .and_then(Value::as_str)
                    })
                    .collect();

                // A `roll_challenge` bumped the relay's generation past this socket's last completed
                // auth (#429): the socket authenticated, but under a now-superseded generation, so its
                // `#p`-pinned REQs no longer count as authenticated until it answers the re-challenge.
                let stale = authed_pubkey.is_some()
                    && authed_gen
                        < controls
                            .auth_generation
                            .load(std::sync::atomic::Ordering::SeqCst);

                let verdict = decide(
                    &subscription_id,
                    &pinned,
                    authed_pubkey.as_deref(),
                    stale,
                    &controls,
                )
                .await;

                transcript.lock().await.push(ReqRecord {
                    subscription_id: subscription_id.clone(),
                    // A REQ arriving under a stale generation is served neither authed nor at all —
                    // the record shows it UNAUTHENTICATED, which is how the test tells a leg re-issued
                    // on the caught-up (post-re-auth) socket from one that was never re-sent.
                    authenticated: authed_pubkey.is_some() && !stale,
                    p_pinned: pinned.iter().any(Option::is_some),
                    filter_count: filters.len(),
                    has_unpinned_filter: pinned.iter().any(Option::is_none),
                    verdict: verdict.clone(),
                });

                match verdict {
                    Verdict::Eose => {
                        send(&writer, json!(["EOSE", subscription_id])).await?;
                    }
                    Verdict::Closed(reason) => {
                        send(&writer, json!(["CLOSED", subscription_id, reason])).await?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The deployed relay's rule, and the whole reason this fixture exists.
///
/// A `#p`-pinned filter is refused with the PERMANENT-class `restricted:` prefix unless the session
/// is authenticated as that very pubkey. maxplayer-relay reaches this verdict by evaluating its p-gate
/// against an empty authed pubkey on an unauthenticated connection (`req.rs:208`), which is why the
/// pre-auth race and a genuine wrong-`#p` request are indistinguishable on the wire — the exact
/// ambiguity the client-side fix has to resolve without softening the taxonomy.
async fn decide(
    subscription_id: &str,
    pinned: &[Option<&str>],
    authed_pubkey: Option<&str>,
    stale: bool,
    controls: &Controls,
) -> Verdict {
    let has_unpinned = pinned.iter().any(Option::is_none);
    let mut forced = controls.forced.lock().await;
    if let Some(index) = forced
        .iter()
        .position(|entry| entry.subscription_id == subscription_id && has_unpinned)
    {
        let entry = forced.remove(index).expect("index just found");
        return Verdict::Closed(entry.reason);
    }
    drop(forced);

    // A `#p`-pinned REQ arriving under a STALE (re-challenged) generation is closed
    // `auth-required: not authenticated` — the OBSERVED wire contract on a live re-auth socket (field
    // log: all four subs closed EXACTLY this, never `restricted:`). This is a test double for the
    // relay's behaviour, NOT a claim about relay internals: mac's read of the deployed relay source
    // says it has NO generation concept, so `stale` here only stands in for "this socket's auth is
    // behind the current epoch". nostr-sdk treats `auth-required:` as retryable (MarkAsClosed — the
    // sub stays in the registry, `closed==true`), so the leg is repaired only by re-issuing on the
    // next completed AUTH. Distinct from the pre-auth race below, where an UNAUTHENTICATED session
    // (never authed) still gets the permanent-class `restricted:` (#189, unchanged).
    let p_gated = pinned.iter().any(Option::is_some);
    if stale && p_gated {
        return Verdict::Closed("auth-required: not authenticated".to_string());
    }

    for value in pinned.iter().flatten() {
        if authed_pubkey != Some(*value) {
            return Verdict::Closed(
                "restricted: p-gated events require #p matching your pubkey".to_string(),
            );
        }
    }
    Verdict::Eose
}

type Writer = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio_tungstenite::tungstenite::Message,
>;

async fn send(
    writer: &Arc<Mutex<Writer>>,
    value: Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    writer
        .lock()
        .await
        .send(tokio_tungstenite::tungstenite::Message::Text(
            value.to_string().into(),
        ))
        .await?;
    Ok(())
}
