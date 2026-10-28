//! NIP-98 HTTP auth: verifying the CLI's requests, and signing the service's own
//! requests to the relay.
//!
//! Checked, in order: `Authorization: Nostr <base64 event>`, kind 27235, event id and
//! Schnorr signature, `created_at` within the allowed skew, exactly one `u` tag equal
//! to the service's own public URL for this route, exactly one `method` tag, and at
//! most one `payload` tag, which must be the SHA-256 of the exact body. A request
//! with a body must carry a `payload` tag (stricter than NIP-98's "SHOULD"), so a
//! captured header cannot be reused with a different email or code.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use nostr::{
    Alphabet, Event, EventBuilder, EventId, JsonUtil, Keys, Kind, PublicKey, SingleLetterTag, Tag,
    TagKind, Timestamp,
};
use sha2::{Digest, Sha256};

/// Largest accepted `Authorization` value. A real NIP-98 event is well under 1 KiB.
const MAX_HEADER_LEN: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nip98Error {
    Missing,
    Malformed,
    WrongKind,
    BadSignature,
    Stale,
    UrlMismatch,
    MethodMismatch,
    PayloadMissing,
    PayloadMismatch,
}

impl Nip98Error {
    /// Stable machine-readable reason, returned to the client.
    pub fn code(self) -> &'static str {
        match self {
            Self::Missing => "missing_auth",
            Self::Malformed => "malformed_auth",
            Self::WrongKind => "wrong_kind",
            Self::BadSignature => "bad_signature",
            Self::Stale => "stale_auth",
            Self::UrlMismatch => "url_mismatch",
            Self::MethodMismatch => "method_mismatch",
            Self::PayloadMissing => "payload_missing",
            Self::PayloadMismatch => "payload_mismatch",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Verified {
    pub pubkey: PublicKey,
    pub event_id: EventId,
}

fn u_kind() -> TagKind<'static> {
    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::U))
}

fn same_url(a: &str, b: &str) -> bool {
    // Scheme and host compare case-insensitively (Url normalises them); the path
    // compares exactly, ignoring one trailing slash. No query strings are used.
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(mut a), Ok(mut b)) => {
            let pa = a.path().trim_end_matches('/').to_string();
            let pb = b.path().trim_end_matches('/').to_string();
            a.set_path(&pa);
            b.set_path(&pb);
            a == b
        }
        _ => false,
    }
}

pub fn body_sha256_hex(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// Verify a NIP-98 `Authorization` header against `expected_url` and
/// `expected_method` at time `now` (unix seconds).
pub fn verify(
    header: Option<&str>,
    expected_url: &str,
    expected_method: &str,
    body: &[u8],
    now: u64,
    max_skew_secs: u64,
) -> Result<Verified, Nip98Error> {
    let header = header.ok_or(Nip98Error::Missing)?;
    if header.len() > MAX_HEADER_LEN {
        return Err(Nip98Error::Malformed);
    }
    let encoded = header
        .strip_prefix("Nostr ")
        .ok_or(Nip98Error::Missing)?
        .trim();
    let json = STANDARD
        .decode(encoded)
        .map_err(|_| Nip98Error::Malformed)?;
    let json = std::str::from_utf8(&json).map_err(|_| Nip98Error::Malformed)?;
    let event = Event::from_json(json).map_err(|_| Nip98Error::Malformed)?;

    if event.kind != Kind::HttpAuth {
        return Err(Nip98Error::WrongKind);
    }
    event.verify().map_err(|_| Nip98Error::BadSignature)?;

    let ts = event.created_at.as_secs();
    if ts.abs_diff(now) > max_skew_secs {
        return Err(Nip98Error::Stale);
    }

    let mut urls = event.tags.iter().filter(|t| t.kind() == u_kind());
    let url = urls.next().and_then(|t| t.content());
    if urls.next().is_some() || !url.is_some_and(|u| same_url(u, expected_url)) {
        return Err(Nip98Error::UrlMismatch);
    }

    let mut methods = event.tags.iter().filter(|t| t.kind() == TagKind::Method);
    let method = methods.next().and_then(|t| t.content());
    if methods.next().is_some() || !method.is_some_and(|m| m.eq_ignore_ascii_case(expected_method))
    {
        return Err(Nip98Error::MethodMismatch);
    }

    let mut payloads = event.tags.iter().filter(|t| t.kind() == TagKind::Payload);
    let payload = payloads.next();
    if payloads.next().is_some() {
        return Err(Nip98Error::PayloadMismatch);
    }
    match payload {
        Some(tag) => {
            let want = body_sha256_hex(body);
            if tag.content() != Some(want.as_str()) {
                return Err(Nip98Error::PayloadMismatch);
            }
        }
        None if !body.is_empty() => return Err(Nip98Error::PayloadMissing),
        None => {}
    }

    Ok(Verified {
        pubkey: event.pubkey,
        event_id: event.id,
    })
}

/// Sign a NIP-98 event for `url`/`method` (with a `payload` tag when `body` is given)
/// and return the full `Authorization` header value.
pub fn authorization(
    keys: &Keys,
    url: &str,
    method: &str,
    body: Option<&[u8]>,
    created_at: Timestamp,
) -> String {
    let mut tags = vec![
        Tag::custom(u_kind(), [url.to_string()]),
        Tag::custom(TagKind::Method, [method.to_string()]),
    ];
    if let Some(body) = body {
        tags.push(Tag::custom(TagKind::Payload, [body_sha256_hex(body)]));
    }
    let event = EventBuilder::new(Kind::HttpAuth, "")
        .tags(tags)
        .custom_created_at(created_at)
        .sign_with_keys(keys)
        .expect("signing with local keys cannot fail");
    format!("Nostr {}", STANDARD.encode(event.as_json()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://relay.maxplayer.ai/account/link/request";
    const NOW: u64 = 1_800_000_000;

    fn sign(keys: &Keys, url: &str, method: &str, body: Option<&[u8]>, ts: u64) -> String {
        authorization(keys, url, method, body, Timestamp::from(ts))
    }

    fn check(h: &str, body: &[u8]) -> Result<Verified, Nip98Error> {
        verify(Some(h), URL, "POST", body, NOW, 60)
    }

    #[test]
    fn valid_request_yields_the_signer() {
        let k = Keys::generate();
        let body = br#"{"email":"a@b.co"}"#;
        let v = check(&sign(&k, URL, "POST", Some(body), NOW), body).unwrap();
        assert_eq!(v.pubkey, k.public_key());
        // Trailing slash and host case are not significant.
        let h = sign(
            &k,
            "https://Relay.Maxplayer.ai/account/link/request/",
            "post",
            Some(body),
            NOW - 59,
        );
        assert!(check(&h, body).is_ok());
    }

    #[test]
    fn missing_or_garbled_headers_are_refused() {
        assert_eq!(
            verify(None, URL, "POST", b"", NOW, 60).unwrap_err(),
            Nip98Error::Missing
        );
        assert_eq!(check("Bearer x", b"").unwrap_err(), Nip98Error::Missing);
        assert_eq!(check("Nostr !!!", b"").unwrap_err(), Nip98Error::Malformed);
        let not_event = format!("Nostr {}", STANDARD.encode("{}"));
        assert_eq!(check(&not_event, b"").unwrap_err(), Nip98Error::Malformed);
        let huge = format!("Nostr {}", "A".repeat(MAX_HEADER_LEN));
        assert_eq!(check(&huge, b"").unwrap_err(), Nip98Error::Malformed);
    }

    #[test]
    fn bad_signature_is_refused() {
        let k = Keys::generate();
        let h = sign(&k, URL, "POST", None, NOW);
        let json =
            String::from_utf8(STANDARD.decode(h.strip_prefix("Nostr ").unwrap()).unwrap()).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Swap in another key's pubkey: the id no longer matches / sig no longer verifies.
        v["pubkey"] = serde_json::Value::String(Keys::generate().public_key().to_hex());
        let forged = format!("Nostr {}", STANDARD.encode(v.to_string()));
        assert_eq!(check(&forged, b"").unwrap_err(), Nip98Error::BadSignature);
        // Flip one signature nibble, keep the id.
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let sig = v["sig"].as_str().unwrap().to_string();
        let flipped = format!(
            "{}{}",
            if sig.starts_with('0') { '1' } else { '0' },
            &sig[1..]
        );
        v["sig"] = serde_json::Value::String(flipped);
        let forged = format!("Nostr {}", STANDARD.encode(v.to_string()));
        assert_eq!(check(&forged, b"").unwrap_err(), Nip98Error::BadSignature);
    }

    #[test]
    fn wrong_url_method_kind_or_age_is_refused() {
        let k = Keys::generate();
        let other = "https://relay.maxplayer.ai/account/link/confirm";
        assert_eq!(
            check(&sign(&k, other, "POST", None, NOW), b"").unwrap_err(),
            Nip98Error::UrlMismatch
        );
        let evil = "https://evil.example/account/link/request";
        assert_eq!(
            check(&sign(&k, evil, "POST", None, NOW), b"").unwrap_err(),
            Nip98Error::UrlMismatch
        );
        let http = "http://relay.maxplayer.ai/account/link/request";
        assert_eq!(
            check(&sign(&k, http, "POST", None, NOW), b"").unwrap_err(),
            Nip98Error::UrlMismatch
        );
        assert_eq!(
            check(&sign(&k, URL, "GET", None, NOW), b"").unwrap_err(),
            Nip98Error::MethodMismatch
        );
        assert_eq!(
            check(&sign(&k, URL, "POST", None, NOW - 61), b"").unwrap_err(),
            Nip98Error::Stale
        );
        assert_eq!(
            check(&sign(&k, URL, "POST", None, NOW + 61), b"").unwrap_err(),
            Nip98Error::Stale
        );
        let wrong_kind = EventBuilder::new(Kind::TextNote, "")
            .tags([
                Tag::custom(u_kind(), [URL]),
                Tag::custom(TagKind::Method, ["POST"]),
            ])
            .custom_created_at(Timestamp::from(NOW))
            .sign_with_keys(&k)
            .unwrap();
        let h = format!("Nostr {}", STANDARD.encode(wrong_kind.as_json()));
        assert_eq!(check(&h, b"").unwrap_err(), Nip98Error::WrongKind);
    }

    #[test]
    fn duplicate_u_or_method_tags_are_refused() {
        let k = Keys::generate();
        let ev = EventBuilder::new(Kind::HttpAuth, "")
            .tags([
                Tag::custom(u_kind(), [URL]),
                Tag::custom(u_kind(), ["https://evil.example/"]),
                Tag::custom(TagKind::Method, ["POST"]),
            ])
            .custom_created_at(Timestamp::from(NOW))
            .sign_with_keys(&k)
            .unwrap();
        let h = format!("Nostr {}", STANDARD.encode(ev.as_json()));
        assert_eq!(check(&h, b"").unwrap_err(), Nip98Error::UrlMismatch);
        let ev = EventBuilder::new(Kind::HttpAuth, "")
            .tags([
                Tag::custom(u_kind(), [URL]),
                Tag::custom(TagKind::Method, ["POST"]),
                Tag::custom(TagKind::Method, ["GET"]),
            ])
            .custom_created_at(Timestamp::from(NOW))
            .sign_with_keys(&k)
            .unwrap();
        let h = format!("Nostr {}", STANDARD.encode(ev.as_json()));
        assert_eq!(check(&h, b"").unwrap_err(), Nip98Error::MethodMismatch);
    }

    #[test]
    fn body_must_be_bound_by_payload() {
        let k = Keys::generate();
        let body = br#"{"code":"123456"}"#;
        assert_eq!(
            check(&sign(&k, URL, "POST", None, NOW), body).unwrap_err(),
            Nip98Error::PayloadMissing
        );
        let h = sign(&k, URL, "POST", Some(br#"{"code":"000000"}"#), NOW);
        assert_eq!(check(&h, body).unwrap_err(), Nip98Error::PayloadMismatch);
        // An empty body needs no payload tag (GET /account/status).
        assert!(
            verify(
                Some(&sign(&k, URL, "GET", None, NOW)),
                URL,
                "GET",
                b"",
                NOW,
                60
            )
            .is_ok()
        );
    }
}
