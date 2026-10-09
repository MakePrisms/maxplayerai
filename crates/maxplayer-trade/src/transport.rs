//! Mint transport dispatch. Every request this crate sends to a mint goes through here, so a
//! caller never decides between HTTP and Nostr itself:
//!
//! - `http(s)://` mints: exactly the raw `reqwest` calls the trade CLI always made.
//! - `nostr://<npub>` mints: one kind-23410 request / kind-23411 reply per call through the
//!   reviewed core connector (`maxplayer_core::nostr_mint::NostrMintConnector`), over the
//!   configured mint relays only (never the market relays).
//!
//! Timing (why a `nostr://` call gets a longer bound than an HTTPS one): the connector re-sends one
//! signed request until its 30 s window ends and stamps it with an `exp` the mint enforces. An outer
//! timeout shorter than that window would drop the call while the signed request can still execute.
//! Dropping stays *ambiguous* here (every swap is journaled first and restored before any retry),
//! but the bounds below are sized so the connector returns its own answer, and the abandonment
//! grace covers the whole window, the `exp`, and the protocol's allowed clock skew.
use anyhow::{Context, Result, anyhow, ensure};
use cashu::nuts::{BlindedMessage, HTLCWitness, Id, Proofs, SecretKey, SwapRequest, Witness};
use cdk::mint_url::MintUrl;
use maxplayer_core::{
    mint_wire::{self, FALLBACK_RELAYS, MAX_CLOCK_SKEW_SECS, MAX_PLAINTEXT_BYTES, op},
    nostr_mint::{
        DEFAULT_CONNECT_TIMEOUT, DEFAULT_RESEND_EVERY, DEFAULT_WINDOW, NostrMintConnector,
        OUTER_MARGIN,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use std::{sync::RwLock, time::Duration};

/// Per-request bound for an HTTP mint POST (unchanged from before this module existed).
pub const HTTP_RPC_TIMEOUT: Duration = Duration::from_secs(20);
/// Bound for one CDK wallet call (metadata, fees, DLEQ keys) against an HTTP mint.
pub const HTTP_WALLET_TIMEOUT: Duration = Duration::from_secs(15);
/// Outer bound for one `nostr://` call: the connector's full window plus the core-documented
/// margin for its bounded disconnect, so the connector always answers before this fires.
pub const NOSTR_OUTER: Duration =
    Duration::from_secs(DEFAULT_WINDOW.as_secs() + OUTER_MARGIN.as_secs());
/// Abandonment grace for an HTTP swap: three HTTP timeouts (unchanged).
pub const HTTP_ABANDON_GRACE_SECONDS: u64 = 3 * HTTP_RPC_TIMEOUT.as_secs();
/// Abandonment grace for a `nostr://` swap: the connector window, its outer margin, the allowed
/// wallet/mint clock skew (the mint enforces `exp` on ITS clock), plus 20 s scheduling slack.
pub const NOSTR_ABANDON_GRACE_SECONDS: u64 =
    DEFAULT_WINDOW.as_secs() + OUTER_MARGIN.as_secs() + MAX_CLOCK_SKEW_SECS + 20;
/// Most inputs or outputs one sidecar request may carry (crates/maxplayer-mint dispatch MAX_IO).
pub const NOSTR_MAX_IO: usize = 128;
/// Headroom kept under the NIP-44 plaintext limit for estimate drift (amount digits, ids).
const PAYLOAD_MARGIN: usize = 512;
/// A deadline-bound `nostr://` swap needs at least this much window left, or it is not sent.
const MIN_NOSTR_WINDOW: Duration = Duration::from_secs(2);
/// Most mint relays a home may configure.
pub const MAX_MINT_RELAYS: usize = 8;
/// First default mint relay. Bob's decision (2026-10-09): relay.maxplayer.ai carries `nostr://`
/// mint traffic (kinds 23410/23411) only. The market fence in `market.rs` still refuses it for
/// 3410/3411/23412.
pub const DEFAULT_MINT_RELAY: &str = "wss://relay.maxplayer.ai";

static MINT_RELAYS: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// Whether `mint` uses the `nostr://` scheme (case-insensitive: such a URL never reaches HTTP).
pub fn is_nostr(mint: &str) -> bool {
    mint_wire::is_nostr_scheme(mint)
}

fn loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// One mint relay: `wss://`, or `ws://` on loopback (tests), no credentials/query/fragment.
pub fn check_mint_relay(relay: &str) -> Result<()> {
    let u = url::Url::parse(relay).with_context(|| format!("invalid mint relay {relay}"))?;
    let host = u.host_str().context("mint relay without host")?;
    ensure!(
        u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none(),
        "ambiguous mint relay URL {relay}"
    );
    ensure!(
        u.scheme() == "wss" || (u.scheme() == "ws" && loopback(host)),
        "mint relay must be wss:// (ws:// only on loopback): {relay}"
    );
    Ok(())
}

/// The default `nostr://` mint relays: [`DEFAULT_MINT_RELAY`] then core [`FALLBACK_RELAYS`], the
/// same list in the same order a default credits sidecar listens on (`maxplayer-mint`
/// `MintConfig::effective_relays`; parity asserted in `tests/nostr.rs`).
pub fn default_mint_relays() -> Vec<String> {
    std::iter::once(DEFAULT_MINT_RELAY)
        .chain(FALLBACK_RELAYS.iter().copied())
        .map(str::to_owned)
        .collect()
}

/// Set the relays used for `nostr://` mint traffic. Empty keeps [`default_mint_relays`].
/// Returns the effective list.
pub fn configure_mint_relays(relays: &[String]) -> Result<Vec<String>> {
    ensure!(
        relays.len() <= MAX_MINT_RELAYS,
        "at most {MAX_MINT_RELAYS} mint relays"
    );
    for relay in relays {
        check_mint_relay(relay)?;
    }
    let mut effective: Vec<String> = vec![];
    for relay in relays {
        if !effective.contains(relay) {
            effective.push(relay.clone());
        }
    }
    *MINT_RELAYS.write().unwrap_or_else(|e| e.into_inner()) =
        (!effective.is_empty()).then(|| effective.clone());
    Ok(mint_relays())
}

/// The relays `nostr://` mint requests use.
pub fn mint_relays() -> Vec<String> {
    MINT_RELAYS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(default_mint_relays)
}

/// Whether every hop to `mint` is loopback (lab-timing fence): the host for HTTP, every
/// configured mint relay for `nostr://`.
pub fn loopback_only(mint: &str) -> bool {
    if is_nostr(mint) {
        return mint_relays().iter().all(|r| {
            url::Url::parse(r)
                .ok()
                .and_then(|u| u.host_str().map(|h| h == "127.0.0.1" || h == "localhost"))
                .unwrap_or(false)
        });
    }
    url::Url::parse(mint)
        .ok()
        .is_some_and(|u| u.host_str() == Some("127.0.0.1"))
}

/// Bound for one CDK wallet call against `mint`.
pub fn wallet_bound(mint: &str) -> Duration {
    if is_nostr(mint) {
        NOSTR_OUTER
    } else {
        HTTP_WALLET_TIMEOUT
    }
}

/// How long after a swap's `send_before` an unanswered attempt must stay unabandoned.
pub fn abandon_grace_seconds(mint: &str) -> u64 {
    if is_nostr(mint) {
        NOSTR_ABANDON_GRACE_SECONDS
    } else {
        HTTP_ABANDON_GRACE_SECONDS
    }
}

/// The wallet connector for a `nostr://` mint over the configured mint relays.
pub fn connector(mint: &str) -> Result<NostrMintConnector> {
    let url: MintUrl = mint.parse()?;
    NostrMintConnector::new(&url, mint_relays()).map_err(|e| anyhow!("nostr mint {mint}: {e}"))
}

/// Refuse a path that has no `nostr://` equivalent (NUT-04/05/20 are not served by the sidecar).
pub fn require_http(mint: &str, what: &str) -> Result<()> {
    ensure!(
        !is_nostr(mint),
        "{what} is not available for nostr:// mints: the Maxplayer credits mint serves no \
         NUT-04 mint, NUT-05 melt or NUT-20; nothing was journaled or sent"
    );
    Ok(())
}

fn nostr_op(path: &str) -> Option<&'static str> {
    Some(match path {
        "swap" => op::SWAP,
        "checkstate" => op::CHECKSTATE,
        "restore" => op::RESTORE,
        "info" => op::INFO,
        "keysets" => op::KEYSETS,
        "keys" => op::KEYS,
        _ => return None,
    })
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

async fn nostr_call(
    mint: &str,
    path: &str,
    body: serde_json::Value,
    not_after: Option<u64>,
) -> Result<serde_json::Value> {
    let operation = nostr_op(path)
        .with_context(|| format!("mint {mint} {path}: not served over nostr://; nothing sent"))?;
    let mut c = connector(mint)?;
    if let Some(deadline) = not_after {
        // The request's `exp` (enforced on the mint's clock) is at most start + window, so a
        // window ending at `deadline` means the mint can never execute it at or after `deadline`.
        let left = Duration::from_millis(deadline.saturating_mul(1000).saturating_sub(unix_ms()));
        ensure!(
            left >= MIN_NOSTR_WINDOW,
            "mint {mint} {path}: too close to the attempt deadline; nothing sent"
        );
        if left < DEFAULT_WINDOW {
            c = c.with_timing(left, DEFAULT_RESEND_EVERY, DEFAULT_CONNECT_TIMEOUT);
        }
    }
    tokio::time::timeout(NOSTR_OUTER, c.call_raw(operation, body))
        .await
        .map_err(|_| anyhow!("mint {mint} {path}: no answer within the nostr bound (ambiguous)"))?
        .map_err(|e| match nostr_refusal(e) {
            // The typed refusal, exactly as an HTTP NUT error body yields; never "HTTP 400" text.
            Ok(refusal) => anyhow::Error::new(refusal).context(format!("mint {mint} {path}")),
            Err(e) => anyhow!("mint {mint} {path}: {e} (no mint NUT error; ambiguous)"),
        })
}

/// A [`crate::mint::MintRefusal`] for the mint's own definitive NUT error over nostr, else the
/// error back unchanged (ambiguous). See [`nostr_nut_refusal`].
fn nostr_refusal(e: cdk::Error) -> std::result::Result<crate::mint::MintRefusal, cdk::Error> {
    if !nostr_nut_refusal(&e) {
        return Err(e);
    }
    let r = cdk::error::ErrorResponse::from(e);
    Ok(crate::mint::MintRefusal::new(
        u64::from(r.code.to_code()),
        &r.detail,
    ))
}

/// Whether a nostr mint call failed with the mint's OWN NUT error (numeric wire `code`, mapped by
/// core to the cdk error HTTPS would give), which definitively refuses THIS request. Every
/// transport code stays ambiguous, even though core calls some of them "definitive": `expired`
/// and `bad_request` (core: HttpError 400; a wallet/mint clock skew can expire a valid request),
/// `rate_limited` (429), `unsupported` (404), `internal` (500), unknown codes, an oversized
/// request (413), timeouts and relay failures. Ambiguous means: not executed or unknown, so the
/// caller keeps the attempt and later replays the identical request. HTTP status codes cannot
/// tell these apart here, so no `HttpError` is ever a NUT refusal on this transport.
pub(crate) fn nostr_nut_refusal(e: &cdk::Error) -> bool {
    !matches!(
        e,
        cdk::Error::HttpError(..)
            | cdk::Error::Timeout
            | cdk::Error::UnknownErrorResponse(_)
            | cdk::Error::InvalidMintResponse(_)
            | cdk::Error::Custom(_)
    ) && e.is_definitive_failure()
}

/// POST-equivalent mint request (`checkstate`, `restore`, `swap`, and HTTP-only quote paths).
/// `not_after` (unix seconds) caps a `nostr://` request's `exp`; HTTP has no request expiry.
pub async fn post<T: DeserializeOwned>(
    mint: &str,
    path: &str,
    body: &impl Serialize,
    not_after: Option<u64>,
) -> Result<T> {
    if is_nostr(mint) {
        let value = nostr_call(mint, path, serde_json::to_value(body)?, not_after).await?;
        return serde_json::from_value(value)
            .with_context(|| format!("mint {mint} {path}: invalid response"));
    }
    let r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_RPC_TIMEOUT)
        .build()?
        .post(format!("{mint}/v1/{path}"))
        .json(body)
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    if let Some(refusal) = crate::mint::MintRefusal::parse(status.as_u16(), &text) {
        return Err(
            anyhow::Error::new(refusal).context(format!("mint {mint} {path}: HTTP {status}"))
        );
    }
    ensure!(
        status.is_success(),
        "mint {mint} {path}: HTTP {status} (body withheld)"
    );
    serde_json::from_str(&text).with_context(|| format!("mint {mint} {path}: invalid response"))
}

/// GET-equivalent read (`info`, `keysets`).
pub async fn get(mint: &str, path: &str) -> Result<serde_json::Value> {
    if is_nostr(mint) {
        return nostr_call(mint, path, serde_json::Value::Null, None).await;
    }
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_WALLET_TIMEOUT)
        .build()?
        .get(format!("{}/v1/{path}", mint.trim_end_matches('/')))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Plaintext bytes of the envelope that would carry `body` as `op` (the NIP-44 input).
pub fn envelope_len(operation: &str, body: &impl Serialize) -> Result<usize> {
    Ok(serde_json::to_string(&mint_wire::Request {
        v: mint_wire::PROTOCOL_VERSION,
        id: "0".repeat(32),
        op: operation.into(),
        body: serde_json::to_value(body)?,
        exp: u64::MAX,
    })?
    .len())
}

/// Refuse a `nostr://` swap that the connector could never send (nothing is published either way,
/// but a refusal at ADMISSION keeps funds from being locked into proofs nobody can redeem).
pub fn check_swap_fits(mint: &str, request: &SwapRequest) -> Result<()> {
    if !is_nostr(mint) {
        return Ok(());
    }
    ensure!(
        request.inputs().len() <= NOSTR_MAX_IO && request.outputs().len() <= NOSTR_MAX_IO,
        "nostr mint swap would carry more than {NOSTR_MAX_IO} inputs or outputs"
    );
    let len = envelope_len(op::SWAP, request)?;
    ensure!(
        len + PAYLOAD_MARGIN <= MAX_PLAINTEXT_BYTES,
        "nostr mint swap would be {len} bytes, over the {MAX_PLAINTEXT_BYTES}-byte NIP-44 limit \
         (with {PAYLOAD_MARGIN} bytes headroom)"
    );
    Ok(())
}

/// Would the HTLC claim of `locked` (each input carrying a 32-byte preimage and one signature,
/// `outputs` blinded outputs) fit a `nostr://` request? The refund (empty preimage) is smaller.
pub fn check_claim_fits(mint: &str, locked: &Proofs, outputs: usize) -> Result<()> {
    if !is_nostr(mint) {
        return Ok(());
    }
    let keyset = locked
        .first()
        .map(|p| p.keyset_id)
        .unwrap_or_else(|| Id::from_bytes(&[0; 8]).expect("static keyset id"));
    let inputs: Proofs = locked
        .iter()
        .map(|p| {
            let mut p = p.clone();
            p.dleq = None;
            p.witness = Some(Witness::HTLCWitness(HTLCWitness {
                preimage: "0".repeat(64),
                signatures: Some(vec!["0".repeat(128)]),
            }));
            p
        })
        .collect();
    let blinded = SecretKey::generate().public_key();
    let messages = (0..outputs)
        .map(|_| BlindedMessage::new(u64::MAX.into(), keyset, blinded))
        .collect();
    check_swap_fits(mint, &SwapRequest::new(inputs, messages))
        .context("HTLC claim of this lock would not fit a nostr mint request")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nostr_grace_covers_window_exp_and_skew() {
        assert!(
            NOSTR_ABANDON_GRACE_SECONDS
                > DEFAULT_WINDOW.as_secs() + OUTER_MARGIN.as_secs() + MAX_CLOCK_SKEW_SECS,
            "SAFETY: abandonment grace must outlast window + exp margin + clock skew"
        );
        assert!(
            NOSTR_OUTER >= DEFAULT_WINDOW + OUTER_MARGIN,
            "SAFETY: outer bound must not cut the connector window"
        );
        assert_eq!(HTTP_ABANDON_GRACE_SECONDS, 60, "HTTP grace unchanged");
        let npub = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
        assert_eq!(abandon_grace_seconds(npub), NOSTR_ABANDON_GRACE_SECONDS);
        assert_eq!(wallet_bound(npub), NOSTR_OUTER);
        assert_eq!(abandon_grace_seconds("https://mint.example"), 60);
        assert_eq!(wallet_bound("https://mint.example"), HTTP_WALLET_TIMEOUT);
    }
    #[test]
    fn nostr_claim_size_bound() {
        let npub = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
        let key = || SecretKey::generate().public_key().to_string();
        let c = crate::mint::conditions(&"ab".repeat(32), &key(), &key(), u64::MAX / 2).unwrap();
        let keyset = Id::from_bytes(&[0, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        let lock = |n: usize| -> Proofs {
            (0..n)
                .map(|_| {
                    let secret: cashu::nuts::nut10::Secret = c.clone().into();
                    cashu::nuts::Proof::new(
                        (1u64 << 40).into(),
                        keyset,
                        secret.try_into().unwrap(),
                        SecretKey::generate().public_key(),
                    )
                })
                .collect()
        };
        // Largest lock whose claim fits, found by search; the next one must be refused.
        let n = (1..=NOSTR_MAX_IO)
            .take_while(|&n| check_claim_fits(npub, &lock(n), 8).is_ok())
            .last()
            .unwrap();
        assert!(n < NOSTR_MAX_IO, "fixture: size, not count, binds");
        let over = lock(n + 1);
        assert!(
            check_claim_fits(npub, &over, 8).is_err(),
            "SAFETY: claim of {} proofs refused",
            n + 1
        );
        // The same proofs WITHOUT the claim witness would still fit: the bound must count the
        // per-input preimage + signature, or an admitted lock could be unclaimable.
        let blinded = SecretKey::generate().public_key();
        let bare = SwapRequest::new(
            over,
            (0..8)
                .map(|_| BlindedMessage::new(u64::MAX.into(), keyset, blinded))
                .collect(),
        );
        assert!(
            check_swap_fits(npub, &bare).is_ok(),
            "SAFETY: witness bytes must be counted in the claim bound"
        );
        assert!(check_claim_fits("https://mint.example", &lock(129), 8).is_ok());
    }
    #[test]
    fn only_a_mint_nut_error_is_a_nostr_refusal() {
        use maxplayer_core::{
            mint_wire::{ErrorBody, ErrorCode, code},
            nostr_mint::map_error,
        };
        let named = |name: &str| {
            map_error(ErrorBody {
                code: ErrorCode::Named(name.into()),
                detail: "x".into(),
            })
        };
        for name in [
            code::EXPIRED,
            code::BAD_REQUEST,
            code::RATE_LIMITED,
            code::UNSUPPORTED,
            code::INTERNAL,
            "brand_new_code",
        ] {
            assert!(
                !nostr_nut_refusal(&named(name)),
                "SAFETY: transport code {name} is not a refusal (not executed or unknown)"
            );
        }
        assert!(!nostr_nut_refusal(&cdk::Error::Timeout));
        assert!(!nostr_nut_refusal(&cdk::Error::HttpError(None, "x".into())));
        assert!(!nostr_nut_refusal(&cdk::Error::HttpError(
            Some(413),
            "x".into()
        )));
        let nut = |n: u16| {
            map_error(ErrorBody {
                code: ErrorCode::Nut(n),
                detail: "x".into(),
            })
        };
        assert!(nostr_nut_refusal(&nut(11001)), "token already spent");
        assert_eq!(
            nostr_refusal(nut(11001)).map(|r| r.code).ok(),
            Some(11001),
            "typed refusal carries the mint's NUT code"
        );
        assert!(
            nostr_refusal(named(code::EXPIRED)).is_err(),
            "SAFETY: expired never typed"
        );
        assert!(nostr_nut_refusal(&nut(11005)), "transaction unbalanced");
        assert!(
            !nostr_nut_refusal(&nut(11002)),
            "token pending stays ambiguous"
        );
        assert!(
            !nostr_nut_refusal(&nut(65_000)),
            "unknown NUT code stays ambiguous"
        );
    }

    #[test]
    fn mint_relays_fenced_and_bounded() {
        // Bob, 2026-10-09: mint traffic only (the market fence is tested in tests/relays.rs).
        assert!(check_mint_relay("wss://relay.maxplayer.ai").is_ok());
        assert!(check_mint_relay("ws://relay.maxplayer.ai").is_err());
        assert!(check_mint_relay("ws://relay.ditto.pub").is_err());
        assert!(check_mint_relay("wss://u:p@relay.ditto.pub").is_err());
        assert!(check_mint_relay("ws://127.0.0.1:7777").is_ok());
        assert!(check_mint_relay("wss://relay.ditto.pub").is_ok());
        let nine: Vec<String> = (0..9).map(|i| format!("wss://r{i}.example")).collect();
        assert!(configure_mint_relays(&nine).is_err());
        assert_eq!(
            configure_mint_relays(&[]).unwrap(),
            vec![
                "wss://relay.maxplayer.ai",
                "wss://relay.ditto.pub",
                "wss://nostr-pub.wellorder.net"
            ],
            "relay.maxplayer.ai first, then the core fallbacks"
        );
        assert!(!nostr_op("mint/quote/bolt11").is_some_and(|_| true));
        assert!(require_http("NOSTR://npub1x", "fund").is_err());
    }
}
