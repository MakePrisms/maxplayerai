//! NIP-98 HTTP Auth verification (kind:27235).
//!
//! NIP-98 is the standard Nostr HTTP Auth pattern used by Nostr.build, Blossom, and
//! other Nostr HTTP services. It is **stateless** — no WebSocket session required.
//!
//! The client signs a short-lived kind:27235 event containing the target URL, HTTP method,
//! and an optional SHA-256 hash of the request body, then sends it as:
//!
//! ```text
//! Authorization: Nostr <base64(JSON-serialized-event)>
//! ```
//!
//! ## Verification steps
//!
//! 1. Parse JSON into a `nostr::Event`
//! 2. Verify `kind == 27235` (`Kind::HttpAuth`)
//! 3. Verify Schnorr signature via `buzz_core::verify_event`
//! 4. Read the FIRST `["ref", <refname>]` tag and the FIRST `["expiration", <unix>]` tag
//! 5. Verify `created_at` is fresh enough, under [`Nip98Freshness`]
//! 6. Verify `["u", <url>]` tag matches `expected_url` (normalised: case-insensitive
//!    scheme/host, trailing slash stripped)
//! 7. Verify `["method", <method>]` tag matches `expected_method` (case-insensitive)
//! 8. If `["payload", <hash>]` tag is present **and** `body` is `Some`: verify
//!    `SHA-256(body) == hex(payload_tag)`. This prevents body-substitution attacks.
//! 9. Return the pubkey and the ref scope on success.
//!
//! ## Freshness
//!
//! The default is the unconditional ±60 s window: [`verify_nip98_event`] keeps it, so
//! every caller that does not opt in behaves as before.
//!
//! A caller MAY opt in to a longer life for a **scoped** token with
//! [`Nip98Freshness::with_scoped_lifetime_cap`]. A token is scoped when its first `ref`
//! tag holds a valid fully-qualified ref name ([`is_valid_ref_name`]). The git transport
//! opts in because it mints a push token on the host minutes before the push runs; the
//! ref scope bounds the token to one branch of one repo, so a longer life adds almost no
//! blast radius. See `docs/superpowers/briefs/2026-08-31-relay-scoped-token-lifetime.md`.

use nostr::{Alphabet, Event, Kind, SingleLetterTag, TagKind, Timestamp};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::AuthError;

const TIMESTAMP_TOLERANCE_SECS: u64 = 60;

/// Default cap on the life of a scoped NIP-98 token: 6 hours.
///
/// The cap must stay at or above the longest job deadline the marketplace allows,
/// because the seller mints one token before the job starts and pushes with it at the
/// end. The relay advertises the effective value as the NIP-11 `limitation` field
/// `scoped_token_max_lifetime_secs`, so a client can refuse an over-cap token at mint
/// time instead of meeting a 403 mid-delivery.
pub const DEFAULT_SCOPED_TOKEN_MAX_LIFETIME_SECS: u64 = 21_600;

/// How old a NIP-98 event may be.
///
/// [`STRICT`](Self::STRICT) is the unconditional ±60 s window and the default. A caller
/// that grants scoped tokens a longer life passes
/// [`with_scoped_lifetime_cap`](Self::with_scoped_lifetime_cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nip98Freshness {
    /// `Some(cap)` grants a scoped token that carries an `expiration` tag a life of up
    /// to `cap` seconds. `None` applies the ±60 s window to every token.
    scoped_token_max_lifetime_secs: Option<u64>,
}

impl Nip98Freshness {
    /// The ±60 s window for every token, scoped or not. This is the historical rule.
    pub const STRICT: Self = Self {
        scoped_token_max_lifetime_secs: None,
    };

    /// Grant a scoped token that carries a NIP-40 `expiration` tag a life of up to
    /// `cap_secs` seconds. Unscoped tokens keep the ±60 s window.
    ///
    /// `cap_secs == 0` returns [`STRICT`](Self::STRICT) itself, so a zero cap is the exact
    /// historical rule and nothing more. This is the operator's rollback switch: a cap of 0
    /// must behave as though this feature never shipped. `Some(0)` would not do that — it
    /// keeps a token with an `expiration` tag on the scoped path, where an unparseable
    /// tag, an `expiration` that equals `created_at`, or a malformed `ref` tag all refuse a
    /// request that the ±60 s window accepts. Those refusals are safe, but they are not a
    /// rollback.
    pub const fn with_scoped_lifetime_cap(cap_secs: u64) -> Self {
        if cap_secs == 0 {
            return Self::STRICT;
        }
        Self {
            scoped_token_max_lifetime_secs: Some(cap_secs),
        }
    }

    /// The cap in force, or `None` when scoped tokens get no relaxation.
    pub const fn scoped_lifetime_cap(&self) -> Option<u64> {
        self.scoped_token_max_lifetime_secs
    }
}

impl Default for Nip98Freshness {
    fn default() -> Self {
        Self::STRICT
    }
}

/// What NIP-98 verification proved about a signed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nip98Auth {
    /// The authenticated public key.
    pub pubkey: nostr::PublicKey,
    /// The single ref this token may write, from the FIRST `["ref", <refname>]` tag on
    /// the signed event, and only when that name is valid ([`is_valid_ref_name`]).
    ///
    /// This is the value the freshness decision above used. A caller that enforces the
    /// scope MUST use this field and MUST NOT re-read the event, so one crafted event
    /// can never pass the freshness check under one ref and the scope check under
    /// another (brief §7(c)).
    pub ref_scope: Option<String>,
}

/// Structural validation for a fully-qualified git ref name.
///
/// One definition serves two readers: the freshness rule here, which decides whether a
/// token counts as scoped, and the pre-receive policy endpoint in `buzz-relay`, which
/// enforces the scope on a push. They must never drift apart — a scope that would be
/// rejected as a ref name must not be accepted as a scope, and a scope that buys a long
/// life must be a scope the push path will enforce.
pub fn is_valid_ref_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name.starts_with("refs/")
        && !name.contains("..")
        && !name.bytes().any(|b| b <= 0x20 || b == 0x7f)
}

/// Verify a NIP-98 HTTP Auth event (kind:27235).
///
/// # Parameters
///
/// - `event_json` — the raw JSON string of the Nostr event (decoded from base64 by the caller).
/// - `expected_url` — the canonical URL of the request being authenticated.
///   For reverse-proxy deployments, reconstruct from `X-Forwarded-Proto` / `X-Forwarded-Host`
///   before passing here.
/// - `expected_method` — the HTTP method (e.g. `"POST"`). Compared case-insensitively.
/// - `body` — raw request body bytes. If `Some` and a `payload` tag is present in the event,
///   the SHA-256 hash of `body` must match the tag value. If `None`, the `payload` tag is
///   ignored (clients SHOULD include it for POST requests, but it is not required).
///
/// # Returns
///
/// The authenticated `nostr::PublicKey` on success.
///
/// # Errors
///
/// Returns [`AuthError::Nip98Invalid`] with a descriptive message for any verification failure.
/// The message is safe for server logs but should not be forwarded verbatim to clients.
pub fn verify_nip98_event(
    event_json: &str,
    expected_url: &str,
    expected_method: &str,
    body: Option<&[u8]>,
) -> Result<nostr::PublicKey, AuthError> {
    verify_nip98_event_with_policy(
        event_json,
        expected_url,
        expected_method,
        body,
        Nip98Freshness::STRICT,
    )
    .map(|auth| auth.pubkey)
}

/// Verify a NIP-98 HTTP Auth event (kind:27235) under an explicit freshness policy.
///
/// Identical to [`verify_nip98_event`] except for two things:
///
/// - `freshness` chooses the age rule. [`Nip98Freshness::STRICT`] is the ±60 s window;
///   [`Nip98Freshness::with_scoped_lifetime_cap`] lets a scoped token that carries a
///   NIP-40 `expiration` tag live until that expiration, up to the cap.
/// - The return value carries the ref scope the freshness rule read, so the caller
///   enforces the same ref the rule saw (brief §7(c)).
///
/// # Errors
///
/// Returns [`AuthError::Nip98Invalid`] with a descriptive message for any verification failure.
pub fn verify_nip98_event_with_policy(
    event_json: &str,
    expected_url: &str,
    expected_method: &str,
    body: Option<&[u8]>,
    freshness: Nip98Freshness,
) -> Result<Nip98Auth, AuthError> {
    // 1. Parse JSON.
    let event: Event = serde_json::from_str(event_json)
        .map_err(|e| AuthError::Nip98Invalid(format!("event JSON parse error: {e}")))?;

    // 2. Verify kind == 27235.
    if event.kind != Kind::HttpAuth {
        return Err(AuthError::Nip98Invalid(format!(
            "expected kind 27235, got {}",
            event.kind.as_u16()
        )));
    }

    // 3. Verify Schnorr signature (also verifies event ID hash).
    buzz_core::verify_event(&event)
        .map_err(|_| AuthError::Nip98Invalid("invalid Schnorr signature".to_string()))?;

    // 4. Read the ref scope and the expiration. AFTER signature verification, never
    // before: the whole value of putting the scope in a tag is that the signature
    // covers it.
    //
    // `Tags::find` returns the FIRST tag of a kind in event order, and this is the ONLY
    // place either tag is read. The scope leaves here inside `Nip98Auth`, so the
    // freshness rule below and the caller's scope enforcement always judge the same ref
    // (brief §7(c)).
    let ref_tag = event
        .tags
        .find(TagKind::custom("ref"))
        .and_then(|t| t.content());
    let expiration_tag = event
        .tags
        .find(TagKind::Expiration)
        .and_then(|t| t.content());

    // 5. Verify created_at is fresh enough under the caller's policy.
    let now = Timestamp::now().as_secs();
    let event_ts = event.created_at.as_secs();
    check_freshness(event_ts, now, ref_tag, expiration_tag, freshness)?;

    // The scope only ever leaves here as a VALID name. Under a cap the freshness rule
    // above already refused an invalid one, so this filter is a no-op there; under
    // `STRICT` it keeps a `ref` tag from reaching a caller that does not enforce scopes.
    let ref_scope = ref_tag
        .filter(|name| is_valid_ref_name(name))
        .map(str::to_owned);

    // 6. Verify `u` tag matches expected_url (normalised).
    // NIP-98 uses the single-letter "u" tag, not the multi-letter "url" tag.
    let u_tag = event
        .tags
        .find(TagKind::SingleLetter(SingleLetterTag::lowercase(
            Alphabet::U,
        )))
        .and_then(|t| t.content())
        .ok_or_else(|| AuthError::Nip98Invalid("missing `u` tag".to_string()))?;

    if normalize_url(u_tag) != normalize_url(expected_url) {
        return Err(AuthError::Nip98Invalid(format!(
            "URL mismatch: event has `{u_tag}`, expected `{expected_url}`"
        )));
    }

    // 7. Verify `method` tag matches expected_method (case-insensitive).
    let method_tag = event
        .tags
        .find(TagKind::Method)
        .and_then(|t| t.content())
        .ok_or_else(|| AuthError::Nip98Invalid("missing `method` tag".to_string()))?;

    if !method_tag.eq_ignore_ascii_case(expected_method) {
        return Err(AuthError::Nip98Invalid(format!(
            "method mismatch: event has `{method_tag}`, expected `{expected_method}`"
        )));
    }

    // 8. If `payload` tag present AND body is Some: verify SHA-256(body) == payload hex.
    let payload_tag = event.tags.find(TagKind::Payload).and_then(|t| t.content());

    if let (Some(payload_hex), Some(body_bytes)) = (payload_tag, body) {
        let computed: [u8; 32] = Sha256::digest(body_bytes).into();
        let computed_hex = hex::encode(computed);
        if computed_hex != payload_hex {
            return Err(AuthError::Nip98Invalid(
                "payload tag SHA-256 mismatch: request body does not match signed hash".to_string(),
            ));
        }
    }

    // 9. Return the authenticated pubkey and the ref scope the freshness rule used.
    Ok(Nip98Auth {
        pubkey: event.pubkey,
        ref_scope,
    })
}

/// Decide whether `created_at` is fresh enough.
///
/// `ref_tag` is the FIRST `ref` tag and `expiration_tag` the FIRST `expiration` tag,
/// both read once by [`verify_nip98_event_with_policy`]. This function never reads the
/// event, so it cannot see a ref the caller's scope enforcement will not see.
///
/// The rule, in order:
///
/// 1. A future-dated token is always rejected: `created_at <= now + 60`. The skew
///    allowance is unchanged, scoped or not.
/// 2. Under a cap, a `ref` tag that is not a valid ref name is REFUSED, not ignored. A
///    caller that grants scoped tokens a longer life is a caller that enforces scopes,
///    so a scope it cannot parse must stop the request. Ignoring it would silently turn
///    a token its holder meant to restrict into an unrestricted one.
/// 3. A token gets the expiration rule only when ALL of these hold: the caller granted a
///    cap, the token is scoped, and the token carries an `expiration` tag. The
///    expiration then REPLACES the upper age bound.
/// 4. Every other token gets the historical ±60 s window. That is the fail-closed path:
///    a scoped token with no explicit, capped expiration buys nothing.
///
/// Under [`Nip98Freshness::STRICT`] only rules 1 and 4 can fire, so a caller that does
/// not opt in sees exactly the historical behaviour whatever tags the event carries.
fn check_freshness(
    created_at: u64,
    now: u64,
    ref_tag: Option<&str>,
    expiration_tag: Option<&str>,
    freshness: Nip98Freshness,
) -> Result<(), AuthError> {
    // 1. Clock skew. Applies to every token, so a scoped token cannot be post-dated to
    // stretch its own life past the cap.
    if created_at.saturating_sub(now) > TIMESTAMP_TOLERANCE_SECS {
        return Err(AuthError::Nip98Invalid(format!(
            "event timestamp is {}s in the future (max {TIMESTAMP_TOLERANCE_SECS}s)",
            created_at - now
        )));
    }

    let Some(cap) = freshness.scoped_lifetime_cap() else {
        return within_window(created_at, now);
    };

    // 2. A scope the relay cannot parse stops the request. The push path would refuse to
    // enforce such a scope, so honouring the token as if it were unscoped would hand a
    // wider credential to a client that asked for a narrower one.
    if let Some(name) = ref_tag {
        if !is_valid_ref_name(name) {
            return Err(AuthError::Nip98Invalid(format!(
                "`ref` tag `{name}` is not a valid fully-qualified ref name"
            )));
        }
    }

    // 3. The scoped expiration rule.
    if let (Some(scope), Some(raw_expiration)) = (ref_tag, expiration_tag) {
        let expiration: u64 = raw_expiration.parse().map_err(|_| {
            AuthError::Nip98Invalid(format!(
                "scoped token has an unparseable `expiration` tag `{raw_expiration}`"
            ))
        })?;
        let lifetime = expiration.checked_sub(created_at).ok_or_else(|| {
            AuthError::Nip98Invalid(
                "scoped token `expiration` precedes its `created_at`".to_string(),
            )
        })?;
        if lifetime > cap {
            return Err(AuthError::Nip98Invalid(format!(
                "scoped token lifetime {lifetime}s exceeds the {cap}s cap (scope `{scope}`)"
            )));
        }
        if now > expiration {
            return Err(AuthError::Nip98Invalid(format!(
                "scoped token expired {}s ago (scope `{scope}`)",
                now - expiration
            )));
        }
        return Ok(());
    }

    // 4. The historical ±60 s window: unscoped tokens, and scoped tokens with no
    // `expiration` tag.
    within_window(created_at, now)
}

/// The historical NIP-98 rule: `created_at` within ±60 s of server time.
fn within_window(created_at: u64, now: u64) -> Result<(), AuthError> {
    let delta = now.abs_diff(created_at);
    if delta > TIMESTAMP_TOLERANCE_SECS {
        return Err(AuthError::Nip98Invalid(format!(
            "event timestamp outside ±{TIMESTAMP_TOLERANCE_SECS}s window (delta: {delta}s)"
        )));
    }
    Ok(())
}

/// Normalize a URL for comparison.
///
/// - Lowercases scheme and host (already done by the `url` crate).
/// - Strips trailing slash from path.
///
/// **No loopback aliasing.** `localhost`, `::1`, and `127.0.0.1` are three
/// distinct hosts here. Under multi-tenant the `u`-tag host is the row-zero
/// community binding (`docs/multi-tenant-conformance.md`, NIP-98 row): if
/// `verify_nip98_event` collapses them, an event signed for `localhost`
/// would pass against a `127.0.0.1`-resolved community (or vice versa) —
/// a host-binding side door. Tests reconstruct `expected_url` from their
/// own bound host, the same shape production does.
fn normalize_url(raw: &str) -> String {
    let mut parsed = match Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return raw.to_lowercase(),
    };
    let path = parsed.path().trim_end_matches('/').to_string();
    parsed.set_path(&path);
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Timestamp};

    const TEST_URL: &str = "https://relay.example.com/api/tokens";
    const TEST_METHOD: &str = "POST";

    fn make_nip98_event(
        keys: &Keys,
        url: &str,
        method: &str,
        payload_hex: Option<&str>,
        created_at: Option<Timestamp>,
    ) -> String {
        use nostr::Tag;

        let mut tags = vec![
            Tag::parse(["u", url]).unwrap(),
            Tag::parse(["method", method]).unwrap(),
        ];
        if let Some(hex) = payload_hex {
            tags.push(Tag::parse(["payload", hex]).unwrap());
        }

        let mut builder = EventBuilder::new(Kind::HttpAuth, "").tags(tags);
        if let Some(ts) = created_at {
            builder = builder.custom_created_at(ts);
        }
        let event = builder.sign_with_keys(keys).expect("sign");
        serde_json::to_string(&event).expect("serialize")
    }

    #[test]
    fn valid_event_returns_pubkey() {
        let keys = Keys::generate();
        let json = make_nip98_event(&keys, TEST_URL, TEST_METHOD, None, None);
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, None);
        assert!(result.is_ok(), "verify failed: {:?}", result.err());
        assert_eq!(result.unwrap(), keys.public_key());
    }

    #[test]
    fn wrong_kind_rejected() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "")
            .tags([])
            .sign_with_keys(&keys)
            .expect("sign");
        let json = serde_json::to_string(&event).unwrap();
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, None);
        assert!(matches!(result, Err(AuthError::Nip98Invalid(_))));
    }

    #[test]
    fn expired_timestamp_rejected() {
        let keys = Keys::generate();
        let old_ts = Timestamp::from(Timestamp::now().as_secs().saturating_sub(120));
        let json = make_nip98_event(&keys, TEST_URL, TEST_METHOD, None, Some(old_ts));
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, None);
        assert!(matches!(result, Err(AuthError::Nip98Invalid(_))));
    }

    #[test]
    fn url_mismatch_rejected() {
        let keys = Keys::generate();
        let json = make_nip98_event(
            &keys,
            "https://other.example.com/api/tokens",
            TEST_METHOD,
            None,
            None,
        );
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, None);
        assert!(matches!(result, Err(AuthError::Nip98Invalid(_))));
    }

    #[test]
    fn method_mismatch_rejected() {
        let keys = Keys::generate();
        let json = make_nip98_event(&keys, TEST_URL, "GET", None, None);
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, None);
        assert!(matches!(result, Err(AuthError::Nip98Invalid(_))));
    }

    #[test]
    fn method_case_insensitive() {
        let keys = Keys::generate();
        let json = make_nip98_event(&keys, TEST_URL, "post", None, None);
        let result = verify_nip98_event(&json, TEST_URL, "POST", None);
        assert!(result.is_ok());
    }

    #[test]
    fn payload_tag_correct_hash_passes() {
        let keys = Keys::generate();
        let body = b"hello world";
        let hash: [u8; 32] = Sha256::digest(body).into();
        let hash_hex = hex::encode(hash);
        let json = make_nip98_event(&keys, TEST_URL, TEST_METHOD, Some(&hash_hex), None);
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, Some(body));
        assert!(result.is_ok());
    }

    #[test]
    fn payload_tag_wrong_hash_rejected() {
        let keys = Keys::generate();
        let body = b"hello world";
        let wrong_hex = "deadbeef".repeat(8); // 64 hex chars but wrong hash
        let json = make_nip98_event(&keys, TEST_URL, TEST_METHOD, Some(&wrong_hex), None);
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, Some(body));
        assert!(matches!(result, Err(AuthError::Nip98Invalid(_))));
    }

    #[test]
    fn payload_tag_absent_with_body_passes() {
        // payload tag is optional per spec; clients SHOULD include it but it's not required
        let keys = Keys::generate();
        let json = make_nip98_event(&keys, TEST_URL, TEST_METHOD, None, None);
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, Some(b"some body"));
        assert!(result.is_ok());
    }

    #[test]
    fn trailing_slash_normalized() {
        let keys = Keys::generate();
        let url_with_slash = "https://relay.example.com/api/tokens/";
        let json = make_nip98_event(&keys, url_with_slash, TEST_METHOD, None, None);
        // expected_url without trailing slash — should still match
        let result = verify_nip98_event(&json, TEST_URL, TEST_METHOD, None);
        assert!(result.is_ok());
    }

    #[test]
    fn loopback_aliases_are_distinct_hosts() {
        // Under multi-tenant, the `u`-tag host is the row-zero community
        // binding. An event signed for `localhost` MUST NOT pass against an
        // expected URL on `127.0.0.1` (or `::1`) — collapsing the three would
        // be a host-check side door. Production reconstructs `expected_url`
        // from the community-bound host; tests do the same.
        let keys = Keys::generate();
        let localhost_url = "http://localhost:3000/api/tokens";
        let loopback_url = "http://127.0.0.1:3000/api/tokens";
        let json = make_nip98_event(&keys, localhost_url, TEST_METHOD, None, None);
        let result = verify_nip98_event(&json, loopback_url, TEST_METHOD, None);
        assert!(
            matches!(result, Err(AuthError::Nip98Invalid(_))),
            "localhost u-tag must NOT match a 127.0.0.1 expected_url; got {result:?}"
        );

        // Symmetric: signed-for-127.0.0.1 against expected localhost — same answer.
        let json2 = make_nip98_event(&keys, loopback_url, TEST_METHOD, None, None);
        let result2 = verify_nip98_event(&json2, localhost_url, TEST_METHOD, None);
        assert!(
            matches!(result2, Err(AuthError::Nip98Invalid(_))),
            "127.0.0.1 u-tag must NOT match a localhost expected_url; got {result2:?}"
        );

        // And identity still holds — same host on both sides verifies.
        let json3 = make_nip98_event(&keys, loopback_url, TEST_METHOD, None, None);
        assert!(verify_nip98_event(&json3, loopback_url, TEST_METHOD, None).is_ok());
    }

    // ---------------------------------------------------------------------
    // Scope-conditional lifetime
    // (docs/superpowers/briefs/2026-08-31-relay-scoped-token-lifetime.md §5)
    // ---------------------------------------------------------------------

    const SCOPE: &str = "refs/heads/maxplayer/contribution/job-1";
    const CAP: u64 = DEFAULT_SCOPED_TOKEN_MAX_LIFETIME_SECS;

    /// Sign a NIP-98 event with arbitrary extra tags and an explicit `created_at`.
    fn make_event_with_tags(keys: &Keys, created_at: u64, extra_tags: &[[&str; 2]]) -> String {
        use nostr::Tag;

        let mut tags = vec![
            Tag::parse(["u", TEST_URL]).unwrap(),
            Tag::parse(["method", TEST_METHOD]).unwrap(),
        ];
        for tag in extra_tags {
            tags.push(Tag::parse(*tag).unwrap());
        }
        let event = EventBuilder::new(Kind::HttpAuth, "")
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("sign");
        serde_json::to_string(&event).expect("serialize")
    }

    /// `expect_err`, but yields the message so a test can match on it.
    trait UnwrapErrMsg {
        fn unwrap_err_or_else_msg(self, why: &str) -> String;
    }

    impl UnwrapErrMsg for Result<Nip98Auth, AuthError> {
        fn unwrap_err_or_else_msg(self, why: &str) -> String {
            match self {
                Ok(auth) => panic!("{why}; instead it verified as {auth:?}"),
                Err(e) => e.to_string(),
            }
        }
    }

    fn verify_scoped(json: &str) -> Result<Nip98Auth, AuthError> {
        verify_nip98_event_with_policy(
            json,
            TEST_URL,
            TEST_METHOD,
            None,
            Nip98Freshness::with_scoped_lifetime_cap(CAP),
        )
    }

    /// §5: scoped, 10 minutes old, expiration in the future → accepted.
    #[test]
    fn scoped_token_ten_minutes_old_with_a_future_expiration_verifies() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let created_at = now - 600;
        let json = make_event_with_tags(
            &keys,
            created_at,
            &[
                ["ref", SCOPE],
                ["expiration", &(created_at + 3600).to_string()],
            ],
        );
        let auth = verify_scoped(&json).expect("a scoped, unexpired token must verify");
        assert_eq!(auth.pubkey, keys.public_key());
        assert_eq!(auth.ref_scope.as_deref(), Some(SCOPE));
    }

    /// §5: scoped, 10 minutes old, NO expiration tag → rejected. Fail closed: a scope
    /// alone buys nothing, only a scope plus an explicit capped expiration does.
    #[test]
    fn scoped_token_ten_minutes_old_without_an_expiration_is_rejected() {
        let keys = Keys::generate();
        let created_at = Timestamp::now().as_secs() - 600;
        let json = make_event_with_tags(&keys, created_at, &[["ref", SCOPE]]);
        let err = verify_scoped(&json).expect_err("no expiration tag ⇒ the ±60s window");
        assert!(
            err.to_string().contains("±60s window"),
            "expected the window rejection, got {err}"
        );
    }

    /// §5: scoped, `expiration - created_at` over the cap → rejected outright. The
    /// client must refuse at mint time; the relay refuses loudly rather than granting a
    /// shorter life than the token asks for.
    #[test]
    fn scoped_token_over_the_lifetime_cap_is_rejected() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let json = make_event_with_tags(
            &keys,
            now,
            &[["ref", SCOPE], ["expiration", &(now + CAP + 1).to_string()]],
        );
        let err = verify_scoped(&json).expect_err("an over-cap lifetime must be rejected");
        assert!(
            err.to_string().contains("exceeds the"),
            "expected the cap rejection, got {err}"
        );

        // Exactly at the cap is still accepted — the bound is inclusive.
        let at_cap = make_event_with_tags(
            &keys,
            now,
            &[["ref", SCOPE], ["expiration", &(now + CAP).to_string()]],
        );
        assert!(
            verify_scoped(&at_cap).is_ok(),
            "the cap itself must be allowed"
        );
    }

    /// §5: scoped, now past the expiration → rejected.
    #[test]
    fn scoped_token_past_its_expiration_is_rejected() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let created_at = now - 600;
        let json = make_event_with_tags(
            &keys,
            created_at,
            &[["ref", SCOPE], ["expiration", &(now - 30).to_string()]],
        );
        let err = verify_scoped(&json).expect_err("an expired token must be rejected");
        assert!(
            err.to_string().contains("expired"),
            "expected the expiration rejection, got {err}"
        );
    }

    /// §5: unscoped, 2 minutes old → rejected, even with an `expiration` tag and the cap
    /// granted. The scope is what unlocks the longer life; nothing else does.
    #[test]
    fn unscoped_token_two_minutes_old_is_rejected_even_with_an_expiration() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let created_at = now - 120;
        let json = make_event_with_tags(
            &keys,
            created_at,
            &[["expiration", &(created_at + 3600).to_string()]],
        );
        let err = verify_scoped(&json).expect_err("an unscoped token keeps the ±60s window");
        assert!(
            err.to_string().contains("±60s window"),
            "expected the window rejection, got {err}"
        );
    }

    /// A `ref` tag that is not a valid fully-qualified ref name is REFUSED under a cap,
    /// not quietly ignored. Ignoring it would turn a token whose holder asked for one
    /// branch into a token that may write any branch — the exact credential widening the
    /// scope exists to prevent — because the push path refuses to enforce a scope it
    /// cannot parse.
    #[test]
    fn an_invalid_ref_name_is_refused_not_ignored() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        for bad in [
            "heads/main",
            "refs/heads/../../etc/passwd",
            "",
            "refs/heads/a b",
        ] {
            // Fresh, so the ±60 s window would have accepted it. Only the ref tag can
            // be the reason it fails.
            let json = make_event_with_tags(&keys, now, &[["ref", bad]]);
            let err = verify_scoped(&json)
                .unwrap_err_or_else_msg("an invalid ref tag must stop the request");
            assert!(
                err.contains("not a valid fully-qualified ref name"),
                "ref `{bad}` must be refused by name, got {err}"
            );

            // A non-git caller is untouched: STRICT never looks at the tag, so the same
            // event verifies on its timestamp alone.
            assert!(
                verify_nip98_event(&json, TEST_URL, TEST_METHOD, None).is_ok(),
                "STRICT must ignore the ref tag entirely (ref `{bad}`)"
            );
        }
    }

    /// §5: a future-dated token is rejected whether it is scoped or not. Without this a
    /// scoped token could post-date itself and stretch its own life past the cap.
    #[test]
    fn a_future_dated_token_is_rejected_scoped_or_not() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let created_at = now + 3600;

        let scoped = make_event_with_tags(
            &keys,
            created_at,
            &[
                ["ref", SCOPE],
                ["expiration", &(created_at + 60).to_string()],
            ],
        );
        let err = verify_scoped(&scoped).expect_err("a future-dated scoped token is rejected");
        assert!(
            err.to_string().contains("in the future"),
            "expected the skew rejection, got {err}"
        );

        let unscoped = make_event_with_tags(&keys, created_at, &[]);
        assert!(
            verify_nip98_event(&unscoped, TEST_URL, TEST_METHOD, None).is_err(),
            "a future-dated unscoped token is rejected"
        );
    }

    /// §7(c): ONE tag, read once. A crafted event with two `ref` tags must be judged
    /// entirely by the FIRST one — the freshness decision and the scope the caller
    /// enforces are the same value, so the event cannot pass one check under one ref and
    /// the other check under a different ref.
    #[test]
    fn the_first_ref_tag_decides_both_the_freshness_and_the_returned_scope() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let created_at = now - 600;
        let expiration = (created_at + 3600).to_string();

        // First ref valid → long life granted, and the scope handed back is the FIRST.
        let json = make_event_with_tags(
            &keys,
            created_at,
            &[
                ["ref", SCOPE],
                ["ref", "refs/heads/main"],
                ["expiration", &expiration],
            ],
        );
        let auth = verify_scoped(&json).expect("the first ref scopes the token");
        assert_eq!(
            auth.ref_scope.as_deref(),
            Some(SCOPE),
            "the returned scope must be the FIRST ref tag, not a later one"
        );

        // First ref INVALID → the request is refused on that name. The trailing valid ref
        // must not rescue it: if either check re-scanned the tags it would find a valid
        // scope and grant this 10-minute-old token a long life.
        let crafted = make_event_with_tags(
            &keys,
            created_at,
            &[
                ["ref", "heads/main"],
                ["ref", SCOPE],
                ["expiration", &expiration],
            ],
        );
        let err = verify_scoped(&crafted)
            .unwrap_err_or_else_msg("a trailing ref tag must not scope the token");
        assert!(
            err.contains("heads/main"),
            "the FIRST ref tag must be the one judged; got {err}"
        );
        assert!(
            !err.contains(SCOPE),
            "the trailing ref tag must never be read; got {err}"
        );
    }

    /// The public [`verify_nip98_event`] entry point — the one every non-git caller uses
    /// — stays on the ±60 s window, scope and expiration tags notwithstanding.
    #[test]
    fn the_strict_entry_point_ignores_the_scope_and_the_expiration() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let created_at = now - 600;
        let json = make_event_with_tags(
            &keys,
            created_at,
            &[
                ["ref", SCOPE],
                ["expiration", &(created_at + 3600).to_string()],
            ],
        );

        let err = verify_nip98_event(&json, TEST_URL, TEST_METHOD, None)
            .expect_err("verify_nip98_event must keep the ±60s window");
        assert!(
            err.to_string().contains("±60s window"),
            "expected the window rejection, got {err}"
        );

        // Same event, same policy, spelled out: STRICT is what the wrapper passes.
        assert!(verify_nip98_event_with_policy(
            &json,
            TEST_URL,
            TEST_METHOD,
            None,
            Nip98Freshness::STRICT,
        )
        .is_err());
        assert_eq!(Nip98Freshness::default(), Nip98Freshness::STRICT);
        assert_eq!(Nip98Freshness::STRICT.scoped_lifetime_cap(), None);
    }

    /// A scoped token inside the window verifies under both policies, so the relaxation
    /// only ever adds acceptances — it never rejects what today accepts.
    #[test]
    fn a_fresh_scoped_token_verifies_under_both_policies() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let json = make_event_with_tags(
            &keys,
            now,
            &[["ref", SCOPE], ["expiration", &(now + 3600).to_string()]],
        );
        assert!(verify_nip98_event(&json, TEST_URL, TEST_METHOD, None).is_ok());
        let auth = verify_scoped(&json).expect("fresh and scoped verifies");
        assert_eq!(auth.ref_scope.as_deref(), Some(SCOPE));
    }

    /// A malformed or backwards `expiration` on a scoped token is refused rather than
    /// silently treated as absent.
    #[test]
    fn a_nonsense_expiration_on_a_scoped_token_is_refused() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();

        let unparseable =
            make_event_with_tags(&keys, now, &[["ref", SCOPE], ["expiration", "soon"]]);
        let err = verify_scoped(&unparseable).expect_err("an unparseable expiration is refused");
        assert!(
            err.to_string().contains("unparseable"),
            "expected the parse rejection, got {err}"
        );

        let backwards = make_event_with_tags(
            &keys,
            now,
            &[["ref", SCOPE], ["expiration", &(now - 10).to_string()]],
        );
        let err = verify_scoped(&backwards).expect_err("expiration before created_at is refused");
        assert!(
            err.to_string().contains("precedes"),
            "expected the ordering rejection, got {err}"
        );
    }

    /// The cap is a plain number of seconds and the default is 6 hours, which is what the
    /// relay advertises as `scoped_token_max_lifetime_secs`.
    #[test]
    fn the_default_cap_is_six_hours() {
        assert_eq!(DEFAULT_SCOPED_TOKEN_MAX_LIFETIME_SECS, 21_600);
        assert_eq!(
            Nip98Freshness::with_scoped_lifetime_cap(99).scoped_lifetime_cap(),
            Some(99)
        );
    }

    /// A zero cap is the operator's rollback switch, so it must be STRICT itself and not a
    /// third behaviour. `Some(0)` used to keep a token that carries an `expiration` tag on
    /// the scoped path, where three inputs are refused that the ±60 s window accepts: an
    /// `expiration` equal to `created_at` once the token ages, an unparseable `expiration`,
    /// and a malformed `ref` tag. Each case below is fresh and unremarkable under STRICT, so
    /// each one fails if a zero cap ever stops meaning "off".
    #[test]
    fn a_zero_cap_is_exactly_strict_for_a_scoped_token_with_an_expiration() {
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        let off = Nip98Freshness::with_scoped_lifetime_cap(0);

        assert_eq!(
            off,
            Nip98Freshness::STRICT,
            "a zero cap IS the strict policy"
        );
        assert_eq!(off.scoped_lifetime_cap(), None, "no cap is in force");

        // 30 s old, `expiration` == `created_at`: inside the window, and the scoped path
        // would call it expired.
        let stale_expiry = make_event_with_tags(
            &keys,
            now - 30,
            &[["ref", SCOPE], ["expiration", &(now - 30).to_string()]],
        );
        // An `expiration` the scoped path cannot parse. STRICT never reads the tag.
        let junk_expiry =
            make_event_with_tags(&keys, now, &[["ref", SCOPE], ["expiration", "tomorrow"]]);
        // A `ref` tag the scoped path refuses. STRICT never reads it, and the push path
        // enforces nothing from an unscoped token.
        let bad_scope = make_event_with_tags(
            &keys,
            now,
            &[
                ["ref", "refs/heads/../../etc/passwd"],
                ["expiration", &(now + 3600).to_string()],
            ],
        );

        for (json, case) in [
            (&stale_expiry, "expiration equal to created_at"),
            (&junk_expiry, "unparseable expiration"),
            (&bad_scope, "malformed ref tag"),
        ] {
            let capped = verify_nip98_event_with_policy(json, TEST_URL, TEST_METHOD, None, off);
            let strict = verify_nip98_event_with_policy(
                json,
                TEST_URL,
                TEST_METHOD,
                None,
                Nip98Freshness::STRICT,
            );
            assert!(
                strict.is_ok(),
                "control: STRICT must accept the {case} case; got {:?}",
                strict.err()
            );
            assert!(
                capped.is_ok(),
                "a zero cap must accept the {case} case exactly as STRICT does; got {:?}",
                capped.err()
            );
        }
    }

    #[test]
    fn ref_names_are_validated_strictly() {
        assert!(is_valid_ref_name("refs/heads/maxplayer/contribution/job-1"));
        assert!(!is_valid_ref_name(""), "empty");
        assert!(!is_valid_ref_name("heads/main"), "must be fully qualified");
        assert!(
            !is_valid_ref_name("refs/heads/../../../etc/passwd"),
            "dot-dot"
        );
        assert!(!is_valid_ref_name("refs/heads/a b"), "space");
        assert!(!is_valid_ref_name("refs/heads/a\nb"), "newline");
        assert!(
            !is_valid_ref_name(&format!("refs/heads/{}", "a".repeat(300))),
            "over 256 bytes"
        );
    }
}
