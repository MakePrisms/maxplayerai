//! Client for the email registration service (`crates/maxplayer-registration`, email signup spec
//! v2.12 §3–4): `maxplayer account link`, `account status` and `account marketing on|off`.
//!
//! The service's README is the contract. Every request is NIP-98 signed by the key being linked,
//! over the service's PUBLIC URL (`<registration_url><path>`, e.g.
//! `https://relay.maxplayer.ai/account/link/request`); a request with a body carries a `payload`
//! tag (SHA-256 of the exact body bytes), and each NIP-98 event is accepted once, so every request
//! signs a fresh event with a random `nonce` tag (two identical requests in the same second would
//! otherwise produce the same event id and the second would be refused as `replayed_auth`).
//!
//! The key is the agent's registered nostr key: the home `key` file, the same key the wallet signs
//! mint requests with since #1210 ([`crate::wallet_ops::open_wallet_async`]). This module never
//! reads, prints or returns the secret; callers hand it a [`Keys`].

use std::time::Duration;

use nostr_sdk::prelude::{
    Alphabet, EventBuilder, JsonUtil, Keys, Kind, SingleLetterTag, Tag, TagKind, Timestamp, Url,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Terms of Service version this build shows and accepts (spec §7.1: one page per version).
pub const TERMS_VERSION: &str = "v1";
/// Privacy Policy version shown when the email is asked for (spec §7.1, §3 step 1).
pub const PRIVACY_VERSION: &str = "v1";
/// Version of [`MARKETING_NOTICE`] (spec §8.1: the notice version shown is recorded).
pub const MARKETING_NOTICE_VERSION: &str = "v1";
/// The versioned pages the CLI links (the service also returns its own URLs on `link/request`).
pub const TERMS_URL: &str = "https://www.maxplayer.ai/terms/v1";
pub const PRIVACY_URL: &str = "https://www.maxplayer.ai/privacy/v1";

/// Spec §3 step 1: shown when the CLI asks for the email, or before it sends one given by flag.
pub fn privacy_notice() -> String {
    format!("Your email is used as described in our Privacy Policy: {PRIVACY_URL}")
}

/// Spec §3 step 6 / §8.1, text version [`MARKETING_NOTICE_VERSION`]. Product news is automatic
/// opt-in; service emails (codes, terms changes) are sent regardless.
pub const MARKETING_NOTICE: &str = "We'll send occasional product news too; unsubscribe any time \
     with `maxplayer account marketing off` or the unsubscribe link in any product-news email.";

/// Paths under the registration base URL (the service mounts them under `/account`).
pub const PATH_LINK_REQUEST: &str = "/link/request";
pub const PATH_LINK_CONFIRM: &str = "/link/confirm";
pub const PATH_STATUS: &str = "/status";
pub const PATH_MARKETING: &str = "/marketing";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Who typed the code (spec §5 `code_entered_by`). The service records `agent` when unsure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnteredBy {
    Human,
    Agent,
}

impl EnteredBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "human" => Some(Self::Human),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

/// `202 code_sent` from `POST /link/request`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CodeSent {
    pub status: String,
    pub expires_in_secs: u64,
    pub max_attempts: u32,
    pub terms_url: String,
    pub privacy_url: String,
}

/// `200 linked` from `POST /link/confirm`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Linked {
    pub status: String,
    pub first_link: bool,
    pub email: String,
    pub label: String,
    pub terms_version: String,
    pub privacy_version: String,
    /// `added` (the relay confirmed the NIP-43 add) or `pending` (queued, retried by the service).
    pub relay_member: String,
    /// `subscribed`, `unsubscribed` or `none`.
    pub marketing: String,
}

/// `GET /status`: `{registered:false}` or the full link.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Status {
    pub registered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_entered_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketing: Option<String>,
    /// `added`, `pending` or `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_member: Option<String>,
}

/// `POST /marketing` answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Marketing {
    /// `subscribed` or `unsubscribed`.
    pub marketing: String,
}

/// A refusal the service answered with its `{error, message}` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    /// The service's stable error code (`wrong_code`, `key_linked_to_other_email`, …).
    pub code: String,
    pub message: String,
    pub retry_after_secs: Option<u64>,
    pub attempts_left: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    /// The service refused the request (see [`ApiError::code`]).
    Api(ApiError),
    /// The service could not be reached, or the connection broke.
    Network { url: String, detail: String },
    /// Something answered that is not the service's JSON (a proxy error page, a bad gateway).
    BadResponse { url: String, status: u16 },
    /// Bad input caught before anything was sent (no request, no rate-limit slot, no code try used).
    Input(String),
}

impl RegistrationError {
    /// The service's error code, when it answered with one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api(api) => Some(api.code.as_str()),
            _ => None,
        }
    }
}

/// "45 seconds" / "about 3 minutes" / "about an hour".
pub fn wait_text(secs: u64) -> String {
    match secs {
        0..=1 => "1 second".to_owned(),
        2..=89 => format!("{secs} seconds"),
        90..=3299 => format!("about {} minutes", secs.div_ceil(60)),
        3300..=5399 => "about an hour".to_owned(),
        _ => format!("about {} hours", secs.div_ceil(3600)),
    }
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(api) => f.write_str(&plain_message(api)),
            Self::Network { url, detail } => write!(
                f,
                "Could not reach the registration service at {url} ({detail}). Check the network \
                 connection and try again."
            ),
            Self::BadResponse { url, status } => write!(
                f,
                "The registration service at {url} gave an unexpected answer (HTTP {status}). Try \
                 again in a few minutes."
            ),
            Self::Input(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RegistrationError {}

/// A plain-language message for each service error code (README "Errors"). Never echoes the email.
pub fn plain_message(api: &ApiError) -> String {
    let wait = api.retry_after_secs.map(wait_text);
    match api.code.as_str() {
        "wrong_code" => match api.attempts_left {
            Some(1) => "That code is not right. 1 try left.".to_owned(),
            Some(n) => format!("That code is not right. {n} tries left."),
            None => "That code is not right.".to_owned(),
        },
        "code_expired" => "That code has expired (codes last 10 minutes). Run `maxplayer account \
             link` again to get a new one."
            .to_owned(),
        "too_many_attempts" => "Too many wrong tries for this code. Run `maxplayer account link` \
             again to get a new code."
            .to_owned(),
        "no_pending_code" => {
            "No code is waiting for this key. Run `maxplayer account link --email \
             <your email> --label <name>` first to get one."
                .to_owned()
        }
        "resend_cooldown" => format!(
            "A code was sent to this key moments ago. Use that code, or wait {} and ask for a new \
             one.",
            wait.unwrap_or_else(|| "a minute".to_owned())
        ),
        "rate_limited" => format!(
            "Too many requests for now. Try again in {}.",
            wait.unwrap_or_else(|| "about an hour".to_owned())
        ),
        "key_linked_to_other_email" => "This agent's key is already linked to a different email. \
             Changing the email is not supported yet; `maxplayer account status` shows the \
             current link."
            .to_owned(),
        "invalid_email" => "That does not look like an email address.".to_owned(),
        "invalid_label" => {
            "The label must be 1-64 letters, digits, spaces, '.', '_' or '-'.".to_owned()
        }
        "unknown_terms_version" | "unknown_privacy_version" | "unknown_notice_version" => format!(
            "The registration service no longer accepts the terms or privacy version this \
             maxplayer offers ({TERMS_VERSION}). Update maxplayer and try again."
        ),
        "invalid_entered_by" | "invalid_body" => format!(
            "The registration service did not accept this request ({}). Update maxplayer and try \
             again.",
            api.code
        ),
        "email_send_failed" => {
            "The registration service could not send the code email. Try again later.".to_owned()
        }
        "not_registered" => {
            "This key is not linked to an email yet. Run `maxplayer account link` first.".to_owned()
        }
        "stale_auth" => "The registration service refused the request's signature as too old or \
             too new. Check that this computer's clock is right, then try again."
            .to_owned(),
        "url_mismatch" => "The registration service expected a different address. Check \
             `registration_url` (or MAXPLAYER_REGISTRATION_URL): it must be the service's public \
             URL."
            .to_owned(),
        "replayed_auth" => {
            "The registration service saw this signed request twice. Try again.".to_owned()
        }
        code if api.status >= 500 => format!(
            "The registration service had a problem ({code}, HTTP {}). Try again in a few minutes.",
            api.status
        ),
        code => format!(
            "The registration service refused the request ({code}, HTTP {}).",
            api.status
        ),
    }
}

/// A code is six digits; spaces and dashes are allowed ("482 913"), as the service allows them.
/// Checked here so a typo never uses up one of the code's five tries.
pub fn normalize_code(raw: &str) -> Option<String> {
    let code: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .collect();
    (code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

/// The service's label rule (1-64 letters, digits, spaces, `.`, `_`, `-` after trimming).
pub fn normalize_label(raw: &str) -> Option<String> {
    let label = raw.trim();
    let ok = !label.is_empty()
        && label.chars().count() <= 64
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-'));
    ok.then(|| label.to_owned())
}

/// A plain shape check; the service decides, and the code email proves the address works.
pub fn looks_like_email(raw: &str) -> bool {
    let email = raw.trim();
    match email.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !email.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

/// Lowercase hex SHA-256 of `body`, the NIP-98 `payload` tag value.
pub fn payload_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// The NIP-98 `Authorization` header value (`Nostr <base64 event>`) for one request: kind 27235,
/// `u` = `url` exactly, `method`, a `payload` tag when there is a body, and a `nonce` tag.
pub fn authorization(
    keys: &Keys,
    url: &str,
    method: &str,
    body: Option<&[u8]>,
    created_at: Timestamp,
    nonce: &str,
) -> Result<String, RegistrationError> {
    use base64::Engine as _;
    let mut tags = vec![
        Tag::custom(
            TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U)),
            [url.to_owned()],
        ),
        Tag::custom(TagKind::Method, [method.to_owned()]),
    ];
    if let Some(body) = body {
        tags.push(Tag::custom(TagKind::Payload, [payload_hash(body)]));
    }
    tags.push(Tag::custom(TagKind::custom("nonce"), [nonce.to_owned()]));
    let event = EventBuilder::new(Kind::HttpAuth, "")
        .tags(tags)
        .custom_created_at(created_at)
        .sign_with_keys(keys)
        .map_err(|error| {
            RegistrationError::Input(format!("could not sign the request: {error}"))
        })?;
    Ok(format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(event.as_json())
    ))
}

fn random_nonce() -> String {
    let mut bytes = [0u8; 16];
    // A failed OS RNG only weakens replay uniqueness; the timestamp still varies per second.
    let _ = getrandom::fill(&mut bytes);
    hex::encode(bytes)
}

/// Validate the configured base: `http(s)://host[:port][/path]`, no query, fragment or userinfo.
pub fn validate_base_url(base: &str) -> Result<String, RegistrationError> {
    let bad = |why: &str| {
        RegistrationError::Input(format!(
            "registration_url {base:?} is not usable ({why}); expected e.g. \
             {}",
            crate::home::DEFAULT_REGISTRATION_URL
        ))
    };
    let parsed = Url::parse(base.trim()).map_err(|_| bad("not a URL"))?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err(bad("must be https:// or http://"));
    }
    if parsed.host_str().is_none()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(bad("no query, fragment or credentials allowed"));
    }
    Ok(base.trim().trim_end_matches('/').to_owned())
}

fn is_loopback_host(base: &str) -> bool {
    Url::parse(base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(|c| c == '[' || c == ']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    retry_after_secs: Option<u64>,
    #[serde(default)]
    attempts_left: Option<u32>,
}

/// One registration-service session for one key.
pub struct RegistrationClient {
    base: String,
    keys: Keys,
    http: reqwest::blocking::Client,
}

impl RegistrationClient {
    /// `base_url` is the service's public base, e.g. `https://relay.maxplayer.ai/account`.
    pub fn new(base_url: &str, keys: Keys) -> Result<Self, RegistrationError> {
        let base = validate_base_url(base_url)?;
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // A redirect would carry the signed header somewhere it was not signed for.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("maxplayer/", env!("CARGO_PKG_VERSION")));
        if is_loopback_host(&base) {
            // A proxy cannot reach this machine's loopback (test and local staging services).
            builder = builder.no_proxy();
        }
        let http = builder
            .build()
            .map_err(|error| RegistrationError::Network {
                url: base.clone(),
                detail: format!("http client: {error}"),
            })?;
        Ok(Self { base, keys, http })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The exact URL a request to `path` is sent to and signed over.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Spec §3 steps 3–4: ask the service to email a 6-digit code to `email`.
    pub fn request_code(&self, email: &str, label: &str) -> Result<CodeSent, RegistrationError> {
        let email = email.trim();
        if !looks_like_email(email) {
            return Err(RegistrationError::Input(
                "That does not look like an email address.".to_owned(),
            ));
        }
        let label = normalize_label(label).ok_or_else(|| {
            RegistrationError::Input(
                "The label must be 1-64 letters, digits, spaces, '.', '_' or '-'.".to_owned(),
            )
        })?;
        let body = serde_json::json!({
            "email": email,
            "label": label,
            "terms_version": TERMS_VERSION,
            "privacy_version": PRIVACY_VERSION,
            "notice_version": MARKETING_NOTICE_VERSION,
        });
        self.send("POST", PATH_LINK_REQUEST, Some(&body))
    }

    /// Spec §3 step 5: submit the code; entering it accepts the terms version requested.
    pub fn confirm(&self, code: &str, entered_by: EnteredBy) -> Result<Linked, RegistrationError> {
        let code = normalize_code(code).ok_or_else(|| {
            RegistrationError::Input(
                "A code is the 6 digits from the email (spaces are fine).".to_owned(),
            )
        })?;
        let body = serde_json::json!({ "code": code, "entered_by": entered_by.as_str() });
        self.send("POST", PATH_LINK_CONFIRM, Some(&body))
    }

    pub fn status(&self) -> Result<Status, RegistrationError> {
        self.send("GET", PATH_STATUS, None)
    }

    /// Spec §4 / §8.1: product news on or off for this key's email. No email code needed.
    pub fn set_marketing(&self, enabled: bool) -> Result<Marketing, RegistrationError> {
        let body =
            serde_json::json!({ "enabled": enabled, "notice_version": MARKETING_NOTICE_VERSION });
        self.send("POST", PATH_MARKETING, Some(&body))
    }

    fn send<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, RegistrationError> {
        let url = self.url(path);
        let bytes = body.map(|b| b.to_string().into_bytes());
        let auth = authorization(
            &self.keys,
            &url,
            method,
            bytes.as_deref(),
            Timestamp::now(),
            &random_nonce(),
        )?;
        let request = match method {
            "GET" => self.http.get(&url),
            _ => self.http.post(&url),
        }
        .header("Authorization", auth)
        .header("Accept", "application/json");
        let request = match bytes {
            Some(bytes) => request
                .header("Content-Type", "application/json")
                .body(bytes),
            None => request,
        };
        let network = |error: reqwest::Error| RegistrationError::Network {
            url: url.clone(),
            detail: if error.is_timeout() {
                "timed out".to_owned()
            } else if error.is_connect() {
                "could not connect".to_owned()
            } else {
                error.to_string()
            },
        };
        let response = request.send().map_err(network)?;
        let status = response.status();
        let header_retry = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let text = response.text().map_err(network)?;
        if status.is_success() {
            return serde_json::from_str(&text).map_err(|_| RegistrationError::BadResponse {
                url: url.clone(),
                status: status.as_u16(),
            });
        }
        match serde_json::from_str::<ErrorBody>(&text) {
            Ok(error) => Err(RegistrationError::Api(ApiError {
                status: status.as_u16(),
                code: error.error,
                message: error.message,
                retry_after_secs: error.retry_after_secs.or(header_retry),
                attempts_left: error.attempts_left,
            })),
            Err(_) => Err(RegistrationError::BadResponse {
                url,
                status: status.as_u16(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::nips::nip98::{HttpMethod, verify_auth_header};
    use nostr_sdk::prelude::Event;

    fn decode(header: &str) -> Event {
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(header.strip_prefix("Nostr ").unwrap())
            .unwrap();
        Event::from_json(std::str::from_utf8(&raw).unwrap()).unwrap()
    }

    fn tag(event: &Event, name: &str) -> Vec<String> {
        event
            .tags
            .iter()
            .filter(|t| t.kind().as_str() == name)
            .filter_map(|t| t.content().map(str::to_owned))
            .collect()
    }

    /// The header the service checks: signed over the PUBLIC URL, with the body's SHA-256 as the
    /// payload tag. Verified independently with nostr's own NIP-98 verifier.
    #[test]
    fn link_request_is_signed_over_the_public_url_with_the_payload_hash() {
        let keys = Keys::generate();
        let url = format!(
            "{}{PATH_LINK_REQUEST}",
            crate::home::DEFAULT_REGISTRATION_URL
        );
        assert_eq!(url, "https://relay.maxplayer.ai/account/link/request");
        let body = br#"{"email":"a@b.co","label":"x"}"#;
        let now = Timestamp::now();
        let header = authorization(&keys, &url, "POST", Some(body), now, "n1").unwrap();
        let event = decode(&header);
        assert_eq!(event.kind, Kind::HttpAuth);
        assert_eq!(event.pubkey, keys.public_key());
        assert_eq!(tag(&event, "u"), std::slice::from_ref(&url));
        assert_eq!(tag(&event, "method"), ["POST"]);
        assert_eq!(tag(&event, "payload"), [hex::encode(Sha256::digest(body))]);
        assert_eq!(tag(&event, "nonce"), ["n1"]);
        let signer = verify_auth_header(
            &header,
            &Url::parse(&url).unwrap(),
            HttpMethod::POST,
            now,
            Some(body),
        )
        .expect("nostr's NIP-98 verifier accepts it");
        assert_eq!(signer, keys.public_key());
        // A different body does not verify: the payload tag binds this exact body.
        assert!(
            verify_auth_header(
                &header,
                &Url::parse(&url).unwrap(),
                HttpMethod::POST,
                now,
                Some(br#"{"email":"evil@b.co","label":"x"}"#),
            )
            .is_err()
        );
    }

    #[test]
    fn get_has_no_payload_tag_and_nonces_make_ids_unique() {
        let keys = Keys::generate();
        let url = "https://relay.maxplayer.ai/account/status";
        let now = Timestamp::now();
        let a = decode(&authorization(&keys, url, "GET", None, now, "a").unwrap());
        let b = decode(&authorization(&keys, url, "GET", None, now, "b").unwrap());
        assert!(tag(&a, "payload").is_empty());
        assert_ne!(
            a.id, b.id,
            "same second, same URL: still two distinct events"
        );
        assert_ne!(random_nonce(), random_nonce());
    }

    #[test]
    fn base_url_is_validated_and_trimmed() {
        assert_eq!(
            validate_base_url("https://relay.maxplayer.ai/account/").unwrap(),
            "https://relay.maxplayer.ai/account"
        );
        assert_eq!(
            validate_base_url("http://127.0.0.1:8080/account").unwrap(),
            "http://127.0.0.1:8080/account"
        );
        for bad in [
            "relay.maxplayer.ai/account",
            "wss://relay.maxplayer.ai/account",
            "https://relay.maxplayer.ai/account?x=1",
            "https://u:p@relay.maxplayer.ai/account",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad}");
        }
        assert!(is_loopback_host("http://127.0.0.1:1/account"));
        assert!(is_loopback_host("http://localhost:1/account"));
        assert!(is_loopback_host("http://[::1]:1/account"));
        assert!(!is_loopback_host("https://relay.maxplayer.ai/account"));
    }

    #[test]
    fn codes_labels_and_emails_are_checked_before_sending() {
        assert_eq!(normalize_code(" 482 913 ").as_deref(), Some("482913"));
        assert_eq!(normalize_code("482-913").as_deref(), Some("482913"));
        for bad in ["", "12345", "1234567", "12a456"] {
            assert!(normalize_code(bad).is_none(), "{bad}");
        }
        assert_eq!(normalize_label(" my-laptop ").as_deref(), Some("my-laptop"));
        assert!(normalize_label("<b>").is_none());
        assert!(normalize_label(&"x".repeat(65)).is_none());
        assert!(looks_like_email("you@example.com"));
        assert!(!looks_like_email("you@localhost"));
        assert!(!looks_like_email("you example.com"));
    }

    fn api(code: &str, status: u16) -> ApiError {
        ApiError {
            status,
            code: code.to_owned(),
            message: String::new(),
            retry_after_secs: None,
            attempts_left: None,
        }
    }

    #[test]
    fn every_documented_error_code_has_a_plain_message() {
        let wrong = ApiError {
            attempts_left: Some(3),
            ..api("wrong_code", 400)
        };
        assert_eq!(
            plain_message(&wrong),
            "That code is not right. 3 tries left."
        );
        let one = ApiError {
            attempts_left: Some(1),
            ..api("wrong_code", 400)
        };
        assert_eq!(plain_message(&one), "That code is not right. 1 try left.");
        let cooldown = ApiError {
            retry_after_secs: Some(42),
            ..api("resend_cooldown", 429)
        };
        assert!(plain_message(&cooldown).contains("42 seconds"));
        let limited = ApiError {
            retry_after_secs: Some(3600),
            ..api("rate_limited", 429)
        };
        assert!(plain_message(&limited).contains("about an hour"));
        assert!(plain_message(&api("code_expired", 410)).contains("maxplayer account link"));
        assert!(plain_message(&api("key_linked_to_other_email", 409)).contains("different email"));
        assert!(plain_message(&api("internal", 500)).contains("Try again"));
        // README error list: none falls through to the generic text by accident.
        for code in [
            "missing_auth",
            "malformed_auth",
            "wrong_kind",
            "bad_signature",
            "method_mismatch",
            "payload_missing",
            "payload_mismatch",
        ] {
            assert!(plain_message(&api(code, 401)).contains(code), "{code}");
        }
        for code in [
            "stale_auth",
            "url_mismatch",
            "replayed_auth",
            "invalid_body",
            "invalid_email",
            "invalid_label",
            "unknown_terms_version",
            "unknown_privacy_version",
            "unknown_notice_version",
            "invalid_entered_by",
            "rate_limited",
            "email_send_failed",
            "no_pending_code",
            "too_many_attempts",
            "not_registered",
        ] {
            let text = plain_message(&api(code, 400));
            assert!(!text.contains("refused the request ("), "{code}: {text}");
        }
    }

    #[test]
    fn wait_text_reads_plainly() {
        assert_eq!(wait_text(1), "1 second");
        assert_eq!(wait_text(59), "59 seconds");
        assert_eq!(wait_text(600), "about 10 minutes");
        assert_eq!(wait_text(3600), "about an hour");
    }

    #[test]
    fn notices_carry_the_versioned_links_and_the_off_switch() {
        assert!(privacy_notice().contains("https://www.maxplayer.ai/privacy/v1"));
        assert!(MARKETING_NOTICE.contains("maxplayer account marketing off"));
        assert_eq!(TERMS_URL, "https://www.maxplayer.ai/terms/v1");
    }
}
