//! Wallet-side transport for a Cashu mint reached over Nostr relays (`nostr://<npub>`).
//!
//! [`NostrMintConnector`] implements cdk's [`MintConnector`]: every call becomes one kind-23410
//! request ([`crate::mint_wire`]) and waits for a kind-23411 reply signed by the npub in the mint
//! URL. Everything above the connector (pay path, receipts, budget) is unchanged; an `https://` mint
//! keeps cdk's own `HttpClient` exactly as before. A `nostr://` mint never makes an HTTP request.
//!
//! Lost replies are handled here, below core: with no valid reply the connector re-sends the
//! IDENTICAL signed event (same event id, same request id) every `resend_every` until `window`
//! runs out, then returns [`Error::Timeout`] — ambiguous, never definitive, so cdk keeps pending
//! proofs instead of compensating them away. The mint rebuilds the original reply for a replayed
//! swap (spec §3.3), so a re-send after a lost reply completes normally.
//!
//! Each request uses a fresh throwaway client key and its own short-lived relay connection, so
//! relays cannot link requests to a wallet identity and the connector holds no state tied to one
//! tokio runtime (the payment worker runs its own runtime).

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cdk::cdk_database::{self, WalletDatabase};
use cdk::error::{ErrorCode as NutErrorCode, ErrorResponse};
use cdk::mint_url::MintUrl;
use cdk::nuts::{
    BatchCheckMintQuoteRequest, BatchMintRequest, CheckStateRequest, CheckStateResponse,
    CurrencyUnit, Id, KeySet, KeysResponse, KeysetResponse, MeltRequest, MintInfo, MintRequest,
    MintResponse, PaymentMethod, RestoreRequest, RestoreResponse, SwapRequest, SwapResponse,
};
use cdk::wallet::{
    AuthWallet, HttpClient, LnurlPayInvoiceResponse, LnurlPayResponse, MintConnector, Wallet,
    WalletBuilder,
};
use cdk::{
    Error, MeltQuoteCreateResponse, MeltQuoteRequest, MeltQuoteResponse, MintQuoteRequest,
    MintQuoteResponse,
};
use nostr_sdk::nips::nip44::{self, Version};
use nostr_sdk::prelude::{
    Client, Event, EventBuilder, EventId, Filter, Keys, Kind, PublicKey, RelayPoolNotification,
    Tag, Timestamp,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::mint_wire::{
    self, ErrorBody, ErrorCode, FALLBACK_RELAYS, MAX_PLAINTEXT_BYTES, Outcome, PROTOCOL_VERSION,
    REQUEST_KIND, RESPONSE_KIND, Request, Response, code, op,
};

/// Total time one request may wait for a reply, re-sends included (spec §3.3).
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(30);
/// Interval between identical re-sends while no valid reply has arrived.
pub const DEFAULT_RESEND_EVERY: Duration = Duration::from_secs(5);
/// How long to wait for at least one relay connection before failing the request.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Extra room an OUTER bound (the pay path's per-leg timeout) must leave past [`DEFAULT_WINDOW`], so
/// the connector always returns its own ambiguous `Error::Timeout` instead of being dropped while
/// its signed request is still valid. Covers the bounded relay disconnect after the window.
pub const OUTER_MARGIN: Duration = Duration::from_secs(10);
/// How long after a wallet saga's last update a `nostr://` request it may have published can still
/// be executed by the mint. Recovery must not unspend or forget that saga's inputs before then.
///
/// Bound: the send saga is stamped at the end of `prepare_send`; the publishing `confirm` is then
/// capped at `DEFAULT_WINDOW + OUTER_MARGIN`, and any request it starts carries an `exp` at most
/// `DEFAULT_WINDOW` later (~70s total). The swap saga `confirm` writes is stamped just before its
/// publish. Five minutes covers that plus [`crate::mint_wire::MAX_CLOCK_SKEW_SECS`] (asserted in a test); a forward wallet clock
/// step larger than the remainder during the window is not covered.
pub const REQUEST_SETTLE: Duration = Duration::from_secs(300);
/// Bound on the relay disconnect after a request, so it cannot stretch past [`OUTER_MARGIN`].
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The relays a wallet uses for a `nostr://` mint: the home's own `relay_url`, then
/// [`FALLBACK_RELAYS`], without duplicates.
pub fn relays_for(relay_url: &str) -> Vec<String> {
    let mut out = vec![relay_url.to_owned()];
    for relay in FALLBACK_RELAYS {
        if !out
            .iter()
            .any(|known| known.trim_end_matches('/') == relay.trim_end_matches('/'))
        {
            out.push((*relay).to_owned());
        }
    }
    out
}

/// The connector for `mint_url`: [`NostrMintConnector`] for `nostr://`, cdk's `HttpClient` (as
/// before) for anything else. A `nostr://` URL that doesn't carry a valid npub is an error, never
/// an HTTP client.
pub fn mint_connector_for(
    mint_url: &MintUrl,
    relay_url: &str,
) -> Result<Arc<dyn MintConnector + Send + Sync>, Error> {
    if mint_wire::is_nostr_scheme(&mint_url.to_string()) {
        Ok(Arc::new(NostrMintConnector::new(
            mint_url,
            relays_for(relay_url),
        )?))
    } else {
        Ok(Arc::new(HttpClient::new(mint_url.clone(), None)))
    }
}

/// Build a wallet at `mint_url`. `https://` is exactly `Wallet::new` as before. `nostr://` gets a
/// [`NostrMintConnector`] and poll-mode subscriptions (cdk's WebSocket path joins `/v1/ws` onto the
/// mint URL and panics on `nostr://`).
pub fn build_wallet(
    mint_url: &str,
    unit: CurrencyUnit,
    localstore: Arc<dyn WalletDatabase<cdk_database::Error> + Send + Sync>,
    seed: [u8; 64],
    target_proof_count: Option<usize>,
    relay_url: &str,
) -> Result<Wallet, Error> {
    if !mint_wire::is_nostr_scheme(mint_url) {
        return Wallet::new(mint_url, unit, localstore, seed, target_proof_count);
    }
    build_wallet_with_relays(
        mint_url,
        unit,
        localstore,
        seed,
        target_proof_count,
        relays_for(relay_url),
    )
}

/// [`build_wallet`] for a `nostr://` mint with an explicit relay list (tests use a local relay).
pub fn build_wallet_with_relays(
    mint_url: &str,
    unit: CurrencyUnit,
    localstore: Arc<dyn WalletDatabase<cdk_database::Error> + Send + Sync>,
    seed: [u8; 64],
    target_proof_count: Option<usize>,
    relays: Vec<String>,
) -> Result<Wallet, Error> {
    let url = MintUrl::from_str(mint_url)?;
    let connector = NostrMintConnector::new(&url, relays)?;
    WalletBuilder::new()
        .mint_url(url)
        .unit(unit)
        .localstore(localstore)
        .seed(seed)
        .target_proof_count(target_proof_count.unwrap_or(3))
        .client(connector)
        .use_http_subscription()
        .build()
}

/// A cdk mint connector that talks to a `nostr://<npub>` mint over Nostr relays.
#[derive(Debug, Clone)]
pub struct NostrMintConnector {
    mint_url: MintUrl,
    mint_pk: PublicKey,
    relays: Vec<String>,
    window: Duration,
    resend_every: Duration,
    connect_timeout: Duration,
}

impl NostrMintConnector {
    /// Connector for a `nostr://<npub>` mint URL using `relays`.
    pub fn new(mint_url: &MintUrl, relays: Vec<String>) -> Result<Self, Error> {
        let raw = mint_url.to_string();
        let npub = mint_wire::nostr_mint_npub(&raw)
            .ok_or_else(|| Error::Custom(format!("not a nostr:// mint url: {raw}")))?;
        let mint_pk = PublicKey::parse(npub)
            .map_err(|error| Error::Custom(format!("invalid mint npub {npub}: {error}")))?;
        if relays.is_empty() {
            return Err(Error::Custom(
                "no relays configured for nostr:// mint".into(),
            ));
        }
        Ok(Self {
            mint_url: mint_url.clone(),
            mint_pk,
            relays,
            window: DEFAULT_WINDOW,
            resend_every: DEFAULT_RESEND_EVERY,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        })
    }

    /// Override the reply window, re-send interval and connect timeout (tests keep them short).
    pub fn with_timing(
        mut self,
        window: Duration,
        resend_every: Duration,
        connect: Duration,
    ) -> Self {
        self.window = window;
        self.resend_every = resend_every;
        self.connect_timeout = connect;
        self
    }

    /// The mint's Nostr public key (the npub in its URL).
    pub fn mint_pubkey(&self) -> PublicKey {
        self.mint_pk
    }

    /// Send one request and return the mint's `ok` JSON, or the mapped error.
    ///
    /// The whole call, relay connect included, ends at `deadline = start + window`. The request's
    /// `exp` is [`request_exp`] of the same start: the mint refuses from that second on, which is
    /// never later than the deadline, so once the connector stops waiting a compliant mint (with a
    /// clock within `MAX_CLOCK_SKEW_SECS`) can no longer execute it (spec §3.1). Nothing past the
    /// deadline is awaited except a bounded disconnect.
    pub async fn call_raw(&self, operation: &str, body: Value) -> Result<Value, Error> {
        let started_unix_ms = unix_now_ms();
        let deadline = Instant::now() + self.window;
        let request = Request {
            v: PROTOCOL_VERSION,
            id: random_id()?,
            op: operation.to_owned(),
            body,
            exp: request_exp(started_unix_ms, self.window),
        };
        let plaintext = serde_json::to_string(&request)
            .map_err(|error| Error::Custom(format!("encode {operation} request: {error}")))?;
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            // Nothing was published, so a definitive refusal is honest.
            return Err(Error::HttpError(
                Some(413),
                format!(
                    "{operation} request is {} bytes, over the {MAX_PLAINTEXT_BYTES}-byte NIP-44 limit",
                    plaintext.len()
                ),
            ));
        }

        let keys = Keys::generate();
        let content = nip44::encrypt(keys.secret_key(), &self.mint_pk, &plaintext, Version::V2)
            .map_err(|error| Error::Custom(format!("encrypt {operation} request: {error}")))?;
        let event = EventBuilder::new(Kind::Custom(REQUEST_KIND), content)
            .tag(Tag::public_key(self.mint_pk))
            .sign_with_keys(&keys)
            .map_err(|error| Error::Custom(format!("sign {operation} request: {error}")))?;

        let client = Client::new(keys.clone());
        client.automatic_authentication(true);
        for relay in &self.relays {
            let _ = client.add_relay(relay.as_str()).await;
        }
        client.connect().await;
        client
            .wait_for_connection(self.connect_timeout.min(remaining(deadline)))
            .await;
        let connected = client
            .relays()
            .await
            .values()
            .any(|relay| relay.is_connected());
        if !connected {
            disconnect(&client).await;
            // Same class as an unreachable HTTPS mint today (connection refused): ambiguous.
            return Err(Error::HttpError(
                None,
                format!(
                    "no relay reachable for mint {} ({:?})",
                    self.mint_url, self.relays
                ),
            ));
        }

        // Subscribe BEFORE publishing, so a fast reply isn't missed.
        let mut notifications = client.notifications();
        let filter = Filter::new()
            .kind(Kind::Custom(RESPONSE_KIND))
            .pubkey(keys.public_key())
            .author(self.mint_pk)
            .since(Timestamp::now() - Duration::from_secs(10));
        let _ = tokio::time::timeout(remaining(deadline), client.subscribe(filter, None)).await;

        let mut next_send = Instant::now();
        let outcome = loop {
            let now = Instant::now();
            if now >= deadline {
                break Err(Error::Timeout);
            }
            if now >= next_send {
                // A publish error is not a result; keep waiting and re-send. Bounded by the deadline
                // so a slow relay OK cannot carry the call past `exp`.
                let _ = tokio::time::timeout(remaining(deadline), client.send_event(&event)).await;
                next_send = Instant::now() + self.resend_every;
            }
            let wait = next_send
                .min(deadline)
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            match tokio::time::timeout(wait, notifications.recv()).await {
                Err(_) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    break Err(Error::HttpError(None, "relay pool closed".into()));
                }
                Ok(Ok(RelayPoolNotification::Event { event: reply, .. })) => {
                    if let Some(result) = self.accept(&reply, &event.id, &keys, &request.id) {
                        break result;
                    }
                }
                Ok(Ok(_)) => continue,
            }
        };
        disconnect(&client).await;
        outcome
    }

    /// A valid reply to THIS request, or `None` (ignored: wrong kind, signer, request, version,
    /// bad signature, undecryptable or malformed).
    fn accept(
        &self,
        reply: &Event,
        request_event: &EventId,
        keys: &Keys,
        request_id: &str,
    ) -> Option<Result<Value, Error>> {
        if reply.kind != Kind::Custom(RESPONSE_KIND) || reply.pubkey != self.mint_pk {
            return None;
        }
        if !reply.tags.event_ids().any(|id| id == request_event) {
            return None;
        }
        reply.verify().ok()?;
        let plain = nip44::decrypt(keys.secret_key(), &self.mint_pk, &reply.content).ok()?;
        let response: Response = serde_json::from_str(&plain).ok()?;
        if response.v != PROTOCOL_VERSION || response.id != request_id {
            return None;
        }
        Some(match response.outcome {
            Outcome::Ok(value) => Ok(value),
            Outcome::Err(error) => Err(map_error(error)),
        })
    }

    async fn call<T: DeserializeOwned>(&self, operation: &str, body: Value) -> Result<T, Error> {
        let value = self.call_raw(operation, body).await?;
        serde_json::from_value(value)
            .map_err(|error| Error::InvalidMintResponse(format!("{operation}: {error}")))
    }
}

/// Map a wire error to the cdk error the same failure would produce over HTTPS. Definitive only
/// where the mint provably did not execute the request.
pub fn map_error(error: ErrorBody) -> Error {
    match error.code {
        ErrorCode::Nut(nut) => Error::from(ErrorResponse {
            code: NutErrorCode::from_code(nut),
            detail: error.detail,
        }),
        ErrorCode::Named(name) => match name.as_str() {
            code::UNSUPPORTED => {
                Error::HttpError(Some(404), format!("unsupported: {}", error.detail))
            }
            code::RATE_LIMITED => {
                Error::HttpError(Some(429), format!("rate limited: {}", error.detail))
            }
            code::BAD_REQUEST | code::EXPIRED => {
                Error::HttpError(Some(400), format!("{name}: {}", error.detail))
            }
            code::INTERNAL => Error::HttpError(Some(500), format!("internal: {}", error.detail)),
            _ => Error::UnknownErrorResponse(format!("{name}: {}", error.detail)),
        },
    }
}

/// Time left until `deadline` (zero once it has passed).
fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

async fn disconnect(client: &Client) {
    let _ = tokio::time::timeout(DISCONNECT_TIMEOUT, client.disconnect()).await;
}

/// A fresh request id. An OS RNG failure is an ambiguous error before anything is published, never
/// a panic inside the payment worker.
fn random_id() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| Error::Custom(format!("request id: {error}")))?;
    Ok(hex::encode(bytes))
}

/// `exp` for a request started at `started_unix_ms` with `window`: the latest whole unix second not
/// after `start + window`. The mint refuses when `now >= exp`, so every instant it may still execute
/// the request lies before the connector stops waiting. The connector may keep waiting up to 1s past
/// `exp`; that direction is safe (it only hears a refusal or nothing).
pub fn request_exp(started_unix_ms: u64, window: Duration) -> u64 {
    let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
    started_unix_ms.saturating_add(window_ms) / 1000
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn unsupported_off_mint(what: &str) -> Error {
    Error::Custom(format!(
        "{what} is not available through a nostr:// mint connector"
    ))
}

#[async_trait]
impl MintConnector for NostrMintConnector {
    async fn fetch_lnurl_pay_request(&self, _url: &str) -> Result<LnurlPayResponse, Error> {
        Err(unsupported_off_mint("LNURL pay request"))
    }

    async fn fetch_lnurl_invoice(&self, _url: &str) -> Result<LnurlPayInvoiceResponse, Error> {
        Err(unsupported_off_mint("LNURL invoice"))
    }

    async fn get_mint_keys(&self) -> Result<Vec<KeySet>, Error> {
        Ok(self
            .call::<KeysResponse>(op::KEYS, Value::Null)
            .await?
            .keysets)
    }

    async fn get_mint_keyset(&self, keyset_id: Id) -> Result<KeySet, Error> {
        let response: KeysResponse = self.call(op::KEYSET, json!({ "id": keyset_id })).await?;
        response
            .keysets
            .into_iter()
            .next()
            .ok_or(Error::UnknownKeySet)
    }

    async fn get_mint_keysets(&self) -> Result<KeysetResponse, Error> {
        self.call(op::KEYSETS, Value::Null).await
    }

    async fn post_mint_quote(
        &self,
        request: MintQuoteRequest,
    ) -> Result<MintQuoteResponse<String>, Error> {
        let method = request.method();
        self.call(
            op::MINT_QUOTE,
            json!({ "method": method, "request": request }),
        )
        .await
    }

    async fn post_mint(
        &self,
        method: &PaymentMethod,
        request: MintRequest<String>,
    ) -> Result<MintResponse, Error> {
        self.call(op::MINT, json!({ "method": method, "request": request }))
            .await
    }

    async fn post_batch_check_mint_quote_status(
        &self,
        method: &PaymentMethod,
        request: BatchCheckMintQuoteRequest<String>,
    ) -> Result<Vec<MintQuoteResponse<String>>, Error> {
        self.call(
            op::MINT_QUOTE_CHECK_BATCH,
            json!({ "method": method, "request": request }),
        )
        .await
    }

    async fn post_batch_mint(
        &self,
        method: &PaymentMethod,
        request: BatchMintRequest<String>,
    ) -> Result<MintResponse, Error> {
        self.call(
            op::MINT_BATCH,
            json!({ "method": method, "request": request }),
        )
        .await
    }

    async fn post_melt_quote(
        &self,
        request: MeltQuoteRequest,
    ) -> Result<MeltQuoteCreateResponse<String>, Error> {
        let method = request.method();
        self.call(
            op::MELT_QUOTE,
            json!({ "method": method, "request": request }),
        )
        .await
    }

    async fn get_mint_quote_status(
        &self,
        method: PaymentMethod,
        quote_id: &str,
    ) -> Result<MintQuoteResponse<String>, Error> {
        self.call(
            op::MINT_QUOTE_STATUS,
            json!({ "method": method, "quote": quote_id }),
        )
        .await
    }

    async fn get_melt_quote_status(
        &self,
        method: PaymentMethod,
        quote_id: &str,
    ) -> Result<MeltQuoteResponse<String>, Error> {
        self.call(
            op::MELT_QUOTE_STATUS,
            json!({ "method": method, "quote": quote_id }),
        )
        .await
    }

    async fn post_melt(
        &self,
        method: &PaymentMethod,
        request: MeltRequest<String>,
    ) -> Result<MeltQuoteResponse<String>, Error> {
        self.call(op::MELT, json!({ "method": method, "request": request }))
            .await
    }

    async fn post_swap(&self, request: SwapRequest) -> Result<SwapResponse, Error> {
        self.call(op::SWAP, to_body(&request)?).await
    }

    async fn get_mint_info(&self) -> Result<MintInfo, Error> {
        self.call(op::INFO, Value::Null).await
    }

    async fn post_check_state(
        &self,
        request: CheckStateRequest,
    ) -> Result<CheckStateResponse, Error> {
        self.call(op::CHECKSTATE, to_body(&request)?).await
    }

    async fn post_restore(&self, request: RestoreRequest) -> Result<RestoreResponse, Error> {
        self.call(op::RESTORE, to_body(&request)?).await
    }

    async fn get_auth_wallet(&self) -> Option<AuthWallet> {
        None
    }

    async fn set_auth_wallet(&self, _wallet: Option<AuthWallet>) {}
}

fn to_body<T: serde::Serialize>(request: &T) -> Result<Value, Error> {
    serde_json::to_value(request).map_err(|error| Error::Custom(format!("encode request: {error}")))
}

/// A shared, type-erased connector (the one a [`Wallet`] already holds), usable where a concrete
/// `MintConnector` type is required.
#[derive(Debug, Clone)]
pub struct SharedMintConnector(pub Arc<dyn MintConnector + Send + Sync>);

#[async_trait]
impl MintConnector for SharedMintConnector {
    async fn fetch_lnurl_pay_request(&self, url: &str) -> Result<LnurlPayResponse, Error> {
        self.0.fetch_lnurl_pay_request(url).await
    }
    async fn fetch_lnurl_invoice(&self, url: &str) -> Result<LnurlPayInvoiceResponse, Error> {
        self.0.fetch_lnurl_invoice(url).await
    }
    async fn get_mint_keys(&self) -> Result<Vec<KeySet>, Error> {
        self.0.get_mint_keys().await
    }
    async fn get_mint_keyset(&self, keyset_id: Id) -> Result<KeySet, Error> {
        self.0.get_mint_keyset(keyset_id).await
    }
    async fn get_mint_keysets(&self) -> Result<KeysetResponse, Error> {
        self.0.get_mint_keysets().await
    }
    async fn post_mint_quote(
        &self,
        request: MintQuoteRequest,
    ) -> Result<MintQuoteResponse<String>, Error> {
        self.0.post_mint_quote(request).await
    }
    async fn post_mint(
        &self,
        method: &PaymentMethod,
        request: MintRequest<String>,
    ) -> Result<MintResponse, Error> {
        self.0.post_mint(method, request).await
    }
    async fn post_batch_check_mint_quote_status(
        &self,
        method: &PaymentMethod,
        request: BatchCheckMintQuoteRequest<String>,
    ) -> Result<Vec<MintQuoteResponse<String>>, Error> {
        self.0
            .post_batch_check_mint_quote_status(method, request)
            .await
    }
    async fn post_batch_mint(
        &self,
        method: &PaymentMethod,
        request: BatchMintRequest<String>,
    ) -> Result<MintResponse, Error> {
        self.0.post_batch_mint(method, request).await
    }
    async fn post_melt_quote(
        &self,
        request: MeltQuoteRequest,
    ) -> Result<MeltQuoteCreateResponse<String>, Error> {
        self.0.post_melt_quote(request).await
    }
    async fn get_mint_quote_status(
        &self,
        method: PaymentMethod,
        quote_id: &str,
    ) -> Result<MintQuoteResponse<String>, Error> {
        self.0.get_mint_quote_status(method, quote_id).await
    }
    async fn get_melt_quote_status(
        &self,
        method: PaymentMethod,
        quote_id: &str,
    ) -> Result<MeltQuoteResponse<String>, Error> {
        self.0.get_melt_quote_status(method, quote_id).await
    }
    async fn post_melt(
        &self,
        method: &PaymentMethod,
        request: MeltRequest<String>,
    ) -> Result<MeltQuoteResponse<String>, Error> {
        self.0.post_melt(method, request).await
    }
    async fn post_swap(&self, request: SwapRequest) -> Result<SwapResponse, Error> {
        self.0.post_swap(request).await
    }
    async fn get_mint_info(&self) -> Result<MintInfo, Error> {
        self.0.get_mint_info().await
    }
    async fn post_check_state(
        &self,
        request: CheckStateRequest,
    ) -> Result<CheckStateResponse, Error> {
        self.0.post_check_state(request).await
    }
    async fn post_restore(&self, request: RestoreRequest) -> Result<RestoreResponse, Error> {
        self.0.post_restore(request).await
    }
    async fn get_auth_wallet(&self) -> Option<AuthWallet> {
        self.0.get_auth_wallet().await
    }
    async fn set_auth_wallet(&self, wallet: Option<AuthWallet>) {
        self.0.set_auth_wallet(wallet).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G_NPUB: &str = "npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";

    fn named(name: &str) -> Error {
        map_error(ErrorBody {
            code: ErrorCode::Named(name.to_owned()),
            detail: "d".into(),
        })
    }

    fn nut(code: u16) -> Error {
        map_error(ErrorBody {
            code: ErrorCode::Nut(code),
            detail: String::new(),
        })
    }

    #[test]
    fn nostr_error_mapping_pins_each_cdk_class() {
        // Pinned so a cdk upgrade that reclassifies any of these fails here (PR #1034 Cashu review).
        for (name, status) in [
            (code::BAD_REQUEST, 400),
            (code::EXPIRED, 400),
            (code::UNSUPPORTED, 404),
            (code::RATE_LIMITED, 429),
        ] {
            let error = named(name);
            assert!(
                matches!(error, Error::HttpError(Some(s), _) if s == status),
                "{name}: {error:?}"
            );
            assert!(error.is_definitive_failure(), "{name} must be definitive");
        }
        let internal = named(code::INTERNAL);
        assert!(
            matches!(internal, Error::HttpError(Some(500), _)),
            "{internal:?}"
        );
        assert!(!internal.is_definitive_failure(), "internal is ambiguous");
        let unknown = named("brand_new_code");
        assert!(
            matches!(unknown, Error::UnknownErrorResponse(_)),
            "{unknown:?}"
        );
        assert!(
            !unknown.is_definitive_failure(),
            "unknown codes are ambiguous"
        );
        let spent = nut(11001);
        assert!(matches!(spent, Error::TokenAlreadySpent), "{spent:?}");
        assert!(spent.is_definitive_failure());
        let pending = nut(11002);
        assert!(matches!(pending, Error::TokenPending), "{pending:?}");
        assert!(!pending.is_definitive_failure());
    }

    #[tokio::test]
    async fn nostr_oversized_request_is_a_definitive_413_before_any_relay() {
        let connector = NostrMintConnector::new(
            &MintUrl::from_str(&format!("nostr://{G_NPUB}")).unwrap(),
            vec!["ws://127.0.0.1:1".to_owned()],
        )
        .unwrap();
        let started = Instant::now();
        let error = connector
            .call_raw(op::SWAP, Value::String("x".repeat(MAX_PLAINTEXT_BYTES)))
            .await
            .expect_err("oversized");
        assert!(matches!(error, Error::HttpError(Some(413), _)), "{error:?}");
        assert!(error.is_definitive_failure());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "refused before connecting"
        );
    }

    #[test]
    fn nostr_request_exp_is_never_later_than_the_wait() {
        // Start 0.2s into a second: the wait ends at …030.2, exp = …030, and the mint refuses from
        // that second on — before the connector gives up.
        assert_eq!(
            request_exp(1_000_000_200, Duration::from_secs(30)),
            1_000_030
        );
        assert_eq!(
            request_exp(1_000_000_999, Duration::from_millis(1_500)),
            1_000_002
        );
        for start in [
            1_000_000_000u64,
            1_000_000_001,
            1_000_000_500,
            1_000_000_999,
        ] {
            for window_ms in [1u64, 999, 1_000, 1_500, 30_000] {
                let exp = request_exp(start, Duration::from_millis(window_ms));
                assert!(exp * 1000 <= start + window_ms, "exp past the wait");
                assert!(
                    (exp + 1) * 1000 > start + window_ms,
                    "exp is the latest safe second"
                );
            }
        }
    }
}
