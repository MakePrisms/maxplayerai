//! A local relay that behaves like relay.maxplayer.ai (buzz with `BUZZ_OPEN_READ`) for NIP-42.
//!
//! - It sends an AUTH challenge shortly after the socket opens (`challenge_delay` stands in for the
//!   network round trip, so a client that subscribes as soon as it is connected really does send
//!   its REQ before it has signed in).
//! - A REQ sent before sign-in gets stored history and EOSE, then NO live events, ever, even after
//!   sign-in succeeds, unless the client sends that REQ again. This is the silent failure: the
//!   client sees a healthy EOSE and then nothing.
//! - An EVENT sent before sign-in is refused with `auth-required:`.
//! - `gate: false` models a relay that challenges but does not require sign-in.
//! - `events_while_deaf` counts EVENTs a client published while its own connection still had a
//!   pre-sign-in subscription, i.e. before re-sending its REQ (the order the connector relies on).
//!
//! Events are stored and fanned out by a real `nostr-relay-builder` relay behind this front.
//! Shared by path from the maxplayer-core, maxplayer-mint and maxplayer-trade tests.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::{Event, Kind, PublicKey};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Copy, Debug)]
pub struct AuthRelayOptions {
    /// Send a NIP-42 challenge and gate reads/writes on it. `false` is an open relay (ditto).
    pub challenge: bool,
    /// Answer every AUTH with `OK false`.
    pub refuse_auth: bool,
    /// Delay between the socket opening and the AUTH challenge.
    pub challenge_delay: Duration,
    /// Require sign-in: refuse EVENT and withhold live events from pre-sign-in REQs. `false` is a
    /// relay that challenges but serves everyone anyway (sign-in is optional there).
    pub gate: bool,
    /// Membership: accept a sign-in only from these keys (buzz with
    /// `BUZZ_REQUIRE_RELAY_MEMBERSHIP`; repeat a key for one member). `None` accepts any sign-in.
    pub members: Option<[PublicKey; 2]>,
    /// Refuse a sign-in the way buzz does (issue #1228): `OK false` with `restricted: not a relay
    /// member`, then close the socket. `false` keeps the refusal text above and the socket open.
    pub buzz_refusal: bool,
}

impl Default for AuthRelayOptions {
    fn default() -> Self {
        Self {
            challenge: true,
            refuse_auth: false,
            challenge_delay: Duration::from_millis(150),
            gate: true,
            members: None,
            buzz_refusal: false,
        }
    }
}

#[derive(Default, Debug)]
pub struct Counters {
    pub connections: AtomicUsize,
    pub auths: AtomicUsize,
    pub pre_auth_reqs: AtomicUsize,
    pub post_auth_reqs: AtomicUsize,
    /// Live events withheld from a subscription that was opened before sign-in.
    pub dropped_live: AtomicUsize,
    /// EVENTs forwarded from a client while that same connection still had a pre-sign-in
    /// (deaf) subscription: a publish that went out before its reply subscription was re-sent.
    pub events_while_deaf: AtomicUsize,
    /// Subscriptions open right now that were sent before sign-in and not re-sent since: they
    /// get no live events. A test barrier ("everyone can hear") is `deaf == 0` after sign-in.
    pub deaf: AtomicUsize,
    /// Per connection: the kind of its first EVENT and the time since its first REQ on the same
    /// socket. Relative and intra-socket, so connect and mint latency don't enter it (#1156 N2).
    pub req_to_first_event: std::sync::Mutex<Vec<(u64, Duration)>>,
}

/// Per-connection pre-sign-in subscriptions (id -> EOSE passed), mirrored into `Counters::deaf`
/// and released when the connection ends.
struct PreAuth {
    subs: HashMap<String, bool>,
    counters: Arc<Counters>,
}

impl PreAuth {
    fn insert(&mut self, id: String) {
        if self.subs.insert(id, false).is_none() {
            self.counters.deaf.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn remove(&mut self, id: &str) {
        if self.subs.remove(id).is_some() {
            self.counters.deaf.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for PreAuth {
    fn drop(&mut self) {
        self.counters
            .deaf
            .fetch_sub(self.subs.len(), Ordering::SeqCst);
    }
}

impl Counters {
    pub fn get(value: &AtomicUsize) -> usize {
        value.load(Ordering::SeqCst)
    }
}

pub struct AuthRelay {
    pub url: String,
    pub counters: Arc<Counters>,
    kill: watch::Sender<u64>,
    task: tokio::task::JoinHandle<()>,
    _backend: LocalRelay,
}

impl AuthRelay {
    pub async fn start(options: AuthRelayOptions) -> Self {
        let backend = LocalRelay::new(RelayBuilder::default());
        backend.run().await.expect("backend relay");
        let backend_url = backend.url().await.to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let counters = Arc::new(Counters::default());
        let (kill, kill_rx) = watch::channel(0u64);
        let shared = counters.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(connection(
                    socket,
                    backend_url.clone(),
                    options,
                    shared.clone(),
                    kill_rx.clone(),
                ));
            }
        });
        Self {
            url,
            counters,
            kill,
            task,
            _backend: backend,
        }
    }

    /// Close every open client socket (the relay keeps listening, so clients reconnect).
    pub fn drop_connections(&self) {
        self.kill.send_modify(|generation| *generation += 1);
    }
}

impl Drop for AuthRelay {
    fn drop(&mut self) {
        self.task.abort();
        self.drop_connections();
    }
}

async fn connection(
    socket: TcpStream,
    backend_url: String,
    options: AuthRelayOptions,
    counters: Arc<Counters>,
    mut kill: watch::Receiver<u64>,
) {
    kill.borrow_and_update();
    let Ok(mut front) = tokio_tungstenite::accept_async(socket).await else {
        return;
    };
    let Ok((mut back, _)) = tokio_tungstenite::connect_async(backend_url).await else {
        return;
    };
    let n = counters.connections.fetch_add(1, Ordering::SeqCst) + 1;
    let challenge = format!("auth-gated-relay-{n}");
    let mut authenticated = !options.challenge;
    let challenge_at = tokio::time::sleep(options.challenge_delay);
    tokio::pin!(challenge_at);
    let mut challenged = !options.challenge;
    // Subscriptions opened before sign-in -> whether their EOSE has passed.
    let mut first_req: Option<std::time::Instant> = None;
    let mut first_event_seen = false;
    let mut pre_auth = PreAuth {
        subs: HashMap::new(),
        counters: counters.clone(),
    };
    loop {
        tokio::select! {
            _ = kill.changed() => return,
            _ = &mut challenge_at, if !challenged => {
                challenged = true;
                if front.send(text(json!(["AUTH", challenge]))).await.is_err() {
                    return;
                }
            }
            frame = front.next() => {
                let Some(Ok(frame)) = frame else { return };
                let Some(v) = parse(&frame) else { continue };
                match v[0].as_str() {
                    Some("REQ") => {
                        first_req.get_or_insert_with(std::time::Instant::now);
                    }
                    Some("EVENT") if !first_event_seen => {
                        first_event_seen = true;
                        if let Some(at) = first_req {
                            let kind = v[1]["kind"].as_u64().unwrap_or_default();
                            counters
                                .req_to_first_event
                                .lock()
                                .unwrap()
                                .push((kind, at.elapsed()));
                        }
                    }
                    _ => {}
                }
                match v[0].as_str() {
                    Some("AUTH") => {
                        let ok = !options.refuse_auth
                            && serde_json::from_value::<Event>(v[1].clone()).is_ok_and(|e| {
                                e.verify().is_ok()
                                    && options.members.is_none_or(|m| m.contains(&e.pubkey))
                                    && e.kind == Kind::Authentication
                                    && e.tags
                                        .iter()
                                        .any(|t| t.as_slice() == ["challenge", challenge.as_str()])
                            });
                        if ok {
                            authenticated = true;
                            counters.auths.fetch_add(1, Ordering::SeqCst);
                        }
                        let reason = match (ok, options.buzz_refusal) {
                            (true, _) => "",
                            (false, true) => "restricted: not a relay member",
                            (false, false) => "restricted: auth rejected",
                        };
                        if front.send(text(json!(["OK", v[1]["id"], ok, reason]))).await.is_err() {
                            return;
                        }
                        if !ok && options.buzz_refusal {
                            let _ = front.close(None).await;
                            return;
                        }
                        continue;
                    }
                    Some("REQ") => {
                        let id = v[1].as_str().unwrap_or_default().to_owned();
                        if authenticated {
                            counters.post_auth_reqs.fetch_add(1, Ordering::SeqCst);
                            pre_auth.remove(&id);
                        } else {
                            counters.pre_auth_reqs.fetch_add(1, Ordering::SeqCst);
                            if options.gate {
                                pre_auth.insert(id);
                            }
                        }
                    }
                    Some("CLOSE") => {
                        pre_auth.remove(v[1].as_str().unwrap_or_default());
                    }
                    Some("EVENT") if authenticated && !pre_auth.subs.is_empty() => {
                        counters.events_while_deaf.fetch_add(1, Ordering::SeqCst);
                    }
                    Some("EVENT") if !authenticated && options.gate => {
                        let refusal = json!(["OK", v[1]["id"], false, "auth-required: not authenticated"]);
                        if front.send(text(refusal)).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    _ => {}
                }
                if back.send(frame).await.is_err() {
                    return;
                }
            }
            frame = back.next() => {
                let Some(Ok(frame)) = frame else { return };
                if let Some(v) = parse(&frame) {
                    match (v[0].as_str(), pre_auth.subs.get_mut(v[1].as_str().unwrap_or_default())) {
                        (Some("EOSE"), Some(eose)) => *eose = true,
                        (Some("EVENT"), Some(true)) => {
                            counters.dropped_live.fetch_add(1, Ordering::SeqCst);
                            continue;
                        }
                        _ => {}
                    }
                }
                if front.send(frame).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn parse(frame: &Message) -> Option<Value> {
    serde_json::from_str(frame.to_text().ok()?).ok()
}

fn text(value: Value) -> Message {
    Message::text(value.to_string())
}
