//! The relay listener: kind-23410 requests in, kind-23411 replies out (spec §3.1).
//!
//! Every check that can refuse a request runs BEFORE the mint is called, so the named refusals
//! (`bad_request`, `expired`, `rate_limited`, `unsupported`) keep their promise that nothing ran.
//! Events that can't be answered safely (forged, not for us, undecryptable, no request id) are
//! dropped without a reply.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cdk::Mint;
use cdk::util::unix_time;
use maxplayer_core::mint_wire::{
    ErrorBody, MAX_PLAINTEXT_BYTES, Outcome, PROTOCOL_VERSION, REQUEST_KIND, RESPONSE_KIND,
    Request, Response, code, request_expired,
};
use nostr_sdk::nips::nip44::{self, Version};
use nostr_sdk::prelude::{
    Client, Event, EventBuilder, Filter, Keys, Kind, RelayMessage, RelayPoolNotification, Tag,
    Timestamp,
};
use serde_json::Value;

use cdk::nuts::{SwapRequest, SwapResponse};
use maxplayer_core::mint_wire::op;

use crate::dispatch::{self, named};
use crate::replay::{self, Record, Signed};

/// Largest NIP-44 v2 payload a [`MAX_PLAINTEXT_BYTES`] plaintext encrypts to (padded to 65,536,
/// plus version, nonce and MAC, base64). Anything longer is dropped before decrypting.
pub const MAX_CONTENT_BYTES: usize = (1usize + 32 + 2 + 65_536 + 32).div_ceil(3) * 4;

/// A running listener.
pub struct Server {
    mint: Mint,
    keys: Keys,
    client: Client,
    limiter: RateLimiter,
}

impl Server {
    /// Connect to `relays` and subscribe to requests addressed to `keys`.
    pub async fn connect(
        mint: Mint,
        keys: Keys,
        relays: &[String],
        rate_limit: u32,
    ) -> Result<Self> {
        let client = Client::new(keys.clone());
        client.automatic_authentication(true);
        for relay in relays {
            client
                .add_relay(relay.as_str())
                .await
                .with_context(|| format!("add relay {relay}"))?;
        }
        client.connect().await;
        client.wait_for_connection(Duration::from_secs(10)).await;
        let filter = Filter::new()
            .kind(Kind::Custom(REQUEST_KIND))
            .pubkey(keys.public_key())
            .since(Timestamp::now() - Duration::from_secs(60));
        client.subscribe(filter, None).await.context("subscribe")?;
        Ok(Self {
            mint,
            keys,
            client,
            limiter: RateLimiter::new(rate_limit),
        })
    }

    /// Serve until the relay pool closes. Requests are handled one at a time, in arrival order.
    pub async fn serve(mut self) -> Result<()> {
        let mut notifications = self.client.notifications();
        loop {
            let notification = match notifications.recv().await {
                Ok(notification) => notification,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    eprintln!("maxplayer-mint: fell behind, {skipped} relay messages skipped");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            };
            // Raw deliveries, not the pool's `Event` notification: that one dedups by event id,
            // and the wallet's lost-reply re-send carries the SAME id and needs an answer.
            let RelayPoolNotification::Message {
                message: RelayMessage::Event { event, .. },
                ..
            } = notification
            else {
                continue;
            };
            let event: Event = event.into_owned();
            if let Some(response) = self.handle(&event).await {
                self.reply(&event, response).await;
            }
        }
    }

    async fn handle(&mut self, event: &Event) -> Option<Response> {
        if event.kind != Kind::Custom(REQUEST_KIND)
            || !event
                .tags
                .public_keys()
                .any(|pk| *pk == self.keys.public_key())
            || event.content.len() > MAX_CONTENT_BYTES
            || event.verify().is_err()
        {
            return None;
        }
        let plain = nip44::decrypt(self.keys.secret_key(), &event.pubkey, &event.content).ok()?;
        let request: Request = match serde_json::from_str(&plain) {
            Ok(request) => request,
            Err(error) => {
                // Answerable only if the plaintext still names a request id.
                let id = serde_json::from_str::<Value>(&plain)
                    .ok()?
                    .get("id")?
                    .as_str()?
                    .to_owned();
                return Some(refusal(id, code::BAD_REQUEST, format!("request: {error}")));
            }
        };
        if request.v != PROTOCOL_VERSION {
            return Some(refusal(
                request.id,
                code::BAD_REQUEST,
                format!("unsupported envelope version {}", request.v),
            ));
        }
        let id = request.id.clone();
        let outcome = self.answer(event, request).await;
        Some(Response {
            v: PROTOCOL_VERSION,
            id,
            outcome,
        })
    }

    /// Replay, reconcile or execute (spec §3.4, see [`crate::replay`]). The log is read BEFORE the
    /// expiry check and the rate limiter, so a recorded reply is replayed even after `exp` and is
    /// never turned into `rate_limited`.
    async fn answer(&mut self, event: &Event, request: Request) -> Outcome {
        let key = replay::key(&event.pubkey, &request.id);
        let digest = replay::digest(&request);
        let event_id = event.id.to_hex();
        let record = match replay::read(&self.mint, &key).await {
            Ok(record) => record,
            Err(error) => {
                eprintln!("maxplayer-mint: {error}");
                return failure(code::INTERNAL, "request log unavailable");
            }
        };
        match record {
            Some(record) if !record.matches(&event_id, &digest) => {
                eprintln!(
                    "maxplayer-mint: ALARM: request id {:?} from {} reused with different content; not executed",
                    request.id, event.pubkey
                );
                failure(code::INTERNAL, "request id reused with different content")
            }
            Some(Record::Completed { outcome, .. }) => outcome,
            Some(Record::Executing { .. }) => self.reconcile(&key, event_id, digest, request).await,
            None => {
                if request_expired(request.exp, unix_time()) {
                    return failure(code::EXPIRED, "request expired");
                }
                if !self.limiter.admit() {
                    return failure(code::RATE_LIMITED, "try again shortly");
                }
                if request.op != op::SWAP {
                    // Read-only (or refused as unsupported): a duplicate may simply run again.
                    return dispatch::execute(&self.mint, &request.op, request.body).await;
                }
                let executing = Record::Executing {
                    event_id: event_id.clone(),
                    digest: digest.clone(),
                };
                if let Err(error) = replay::write(&self.mint, &key, &executing).await {
                    eprintln!("maxplayer-mint: {error}");
                    return failure(code::INTERNAL, "request log unavailable");
                }
                let outcome = dispatch::execute(&self.mint, &request.op, request.body).await;
                self.finish(&key, event_id, digest, outcome).await
            }
        }
    }

    /// A swap whose first execution left no final record (crash, or an ambiguous outcome):
    /// answer from what the mint actually committed.
    async fn reconcile(
        &mut self,
        key: &str,
        event_id: String,
        digest: String,
        request: Request,
    ) -> Outcome {
        let swap: SwapRequest = match serde_json::from_value(request.body.clone()) {
            Ok(swap) => swap,
            Err(_) => return failure(code::INTERNAL, "unreadable executing record"),
        };
        match replay::signed_outputs(&self.mint, swap.outputs()).await {
            Err(error) => {
                eprintln!("maxplayer-mint: {error}");
                failure(code::INTERNAL, "cannot reconcile request")
            }
            Ok(Signed::All(signatures)) => {
                match serde_json::to_value(SwapResponse::new(signatures)) {
                    Ok(value) => self.finish(key, event_id, digest, Outcome::Ok(value)).await,
                    Err(error) => failure(code::INTERNAL, format!("encode: {error}")),
                }
            }
            Ok(Signed::Partial) => {
                eprintln!(
                    "maxplayer-mint: ALARM: swap {:?} is partially signed; not answered",
                    request.id
                );
                failure(code::INTERNAL, "swap partially signed")
            }
            Ok(Signed::None) => {
                // Nothing committed: the first attempt never took effect.
                if request_expired(request.exp, unix_time()) {
                    let expired = failure(code::EXPIRED, "request expired");
                    return self.finish(key, event_id, digest, expired).await;
                }
                if !self.limiter.admit() {
                    return failure(code::RATE_LIMITED, "try again shortly");
                }
                let outcome = dispatch::execute(&self.mint, &request.op, request.body).await;
                self.finish(key, event_id, digest, outcome).await
            }
        }
    }

    /// Record a final outcome so every later duplicate gets exactly this reply.
    async fn finish(
        &self,
        key: &str,
        event_id: String,
        digest: String,
        outcome: Outcome,
    ) -> Outcome {
        if replay::settled(&outcome) {
            let record = Record::Completed {
                event_id,
                digest,
                outcome: outcome.clone(),
            };
            if let Err(error) = replay::write(&self.mint, key, &record).await {
                // The reply is still right; a duplicate is reconciled from the executing record.
                eprintln!("maxplayer-mint: {error}");
            }
        }
        outcome
    }

    async fn reply(&self, to: &Event, response: Response) {
        let mut plain = serde_json::to_string(&response).unwrap_or_default();
        if plain.len() > MAX_PLAINTEXT_BYTES {
            // Can't happen within the mint's 128-output limit; if it does, say so honestly
            // (`internal` is ambiguous, so the wallet recovers instead of assuming nothing ran).
            let too_big = refusal(response.id, code::INTERNAL, "response over the size limit");
            plain = serde_json::to_string(&too_big).unwrap_or_default();
        }
        let Ok(content) = nip44::encrypt(self.keys.secret_key(), &to.pubkey, plain, Version::V2)
        else {
            return;
        };
        let Ok(event) = EventBuilder::new(Kind::Custom(RESPONSE_KIND), content)
            .tag(Tag::public_key(to.pubkey))
            .tag(Tag::event(to.id))
            .sign_with_keys(&self.keys)
        else {
            return;
        };
        if let Err(error) = self.client.send_event(&event).await {
            eprintln!("maxplayer-mint: reply {} not published: {error}", to.id);
        }
    }
}

fn failure(name: &str, detail: impl Into<String>) -> Outcome {
    Outcome::Err(named(name, detail))
}

fn refusal(id: String, name: &str, detail: impl Into<String>) -> Response {
    let ErrorBody { code, detail } = named(name, detail);
    Response {
        v: PROTOCOL_VERSION,
        id,
        outcome: Outcome::Err(ErrorBody { code, detail }),
    }
}

/// New requests admitted per one-second window, across all clients.
struct RateLimiter {
    per_second: u32,
    window: Instant,
    used: u32,
}

impl RateLimiter {
    fn new(per_second: u32) -> Self {
        Self {
            per_second,
            window: Instant::now(),
            used: 0,
        }
    }

    fn admit(&mut self) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.used = 0;
        }
        if self.used >= self.per_second {
            return false;
        }
        self.used += 1;
        true
    }
}
