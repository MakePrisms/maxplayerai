//! LNURL-pay resolution of a Lightning address to a bolt11 invoice — LUD-16 (`user@host`) over
//! LUD-06 (payRequest), **fail-closed at every step**.
//!
//! The flow, and the single place each of its hazards is refused:
//!
//! 1. `user@host` → `GET https://host/.well-known/lnurlp/user` ([`LightningAddress::well_known_url`]).
//!    The address is validated before any URL is built ([`LightningAddress::parse`]).
//! 2. The body must be a 200 with a JSON object tagged `payRequest`, an absolute **https** `callback`
//!    on the **same host** as the address, and integer `minSendable`/`maxSendable` in millisats
//!    ([`parse_pay_request`]). Anything else is a typed [`LnurlError`], never a default.
//! 3. `GET callback?amount=<millisats>` — only for an amount inside the advertised bounds
//!    ([`PayRequest::invoice_url`]).
//! 4. The body must carry `pr`, a bolt11 that decodes, is not expired, and whose amount **equals** the
//!    millisats requested ([`parse_invoice_response`]). The decoded payment hash is returned so the
//!    caller can journal it.
//!
//! No `http://` is accepted anywhere: the well-known URL is built `https://`, the callback must be
//! `https://`, and the shipped fetcher ([`HttpsFetch`]) refuses non-https URLs and follows no
//! redirects (a 3xx is a non-200 and is refused). Every network read goes through the [`LnurlFetch`]
//! trait so the parsing and the refusals are tested without a network.
//!
//! **Units.** The wire is millisats; the ledger is sats. The conversion lives in exactly three named
//! functions — [`sats_to_msat`], [`msat_to_sats_ceil`], [`msat_to_sats_floor`] — and nowhere else.
//! A minimum rounds UP to sats (1500 msat means you need 2 whole sats), a maximum rounds DOWN.
//!
//! This module moves no money. It produces an invoice; paying it is the caller's act
//! ([`crate::fee_remit`], through `wallet_ops::prepare_melt_payment_blocking` →
//! `PreparedMeltPayment::confirm` under a total ceiling — paying the one quote its store fence
//! bound, never a fresh one; `wallet_ops::pay_melt_quote_blocking` is retained but no longer called
//! on that path).

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use cdk::Bolt11Invoice;
/// The URL type the flow is expressed in (reqwest's re-export of `url::Url`), re-exported so a
/// caller can name a [`PayRequest::callback`] without depending on `reqwest` directly.
pub use reqwest::Url;

/// Millisats in one sat — the one conversion constant.
pub const MSAT_PER_SAT: u64 = 1_000;

/// How long one LNURL HTTP round trip may take before it is refused as a transport failure.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Largest response body accepted from an LNURL server. A payRequest is a few hundred bytes; a
/// bolt11 response is under 2 KiB. Anything approaching this is not an LNURL answer.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// A LUD-16 Lightning address, `user@host`, validated on construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightningAddress {
    user: String,
    host: String,
}

impl LightningAddress {
    /// Parse and validate `user@host`. Refuses anything that is not exactly one `@`, a non-empty
    /// user of `[A-Za-z0-9._-]`, and a non-empty hostname of `[A-Za-z0-9.-]` with no empty labels.
    /// The host is lower-cased (DNS is case-insensitive); the user is kept as written (LUD-16 says
    /// lowercase, and servers may be strict).
    pub fn parse(raw: &str) -> Result<Self, LnurlError> {
        let invalid = |reason: &'static str| LnurlError::InvalidAddress {
            address: raw.to_owned(),
            reason,
        };
        let (user, host) = raw.split_once('@').ok_or_else(|| invalid("no @"))?;
        if host.contains('@') {
            return Err(invalid("more than one @"));
        }
        if user.is_empty() {
            return Err(invalid("empty user"));
        }
        if !user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(invalid("user has a character outside [A-Za-z0-9._-]"));
        }
        if host.is_empty() {
            return Err(invalid("empty host"));
        }
        if !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        {
            return Err(invalid("host has a character outside [A-Za-z0-9.-]"));
        }
        if host.split('.').any(str::is_empty) {
            return Err(invalid("host has an empty label"));
        }
        Ok(Self {
            user: user.to_owned(),
            host: host.to_ascii_lowercase(),
        })
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// `https://host/.well-known/lnurlp/user` — always https, never anything else.
    pub fn well_known_url(&self) -> Url {
        Url::parse(&format!(
            "https://{}/.well-known/lnurlp/{}",
            self.host, self.user
        ))
        .expect("a validated address builds a valid https URL")
    }
}

impl fmt::Display for LightningAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.user, self.host)
    }
}

/// Every way the resolution refuses. One variant per hazard so a caller (and a test) can name which
/// gate closed; none of them is recoverable by retrying with a different default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LnurlError {
    InvalidAddress {
        address: String,
        reason: &'static str,
    },
    /// A URL in the flow is not `https://`. Checked before every fetch, independent of the fetcher.
    InsecureScheme {
        url: String,
    },
    /// The HTTP round trip itself failed (DNS, TLS, timeout, connection).
    Transport {
        url: String,
        detail: String,
    },
    /// Not a 200. A redirect is a 3xx here — redirects are never followed.
    HttpStatus {
        url: String,
        status: u16,
    },
    /// The body is not a JSON object.
    NotJson {
        url: String,
        detail: String,
    },
    /// The server answered `{"status":"ERROR","reason":...}`.
    ServiceError {
        reason: String,
    },
    /// `tag` missing or not `payRequest`.
    WrongTag {
        found: Option<String>,
    },
    CallbackMissing,
    CallbackNotAbsolute {
        callback: String,
    },
    CallbackNotHttps {
        callback: String,
    },
    /// The callback names a host other than the address's domain. Paying it would send the fee
    /// wherever a compromised or misconfigured well-known endpoint pointed.
    CallbackHostMismatch {
        expected: String,
        found: String,
    },
    /// The callback carries userinfo (`https://u:p@host/...`). Never legitimate here.
    CallbackHasCredentials {
        callback: String,
    },
    /// A millisat field is absent, or is not a non-negative JSON integer: strings, floats, exponent
    /// notation, negatives and out-of-range values are all refused, never prefix-parsed.
    AmountNotInteger {
        field: &'static str,
        found: String,
    },
    /// `minSendable` is zero (LUD-06 requires `> 0`) or exceeds `maxSendable`.
    BoundsInvalid {
        min_msat: u64,
        max_msat: u64,
    },
    AmountBelowMin {
        requested_msat: u64,
        min_msat: u64,
    },
    AmountAboveMax {
        requested_msat: u64,
        max_msat: u64,
    },
    /// `sats × 1000` does not fit a `u64`.
    AmountOverflow {
        sats: u64,
    },
    InvoiceMissing,
    InvoiceUndecodable {
        detail: String,
    },
    InvoiceExpired,
    /// The bolt11's amount is absent or differs from what was requested. Paying it would pay a
    /// figure the server chose, not the one the ledger owes.
    InvoiceAmountMismatch {
        expected_msat: u64,
        found_msat: Option<u64>,
    },
}

impl fmt::Display for LnurlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAddress { address, reason } => {
                write!(
                    formatter,
                    "lnurl: invalid lightning address {address:?}: {reason}"
                )
            }
            Self::InsecureScheme { url } => {
                write!(formatter, "lnurl: refusing non-https URL {url}")
            }
            Self::Transport { url, detail } => {
                write!(formatter, "lnurl: GET {url} failed: {detail}")
            }
            Self::HttpStatus { url, status } => {
                write!(
                    formatter,
                    "lnurl: GET {url} returned HTTP {status} (need 200)"
                )
            }
            Self::NotJson { url, detail } => {
                write!(
                    formatter,
                    "lnurl: GET {url} body is not a JSON object: {detail}"
                )
            }
            Self::ServiceError { reason } => {
                write!(formatter, "lnurl: service reported an error: {reason}")
            }
            Self::WrongTag { found: Some(tag) } => {
                write!(formatter, "lnurl: tag is {tag:?}, need \"payRequest\"")
            }
            Self::WrongTag { found: None } => write!(formatter, "lnurl: tag missing"),
            Self::CallbackMissing => write!(formatter, "lnurl: callback missing"),
            Self::CallbackNotAbsolute { callback } => {
                write!(
                    formatter,
                    "lnurl: callback is not an absolute URL: {callback:?}"
                )
            }
            Self::CallbackNotHttps { callback } => {
                write!(formatter, "lnurl: callback is not https: {callback}")
            }
            Self::CallbackHostMismatch { expected, found } => write!(
                formatter,
                "lnurl: callback host {found} is not the address host {expected}; refusing"
            ),
            Self::CallbackHasCredentials { callback } => {
                write!(formatter, "lnurl: callback carries credentials: {callback}")
            }
            Self::AmountNotInteger { field, found } => write!(
                formatter,
                "lnurl: {field} is not a non-negative JSON integer (millisats): {found}"
            ),
            Self::BoundsInvalid { min_msat, max_msat } => write!(
                formatter,
                "lnurl: sendable bounds invalid: minSendable={min_msat} maxSendable={max_msat} msat"
            ),
            Self::AmountBelowMin {
                requested_msat,
                min_msat,
            } => write!(
                formatter,
                "lnurl: {requested_msat} msat is below minSendable {min_msat} msat"
            ),
            Self::AmountAboveMax {
                requested_msat,
                max_msat,
            } => write!(
                formatter,
                "lnurl: {requested_msat} msat is above maxSendable {max_msat} msat"
            ),
            Self::AmountOverflow { sats } => {
                write!(formatter, "lnurl: {sats} sats overflows millisats")
            }
            Self::InvoiceMissing => write!(formatter, "lnurl: response has no pr (bolt11)"),
            Self::InvoiceUndecodable { detail } => {
                write!(formatter, "lnurl: pr is not a decodable bolt11: {detail}")
            }
            Self::InvoiceExpired => {
                write!(formatter, "lnurl: the returned bolt11 is already expired")
            }
            Self::InvoiceAmountMismatch {
                expected_msat,
                found_msat: Some(found),
            } => write!(
                formatter,
                "lnurl: bolt11 amount {found} msat != requested {expected_msat} msat; refusing"
            ),
            Self::InvoiceAmountMismatch {
                expected_msat,
                found_msat: None,
            } => write!(
                formatter,
                "lnurl: bolt11 carries no amount (requested {expected_msat} msat); refusing"
            ),
        }
    }
}

impl std::error::Error for LnurlError {}

/// A validated LUD-06 payRequest: where to ask for an invoice and the bounds it will honour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayRequest {
    pub callback: Url,
    pub min_sendable_msat: u64,
    pub max_sendable_msat: u64,
}

impl PayRequest {
    /// The smallest whole-sat amount the service accepts (millisats rounded UP).
    pub fn min_sendable_sats(&self) -> u64 {
        msat_to_sats_ceil(self.min_sendable_msat)
    }

    /// The largest whole-sat amount the service accepts (millisats rounded DOWN).
    pub fn max_sendable_sats(&self) -> u64 {
        msat_to_sats_floor(self.max_sendable_msat)
    }

    /// `callback?amount=<millisats>` for `amount_sats`, refused if the amount is outside the
    /// advertised bounds or the callback stopped being https.
    pub fn invoice_url(&self, amount_sats: u64) -> Result<Url, LnurlError> {
        let requested_msat = sats_to_msat(amount_sats)?;
        if requested_msat < self.min_sendable_msat {
            return Err(LnurlError::AmountBelowMin {
                requested_msat,
                min_msat: self.min_sendable_msat,
            });
        }
        if requested_msat > self.max_sendable_msat {
            return Err(LnurlError::AmountAboveMax {
                requested_msat,
                max_msat: self.max_sendable_msat,
            });
        }
        let mut url = self.callback.clone();
        url.query_pairs_mut()
            .append_pair("amount", &requested_msat.to_string());
        require_https(&url)?;
        Ok(url)
    }
}

/// A bolt11 the service issued for exactly the amount asked, with the figures the ledger journals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInvoice {
    pub bolt11: String,
    /// Hex of the invoice's payment hash — the remittance's idempotency key.
    pub payment_hash: String,
    pub amount_sats: u64,
    pub amount_msat: u64,
}

/// Sats → millisats, refusing overflow.
pub fn sats_to_msat(sats: u64) -> Result<u64, LnurlError> {
    sats.checked_mul(MSAT_PER_SAT)
        .ok_or(LnurlError::AmountOverflow { sats })
}

/// Millisats → sats, rounding UP: the right direction for a minimum (1 msat above a whole sat
/// means the next whole sat is the least you can send).
pub fn msat_to_sats_ceil(msat: u64) -> u64 {
    msat.div_ceil(MSAT_PER_SAT)
}

/// Millisats → sats, rounding DOWN: the right direction for a maximum.
pub fn msat_to_sats_floor(msat: u64) -> u64 {
    msat / MSAT_PER_SAT
}

fn require_https(url: &Url) -> Result<(), LnurlError> {
    if url.scheme() != "https" {
        return Err(LnurlError::InsecureScheme {
            url: url.to_string(),
        });
    }
    Ok(())
}

fn json_object(
    url: &Url,
    body: &[u8],
) -> Result<serde_json::Map<String, serde_json::Value>, LnurlError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|error| LnurlError::NotJson {
            url: url.to_string(),
            detail: error.to_string(),
        })?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        other => Err(LnurlError::NotJson {
            url: url.to_string(),
            detail: format!("top-level JSON is {}", json_kind(&other)),
        }),
    }
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// `{"status":"ERROR","reason":"..."}` is how LNURL services report failure (LUD-06). Surface it as
/// its own refusal rather than a confusing "tag missing".
fn refuse_service_error(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), LnurlError> {
    if map.get("status").and_then(serde_json::Value::as_str) == Some("ERROR") {
        return Err(LnurlError::ServiceError {
            reason: map
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("(no reason given)")
                .to_owned(),
        });
    }
    Ok(())
}

/// A millisat field must be a JSON **number** that is a non-negative integer. `serde_json` yields
/// `as_u64() == None` for floats (`1000.0`), exponents (`1e3`), negatives and anything past
/// `u64::MAX`; strings (`"1000"`, `"0junk"`, `" 1000"`), hex and leading-zero forms are either not
/// numbers or not valid JSON at all. Nothing here is prefix-parsed.
fn msat_field(
    map: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<u64, LnurlError> {
    let value = map.get(field).ok_or(LnurlError::AmountNotInteger {
        field,
        found: "(missing)".to_owned(),
    })?;
    match value {
        serde_json::Value::Number(number) => {
            number.as_u64().ok_or_else(|| LnurlError::AmountNotInteger {
                field,
                found: number.to_string(),
            })
        }
        other => Err(LnurlError::AmountNotInteger {
            field,
            found: other.to_string(),
        }),
    }
}

/// Validate a well-known payRequest body against the address it was fetched for.
pub fn parse_pay_request(
    body: &[u8],
    address: &LightningAddress,
) -> Result<PayRequest, LnurlError> {
    let url = address.well_known_url();
    let map = json_object(&url, body)?;
    refuse_service_error(&map)?;
    match map.get("tag") {
        Some(serde_json::Value::String(tag)) if tag == "payRequest" => {}
        Some(other) => {
            return Err(LnurlError::WrongTag {
                found: Some(other.to_string()),
            });
        }
        None => return Err(LnurlError::WrongTag { found: None }),
    }
    let callback_raw = match map.get("callback") {
        Some(serde_json::Value::String(callback)) => callback.as_str(),
        Some(other) => {
            return Err(LnurlError::CallbackNotAbsolute {
                callback: other.to_string(),
            });
        }
        None => return Err(LnurlError::CallbackMissing),
    };
    let callback = Url::parse(callback_raw).map_err(|_| LnurlError::CallbackNotAbsolute {
        callback: callback_raw.to_owned(),
    })?;
    if callback.cannot_be_a_base() || callback.host_str().is_none() {
        return Err(LnurlError::CallbackNotAbsolute {
            callback: callback_raw.to_owned(),
        });
    }
    if callback.scheme() != "https" {
        return Err(LnurlError::CallbackNotHttps {
            callback: callback_raw.to_owned(),
        });
    }
    if !callback.username().is_empty() || callback.password().is_some() {
        return Err(LnurlError::CallbackHasCredentials {
            callback: callback_raw.to_owned(),
        });
    }
    let callback_host = callback
        .host_str()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if callback_host != address.host() {
        return Err(LnurlError::CallbackHostMismatch {
            expected: address.host().to_owned(),
            found: callback_host,
        });
    }
    let min_sendable_msat = msat_field(&map, "minSendable")?;
    let max_sendable_msat = msat_field(&map, "maxSendable")?;
    if min_sendable_msat == 0 || min_sendable_msat > max_sendable_msat {
        return Err(LnurlError::BoundsInvalid {
            min_msat: min_sendable_msat,
            max_msat: max_sendable_msat,
        });
    }
    Ok(PayRequest {
        callback,
        min_sendable_msat,
        max_sendable_msat,
    })
}

/// Validate a callback response: `pr` present, a decodable, unexpired bolt11 whose amount equals
/// `expected_msat`. Returns the invoice with its payment hash.
pub fn parse_invoice_response(
    callback_url: &Url,
    body: &[u8],
    expected_msat: u64,
) -> Result<ResolvedInvoice, LnurlError> {
    let map = json_object(callback_url, body)?;
    refuse_service_error(&map)?;
    let pr = match map.get("pr") {
        Some(serde_json::Value::String(pr)) if !pr.trim().is_empty() => pr.trim().to_owned(),
        _ => return Err(LnurlError::InvoiceMissing),
    };
    let invoice = Bolt11Invoice::from_str(&pr).map_err(|error| LnurlError::InvoiceUndecodable {
        detail: error.to_string(),
    })?;
    if invoice.is_expired() {
        return Err(LnurlError::InvoiceExpired);
    }
    let found_msat = invoice.amount_milli_satoshis();
    if found_msat != Some(expected_msat) {
        return Err(LnurlError::InvoiceAmountMismatch {
            expected_msat,
            found_msat,
        });
    }
    Ok(ResolvedInvoice {
        bolt11: pr,
        payment_hash: invoice.payment_hash().to_string(),
        amount_sats: msat_to_sats_floor(expected_msat),
        amount_msat: expected_msat,
    })
}

/// One HTTP GET, abstracted so the flow is testable offline. Implementations MUST NOT follow
/// redirects and MUST refuse non-https URLs; [`fetch_pay_request`] / [`request_invoice`] check the
/// scheme again before calling, so a permissive implementation still cannot reach `http://`.
pub trait LnurlFetch {
    fn get(&self, url: &Url) -> Result<(u16, Vec<u8>), LnurlError>;
}

/// The shipped fetcher: reqwest, https-only, no redirects, bounded timeout and body.
pub struct HttpsFetch {
    client: reqwest::blocking::Client,
}

impl HttpsFetch {
    pub fn new() -> Result<Self, LnurlError> {
        let client = reqwest::blocking::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .user_agent("maxplayer-seller-fees-remit")
            .build()
            .map_err(|error| LnurlError::Transport {
                url: String::new(),
                detail: format!("build client: {error}"),
            })?;
        Ok(Self { client })
    }
}

impl LnurlFetch for HttpsFetch {
    fn get(&self, url: &Url) -> Result<(u16, Vec<u8>), LnurlError> {
        require_https(url)?;
        let response = self
            .client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .map_err(|error| LnurlError::Transport {
                url: url.to_string(),
                detail: error.to_string(),
            })?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(|error| LnurlError::Transport {
            url: url.to_string(),
            detail: error.to_string(),
        })?;
        if body.len() > MAX_BODY_BYTES {
            return Err(LnurlError::Transport {
                url: url.to_string(),
                detail: format!("body of {} bytes exceeds {MAX_BODY_BYTES}", body.len()),
            });
        }
        Ok((status, body.to_vec()))
    }
}

fn get_ok(fetch: &dyn LnurlFetch, url: &Url) -> Result<Vec<u8>, LnurlError> {
    require_https(url)?;
    let (status, body) = fetch.get(url)?;
    if status != 200 {
        return Err(LnurlError::HttpStatus {
            url: url.to_string(),
            status,
        });
    }
    Ok(body)
}

/// Step 1–2: fetch and validate the address's payRequest.
pub fn fetch_pay_request(
    fetch: &dyn LnurlFetch,
    address: &LightningAddress,
) -> Result<PayRequest, LnurlError> {
    let body = get_ok(fetch, &address.well_known_url())?;
    parse_pay_request(&body, address)
}

/// Step 3–4: ask the callback for an invoice of exactly `amount_sats` and validate what came back.
pub fn request_invoice(
    fetch: &dyn LnurlFetch,
    pay: &PayRequest,
    amount_sats: u64,
) -> Result<ResolvedInvoice, LnurlError> {
    let url = pay.invoice_url(amount_sats)?;
    let body = get_ok(fetch, &url)?;
    parse_invoice_response(&url, &body, sats_to_msat(amount_sats)?)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A signed bolt11 for tests: real encoding, real signature, chosen amount and expiry.

    use cdk::lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
    use cdk::secp256k1::hashes::{Hash, sha256};
    use cdk::secp256k1::{Secp256k1, SecretKey};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// A bolt11 for `amount_msat` (or amountless when `None`) timestamped `now - age`, expiring an
    /// hour after its timestamp. Its payment hash is the sha256 of `seed`.
    pub(crate) fn signed_bolt11(amount_msat: Option<u64>, seed: &[u8], age: Duration) -> String {
        let key = SecretKey::from_slice(&[0x42; 32]).expect("32 bytes is a key");
        let payment_hash = sha256::Hash::hash(seed);
        let mut builder = InvoiceBuilder::new(Currency::Bitcoin)
            .description("platform fee remittance (test)".into())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([7u8; 32]))
            .timestamp(SystemTime::now() - age)
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(3600));
        if let Some(msat) = amount_msat {
            builder = builder.amount_milli_satoshis(msat);
        }
        builder
            .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &key))
            .expect("a well-formed invoice signs")
            .to_string()
    }

    pub(crate) fn payment_hash_hex(seed: &[u8]) -> String {
        sha256::Hash::hash(seed).to_string()
    }

    #[allow(dead_code)]
    pub(crate) fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{payment_hash_hex, signed_bolt11};
    use super::*;
    use std::cell::RefCell;

    fn address() -> LightningAddress {
        LightningAddress::parse("maxplayer@strike.me").expect("valid")
    }

    fn pay_request_json(callback: &str, min: &str, max: &str) -> Vec<u8> {
        format!(
            r#"{{"tag":"payRequest","callback":{callback},"minSendable":{min},"maxSendable":{max},"metadata":"[]"}}"#
        )
        .into_bytes()
    }

    fn good_pay_request() -> PayRequest {
        parse_pay_request(
            &pay_request_json(
                r#""https://strike.me/lnurlp/maxplayer/callback""#,
                "1000",
                "1000000000",
            ),
            &address(),
        )
        .expect("the measured answer parses")
    }

    // ---- the address ----

    #[test]
    fn address_parses_user_at_host_and_builds_the_https_well_known_url() {
        let address = address();
        assert_eq!((address.user(), address.host()), ("maxplayer", "strike.me"));
        assert_eq!(
            address.well_known_url().as_str(),
            "https://strike.me/.well-known/lnurlp/maxplayer"
        );
        assert_eq!(address.to_string(), "maxplayer@strike.me");
        // Host is case-insensitive and lower-cased; user is kept.
        let mixed = LightningAddress::parse("Max.player_1@STRIKE.Me").expect("valid");
        assert_eq!((mixed.user(), mixed.host()), ("Max.player_1", "strike.me"));
    }

    #[test]
    fn address_refuses_every_malformed_shape() {
        for bad in [
            "",
            "maxplayer",
            "@strike.me",
            "maxplayer@",
            "max@player@strike.me",
            "max player@strike.me",
            "maxplayer@strike me",
            "maxplayer@strike..me",
            "maxplayer@.strike.me",
            "maxplayer@strike.me/",
            "maxplayer@strike.me:443",
            "maxplayer@strike.me?x=1",
            "max/player@strike.me",
            "https://strike.me/.well-known/lnurlp/maxplayer",
        ] {
            assert!(
                matches!(
                    LightningAddress::parse(bad),
                    Err(LnurlError::InvalidAddress { .. })
                ),
                "{bad:?} must be refused"
            );
        }
    }

    // ---- units: one named place, boundaries tested ----

    #[test]
    fn millisat_conversions_round_the_right_way_at_the_boundaries() {
        assert_eq!(sats_to_msat(0).unwrap(), 0);
        assert_eq!(sats_to_msat(1).unwrap(), 1000);
        assert_eq!(sats_to_msat(1_000_000).unwrap(), 1_000_000_000);
        assert_eq!(
            sats_to_msat(u64::MAX),
            Err(LnurlError::AmountOverflow { sats: u64::MAX })
        );
        assert_eq!(
            sats_to_msat(u64::MAX / 1000).unwrap(),
            (u64::MAX / 1000) * 1000
        );
        assert!(sats_to_msat(u64::MAX / 1000 + 1).is_err());

        // A minimum rounds UP: 1000 msat is 1 sat; 1001 msat means 2 whole sats are the least you
        // can send; 999 msat means 1 sat clears it.
        assert_eq!(msat_to_sats_ceil(0), 0);
        assert_eq!(msat_to_sats_ceil(1), 1);
        assert_eq!(msat_to_sats_ceil(999), 1);
        assert_eq!(msat_to_sats_ceil(1000), 1);
        assert_eq!(msat_to_sats_ceil(1001), 2);
        assert_eq!(msat_to_sats_ceil(1999), 2);
        assert_eq!(msat_to_sats_ceil(2000), 2);
        // A maximum rounds DOWN.
        assert_eq!(msat_to_sats_floor(999), 0);
        assert_eq!(msat_to_sats_floor(1000), 1);
        assert_eq!(msat_to_sats_floor(1999), 1);
        assert_eq!(msat_to_sats_floor(1_000_000_000), 1_000_000);

        // The fixture bounds (measured on the original host): 1000 msat = 1 sat, 1_000_000_000 msat = 1_000_000 sats.
        let pay = good_pay_request();
        assert_eq!(pay.min_sendable_sats(), 1);
        assert_eq!(pay.max_sendable_sats(), 1_000_000);
    }

    // ---- payRequest parsing ----

    #[test]
    fn pay_request_parses_the_measured_shape_and_builds_the_invoice_url() {
        let pay = good_pay_request();
        assert_eq!(pay.min_sendable_msat, 1000);
        assert_eq!(pay.max_sendable_msat, 1_000_000_000);
        let url = pay.invoice_url(21).expect("21 sats is in range");
        assert_eq!(
            url.as_str(),
            "https://strike.me/lnurlp/maxplayer/callback?amount=21000"
        );
        // An existing query string is extended, not clobbered.
        let with_query = parse_pay_request(
            &pay_request_json(r#""https://strike.me/cb?u=maxplayer""#, "1000", "2000"),
            &address(),
        )
        .expect("valid");
        assert_eq!(
            with_query.invoice_url(2).expect("in range").as_str(),
            "https://strike.me/cb?u=maxplayer&amount=2000"
        );
    }

    #[test]
    fn pay_request_refuses_non_json_and_non_object_bodies() {
        for body in [
            b"".as_slice(),
            b"not json",
            b"<html>502</html>",
            b"[]",
            b"\"payRequest\"",
            b"42",
            b"null",
        ] {
            assert!(
                matches!(
                    parse_pay_request(body, &address()),
                    Err(LnurlError::NotJson { .. })
                ),
                "{:?} must be refused as not-JSON-object",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn pay_request_refuses_a_service_error_wrong_tag_and_missing_tag() {
        assert_eq!(
            parse_pay_request(br#"{"status":"ERROR","reason":"no such user"}"#, &address()),
            Err(LnurlError::ServiceError {
                reason: "no such user".into()
            })
        );
        assert_eq!(
            parse_pay_request(
                br#"{"tag":"withdrawRequest","callback":"https://strike.me/cb","minSendable":1000,"maxSendable":2000}"#,
                &address()
            ),
            Err(LnurlError::WrongTag {
                found: Some("\"withdrawRequest\"".into())
            })
        );
        assert_eq!(
            parse_pay_request(
                br#"{"callback":"https://strike.me/cb","minSendable":1000,"maxSendable":2000}"#,
                &address()
            ),
            Err(LnurlError::WrongTag { found: None })
        );
        // Case matters: "PayRequest" is not the LUD-06 tag.
        assert!(matches!(
            parse_pay_request(
                br#"{"tag":"PayRequest","callback":"https://strike.me/cb","minSendable":1000,"maxSendable":2000}"#,
                &address()
            ),
            Err(LnurlError::WrongTag { .. })
        ));
    }

    #[test]
    fn pay_request_refuses_every_bad_callback() {
        /// Does this refusal name the hazard the case was built to trip?
        type Accepts = fn(&LnurlError) -> bool;
        let cases: [(&str, Accepts); 8] = [
            (
                r#"{"tag":"payRequest","minSendable":1000,"maxSendable":2000}"#,
                |e| matches!(e, LnurlError::CallbackMissing),
            ),
            (
                r#"{"tag":"payRequest","callback":"/lnurlp/cb","minSendable":1000,"maxSendable":2000}"#,
                |e| matches!(e, LnurlError::CallbackNotAbsolute { .. }),
            ),
            (
                r#"{"tag":"payRequest","callback":"strike.me/cb","minSendable":1000,"maxSendable":2000}"#,
                |e| matches!(e, LnurlError::CallbackNotAbsolute { .. }),
            ),
            (
                r#"{"tag":"payRequest","callback":42,"minSendable":1000,"maxSendable":2000}"#,
                |e| matches!(e, LnurlError::CallbackNotAbsolute { .. }),
            ),
            (
                r#"{"tag":"payRequest","callback":"http://strike.me/cb","minSendable":1000,"maxSendable":2000}"#,
                |e| matches!(e, LnurlError::CallbackNotHttps { .. }),
            ),
            (
                r#"{"tag":"payRequest","callback":"https://evil.example/cb","minSendable":1000,"maxSendable":2000}"#,
                |e| {
                    matches!(
                        e,
                        LnurlError::CallbackHostMismatch { expected, found }
                            if expected == "strike.me" && found == "evil.example"
                    )
                },
            ),
            (
                r#"{"tag":"payRequest","callback":"https://strike.me.evil.example/cb","minSendable":1000,"maxSendable":2000}"#,
                |e| matches!(e, LnurlError::CallbackHostMismatch { .. }),
            ),
            (
                r#"{"tag":"payRequest","callback":"https://user:pw@strike.me/cb","minSendable":1000,"maxSendable":2000}"#,
                |e| matches!(e, LnurlError::CallbackHasCredentials { .. }),
            ),
        ];
        for (body, accept) in cases {
            let error = parse_pay_request(body.as_bytes(), &address())
                .expect_err(&format!("{body} must be refused"));
            assert!(accept(&error), "{body}: wrong refusal {error:?}");
        }
        // A callback on the same host with a different case is the same host.
        assert!(parse_pay_request(
            br#"{"tag":"payRequest","callback":"https://STRIKE.me/cb","minSendable":1000,"maxSendable":2000}"#,
            &address()
        )
        .is_ok());
    }

    // The adversarial parse gate (brief §3.1): every junk shape is REFUSED, never prefix-parsed.
    #[test]
    fn pay_request_refuses_junk_amount_fields_rather_than_prefix_parsing_them() {
        let junk = [
            r#""0junk""#,
            r#""1000""#,
            r#""""#,
            r#"" ""#,
            r#"" 1000 ""#,
            r#""1000\n""#,
            "1000.0",
            "1000.5",
            "1e3",
            "1E3",
            "-1000",
            "-0.5",
            "18446744073709551616",
            "99999999999999999999999999999",
            "true",
            "null",
            "[1000]",
            r#"{"msat":1000}"#,
        ];
        for bad in junk {
            for field in ["minSendable", "maxSendable"] {
                let body = if field == "minSendable" {
                    pay_request_json(r#""https://strike.me/cb""#, bad, "2000")
                } else {
                    pay_request_json(r#""https://strike.me/cb""#, "1000", bad)
                };
                let result = parse_pay_request(&body, &address());
                assert!(
                    matches!(&result, Err(LnurlError::AmountNotInteger { field: f, .. }) if *f == field),
                    "{field}={bad} must be refused as not-an-integer, got {result:?}"
                );
            }
        }
        // Shapes that are not even JSON (leading zero, hex, trailing junk) fail at the JSON layer.
        for bad in ["01000", "0x3e8", "1000junk", "+1000", "1_000"] {
            let body = pay_request_json(r#""https://strike.me/cb""#, bad, "2000");
            assert!(
                matches!(
                    parse_pay_request(&body, &address()),
                    Err(LnurlError::NotJson { .. })
                ),
                "minSendable={bad} must not parse as JSON at all"
            );
        }
        // Missing fields are refused too, not defaulted.
        assert!(matches!(
            parse_pay_request(
                br#"{"tag":"payRequest","callback":"https://strike.me/cb","maxSendable":2000}"#,
                &address()
            ),
            Err(LnurlError::AmountNotInteger {
                field: "minSendable",
                ..
            })
        ));
        assert!(matches!(
            parse_pay_request(
                br#"{"tag":"payRequest","callback":"https://strike.me/cb","minSendable":1000}"#,
                &address()
            ),
            Err(LnurlError::AmountNotInteger {
                field: "maxSendable",
                ..
            })
        ));
        // Exactly u64::MAX is an integer and is accepted as a maximum.
        assert!(
            parse_pay_request(
                &pay_request_json(r#""https://strike.me/cb""#, "1000", "18446744073709551615"),
                &address()
            )
            .is_ok()
        );
    }

    #[test]
    fn pay_request_refuses_zero_or_inverted_bounds_and_out_of_range_amounts() {
        assert_eq!(
            parse_pay_request(
                &pay_request_json(r#""https://strike.me/cb""#, "0", "2000"),
                &address()
            ),
            Err(LnurlError::BoundsInvalid {
                min_msat: 0,
                max_msat: 2000
            })
        );
        assert_eq!(
            parse_pay_request(
                &pay_request_json(r#""https://strike.me/cb""#, "3000", "2000"),
                &address()
            ),
            Err(LnurlError::BoundsInvalid {
                min_msat: 3000,
                max_msat: 2000
            })
        );
        let pay = parse_pay_request(
            &pay_request_json(r#""https://strike.me/cb""#, "1500", "5000"),
            &address(),
        )
        .expect("valid");
        // 1500 msat minimum ⇒ 1 sat (1000 msat) is below it; 2 sats clears it.
        assert_eq!(pay.min_sendable_sats(), 2);
        assert_eq!(
            pay.invoice_url(1),
            Err(LnurlError::AmountBelowMin {
                requested_msat: 1000,
                min_msat: 1500
            })
        );
        assert!(pay.invoice_url(2).is_ok());
        assert!(pay.invoice_url(5).is_ok());
        assert_eq!(
            pay.invoice_url(6),
            Err(LnurlError::AmountAboveMax {
                requested_msat: 6000,
                max_msat: 5000
            })
        );
        assert_eq!(
            pay.invoice_url(0),
            Err(LnurlError::AmountBelowMin {
                requested_msat: 0,
                min_msat: 1500
            })
        );
        assert!(matches!(
            pay.invoice_url(u64::MAX),
            Err(LnurlError::AmountOverflow { .. })
        ));
    }

    // ---- invoice response parsing ----

    fn callback() -> Url {
        Url::parse("https://strike.me/cb?amount=21000").unwrap()
    }

    #[test]
    fn invoice_response_accepts_a_matching_unexpired_bolt11_and_returns_its_payment_hash() {
        let bolt11 = signed_bolt11(Some(21_000), b"seed-a", Duration::ZERO);
        let body = format!(r#"{{"pr":"  {bolt11}  ","routes":[]}}"#);
        let resolved = parse_invoice_response(&callback(), body.as_bytes(), 21_000).expect("valid");
        assert_eq!(resolved.bolt11, bolt11, "trimmed, otherwise verbatim");
        assert_eq!(resolved.payment_hash, payment_hash_hex(b"seed-a"));
        assert_eq!(resolved.payment_hash.len(), 64);
        assert_eq!((resolved.amount_sats, resolved.amount_msat), (21, 21_000));
    }

    #[test]
    fn invoice_response_refuses_missing_undecodable_expired_and_mismatched_invoices() {
        let cb = callback();
        assert!(matches!(
            parse_invoice_response(&cb, b"not json", 21_000),
            Err(LnurlError::NotJson { .. })
        ));
        assert_eq!(
            parse_invoice_response(
                &cb,
                br#"{"status":"ERROR","reason":"amount too low"}"#,
                21_000
            ),
            Err(LnurlError::ServiceError {
                reason: "amount too low".into()
            })
        );
        for body in [
            br#"{"routes":[]}"#.as_slice(),
            br#"{"pr":""}"#,
            br#"{"pr":"   "}"#,
            br#"{"pr":42}"#,
            br#"{"pr":null}"#,
        ] {
            assert_eq!(
                parse_invoice_response(&cb, body, 21_000),
                Err(LnurlError::InvoiceMissing),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        assert!(matches!(
            parse_invoice_response(&cb, br#"{"pr":"lnbc1notaninvoice"}"#, 21_000),
            Err(LnurlError::InvoiceUndecodable { .. })
        ));
        // Expired: timestamped two hours ago with a one-hour expiry.
        let expired = signed_bolt11(Some(21_000), b"seed-b", Duration::from_secs(7200));
        assert_eq!(
            parse_invoice_response(&cb, format!(r#"{{"pr":"{expired}"}}"#).as_bytes(), 21_000),
            Err(LnurlError::InvoiceExpired)
        );
        // Wrong amount, by one millisat either way, and amountless.
        for (msat, found) in [(21_001, Some(21_001)), (20_999, Some(20_999)), (0, None)] {
            let pr = signed_bolt11(
                if msat == 0 { None } else { Some(msat) },
                b"seed-c",
                Duration::ZERO,
            );
            assert_eq!(
                parse_invoice_response(&cb, format!(r#"{{"pr":"{pr}"}}"#).as_bytes(), 21_000),
                Err(LnurlError::InvoiceAmountMismatch {
                    expected_msat: 21_000,
                    found_msat: found
                }),
                "amount {msat}"
            );
        }
    }

    // ---- the driver, over a scripted fetcher ----

    /// One scripted answer: the exact URL expected, the status, the body.
    type Answer = (String, u16, Vec<u8>);

    struct Scripted {
        answers: RefCell<Vec<Answer>>,
        seen: RefCell<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: Vec<(&str, u16, Vec<u8>)>) -> Self {
            Self {
                answers: RefCell::new(
                    answers
                        .into_iter()
                        .map(|(url, status, body)| (url.to_owned(), status, body))
                        .collect(),
                ),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl LnurlFetch for Scripted {
        fn get(&self, url: &Url) -> Result<(u16, Vec<u8>), LnurlError> {
            self.seen.borrow_mut().push(url.to_string());
            let mut answers = self.answers.borrow_mut();
            let position = answers
                .iter()
                .position(|(expected, _, _)| expected == url.as_str())
                .unwrap_or_else(|| panic!("unexpected GET {url}"));
            let (_, status, body) = answers.remove(position);
            Ok((status, body))
        }
    }

    #[test]
    fn driver_resolves_well_known_then_callback_and_refuses_a_non_200_or_a_redirect() {
        let bolt11 = signed_bolt11(Some(21_000), b"seed-d", Duration::ZERO);
        let fetch = Scripted::new(vec![
            (
                "https://strike.me/.well-known/lnurlp/maxplayer",
                200,
                pay_request_json(
                    r#""https://strike.me/lnurlp/maxplayer/callback""#,
                    "1000",
                    "1000000000",
                ),
            ),
            (
                "https://strike.me/lnurlp/maxplayer/callback?amount=21000",
                200,
                format!(r#"{{"pr":"{bolt11}","routes":[]}}"#).into_bytes(),
            ),
        ]);
        let pay = fetch_pay_request(&fetch, &address()).expect("payRequest");
        let resolved = request_invoice(&fetch, &pay, 21).expect("invoice");
        assert_eq!(resolved.amount_sats, 21);
        assert_eq!(resolved.payment_hash, payment_hash_hex(b"seed-d"));
        assert_eq!(
            *fetch.seen.borrow(),
            vec![
                "https://strike.me/.well-known/lnurlp/maxplayer".to_owned(),
                "https://strike.me/lnurlp/maxplayer/callback?amount=21000".to_owned(),
            ]
        );

        // A 302 (redirects are never followed) and a 500 are both refused as non-200.
        for status in [302u16, 404, 500] {
            let fetch = Scripted::new(vec![(
                "https://strike.me/.well-known/lnurlp/maxplayer",
                status,
                b"whatever".to_vec(),
            )]);
            assert_eq!(
                fetch_pay_request(&fetch, &address()),
                Err(LnurlError::HttpStatus {
                    url: "https://strike.me/.well-known/lnurlp/maxplayer".into(),
                    status
                })
            );
        }
        // Out-of-range amounts never reach the network.
        let fetch = Scripted::new(vec![]);
        assert!(matches!(
            request_invoice(&fetch, &pay, 0),
            Err(LnurlError::AmountBelowMin { .. })
        ));
        assert!(matches!(
            request_invoice(&fetch, &pay, 1_000_001),
            Err(LnurlError::AmountAboveMax { .. })
        ));
        assert!(fetch.seen.borrow().is_empty());
    }

    // The scheme gate is the driver's, not only the fetcher's: even a fetcher that would happily
    // GET http:// is never asked to.
    #[test]
    fn driver_refuses_an_http_url_before_touching_the_fetcher() {
        struct Permissive(RefCell<usize>);
        impl LnurlFetch for Permissive {
            fn get(&self, _: &Url) -> Result<(u16, Vec<u8>), LnurlError> {
                *self.0.borrow_mut() += 1;
                Ok((200, b"{}".to_vec()))
            }
        }
        let fetch = Permissive(RefCell::new(0));
        let pay = PayRequest {
            callback: Url::parse("http://strike.me/cb").unwrap(),
            min_sendable_msat: 1000,
            max_sendable_msat: 2000,
        };
        assert!(matches!(
            request_invoice(&fetch, &pay, 1),
            Err(LnurlError::InsecureScheme { .. })
        ));
        assert_eq!(*fetch.0.borrow(), 0, "the fetcher was never called");
    }

    #[test]
    fn shipped_fetcher_refuses_http_without_a_network() {
        let fetch = HttpsFetch::new().expect("client builds");
        assert!(matches!(
            fetch.get(&Url::parse("http://127.0.0.1:9/x").unwrap()),
            Err(LnurlError::InsecureScheme { .. })
        ));
    }
}
