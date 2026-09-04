//! The relay's advertised scoped-token policy, read from its NIP-11 information document.
//!
//! ## Why this module exists
//!
//! `[sandbox] container_delivery_token = "long-lived"` mints ONE branch-scoped push token before the
//! job container starts and gives it a NIP-40 `expiration` tag. That only works on a relay that
//! HONOURS the expiration tag for a scoped token (relay Requirement B). A relay that ignores the tag
//! applies its plain ±60 s age window instead, so the token is stale long before the push — and the
//! push is the LAST step of a paid job, after the agent already ran and the buyer already paid. The
//! seat learns the answer as an HTTP 401 at the most expensive point of the job.
//!
//! Measured on the deployed relay on 2026-09-03 (`tests/relay_canary.rs`, part B): the ref scope IS
//! enforced, and the expiration tag is NOT honoured. An aged scoped token with a future expiration
//! got HTTP 401. So `long-lived` is unusable there today, and a seat must be told at BOOT rather
//! than mid-delivery.
//!
//! ## The contract
//!
//! A relay that implements Requirement B advertises the cap it applies in its NIP-11 `limitation`
//! object, as [`SCOPED_TOKEN_CAP_FIELD`]. The field's ABSENCE is the signal that the relay does not
//! implement Requirement B — there is no second way to ask, so absence is read as "not supported"
//! and never as "probably fine".
//!
//! Everything here fails CLOSED for `long-lived`: absent, too small, unreachable and unparseable all
//! refuse. Nothing here changes `fresh-after-agent`, which is the default and needs no relay feature.

use std::fmt;

/// The NIP-11 `limitation` field a relay sets when it honours the NIP-40 `expiration` tag of a
/// branch-scoped push token (relay Requirement B). Its value is the longest scoped-token lifetime
/// the relay accepts, in seconds.
pub const SCOPED_TOKEN_CAP_FIELD: &str = "scoped_token_max_lifetime_secs";

/// The `Accept` header NIP-11 defines for the relay information document.
pub const NIP11_ACCEPT: &str = "application/nostr+json";

/// A NIP-11 read is one small JSON document; anything larger is not the document we asked for.
/// Bounds the boot gate's memory against a relay (or a middlebox) that answers with a stream.
const NIP11_MAX_BYTES: usize = 64 * 1024;

/// What ONE relay says about the lifetime of a branch-scoped push token.
///
/// Three states, not two: "the relay says 6 h", "the relay says nothing" and "nobody could ask" are
/// different facts, and reporting the third as the second would tell an operator the relay lacks a
/// feature nobody measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedTokenSupport {
    /// The relay advertises `limitation.`[`SCOPED_TOKEN_CAP_FIELD`], in seconds.
    Advertised(u64),
    /// The document parsed and the field is not there — the relay does not implement Requirement B.
    Absent,
    /// The relay could not be asked, or its answer could not be read. Never read as support.
    Unknown(String),
}

impl ScopedTokenSupport {
    /// The advertised cap, or `None` for both the absent and the unknown state.
    pub fn advertised_secs(&self) -> Option<u64> {
        match self {
            Self::Advertised(secs) => Some(*secs),
            Self::Absent | Self::Unknown(_) => None,
        }
    }
}

impl fmt::Display for ScopedTokenSupport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Advertised(secs) => {
                write!(
                    formatter,
                    "{SCOPED_TOKEN_CAP_FIELD}={secs} (relay honours the expiration tag)"
                )
            }
            Self::Absent => write!(
                formatter,
                "no {SCOPED_TOKEN_CAP_FIELD} in the relay's NIP-11 limitation object"
            ),
            Self::Unknown(reason) => write!(
                formatter,
                "could not read the relay's NIP-11 document: {reason}"
            ),
        }
    }
}

/// The https origin whose NIP-11 document answers the scoped-token question.
///
/// NIP-11 serves the document at the relay's own origin, so any path is dropped: a relay-git remote
/// is `https://<relay>/git/<owner>/<repo>.git`, and the document lives at `https://<relay>/`.
///
/// `wss` maps to `https` and `ws` maps to `http`, which is the standard NIP-11 mapping. A plaintext
/// origin is accepted because a local test relay is served that way; its answer is then as
/// trustworthy as the transport, which is the operator's own choice of relay.
pub fn nip11_origin(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    let (scheme_raw, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| format!("relay url has no scheme: {trimmed:?}"))?;
    let scheme = match scheme_raw.to_ascii_lowercase().as_str() {
        "wss" | "https" => "https",
        "ws" | "http" => "http",
        other => {
            return Err(format!(
                "relay url scheme {other:?} carries no NIP-11 document (expected ws, wss, http or https)"
            ));
        }
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("relay url is missing a host: {trimmed:?}"));
    }
    // Userinfo in a relay url is refused rather than forwarded: the same rule the delivery transport
    // allowlist applies, and it keeps a credential out of a boot-log url.
    if authority.contains('@') {
        return Err("relay url must not embed credentials".to_owned());
    }
    Ok(format!("{scheme}://{authority}/"))
}

/// The origin to ask about a seat's push tokens: the relay-git REMOTE when the seat pushes there,
/// else the configured relay url.
///
/// The server that checks the token is the git endpoint, not the event relay, so the remote is the
/// authority whenever it is a relay-git locator. The two share a host on the shipped default
/// (`DEFAULT_RELAY_GIT_BASE` is the relay host plus `/git`), and they need not on a custom seat.
pub fn scoped_token_authority(relay_url: &str, git_remote: &str) -> Result<String, String> {
    if crate::delivery_transport::is_relay_git_locator(git_remote) {
        nip11_origin(git_remote)
    } else {
        nip11_origin(relay_url)
    }
}

/// Read [`SCOPED_TOKEN_CAP_FIELD`] out of a NIP-11 document.
///
/// A document that is not a JSON object, and a field that is not a positive whole number of seconds,
/// both give [`ScopedTokenSupport::Unknown`] — we could not read the answer, which is not the same
/// fact as the relay not having one. A missing `limitation`, or a `limitation` without the field,
/// gives [`ScopedTokenSupport::Absent`].
pub fn parse_scoped_token_cap(document: &str) -> ScopedTokenSupport {
    let value: serde_json::Value = match serde_json::from_str(document) {
        Ok(value) => value,
        Err(error) => return ScopedTokenSupport::Unknown(format!("not JSON ({error})")),
    };
    let Some(object) = value.as_object() else {
        return ScopedTokenSupport::Unknown("NIP-11 document is not a JSON object".to_owned());
    };
    let Some(limitation) = object.get("limitation") else {
        return ScopedTokenSupport::Absent;
    };
    let Some(limitation) = limitation.as_object() else {
        return ScopedTokenSupport::Unknown("NIP-11 limitation is not a JSON object".to_owned());
    };
    let Some(field) = limitation.get(SCOPED_TOKEN_CAP_FIELD) else {
        return ScopedTokenSupport::Absent;
    };
    // An explicit `null` is the same fact as a missing key: the relay named no cap.
    if field.is_null() {
        return ScopedTokenSupport::Absent;
    }
    match field.as_u64() {
        Some(0) => ScopedTokenSupport::Unknown(format!("{SCOPED_TOKEN_CAP_FIELD} is 0")),
        Some(secs) => ScopedTokenSupport::Advertised(secs),
        None => ScopedTokenSupport::Unknown(format!(
            "{SCOPED_TOKEN_CAP_FIELD} is not a positive whole number of seconds ({field})"
        )),
    }
}

/// The boot verdict for `[sandbox] container_delivery_token = "long-lived"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LongLivedVerdict {
    /// The relay advertises a cap at least as large as the seat's configured cap.
    Supported {
        /// The relay's advertised cap, in seconds.
        advertised_secs: u64,
    },
    /// The relay does not advertise the field, so it does not implement Requirement B.
    NotAdvertised,
    /// The relay advertises a cap SMALLER than the seat's configured cap, so the seat would mint
    /// tokens the relay refuses.
    CapTooSmall {
        /// The relay's advertised cap, in seconds.
        advertised_secs: u64,
        /// The seat's `container_delivery_token_cap_secs`, in seconds.
        configured_secs: u64,
    },
    /// The relay could not be asked, or its answer could not be read.
    Unknown(String),
}

impl LongLivedVerdict {
    /// True only for [`Self::Supported`]. Every other state refuses.
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported { .. })
    }

    /// WHAT WAS MEASURED, with no remedy attached — `None` when the mode is safe to use.
    ///
    /// Split from [`Self::refusal`] because the two consumers render differently: the boot gate
    /// prints one sentence that must carry its own way forward, while a `maxplayer doctor` row keeps
    /// the finding and the fix in separate columns.
    pub fn measured(&self) -> Option<String> {
        let detail = match self {
            Self::Supported { .. } => return None,
            Self::NotAdvertised => format!(
                "the relay does not advertise {SCOPED_TOKEN_CAP_FIELD} in its NIP-11 limitation \
                 object, so it does not honour the NIP-40 expiration tag of a branch-scoped push \
                 token. The token would be stale at push time and the push would fail with HTTP 401 \
                 AFTER the agent ran and the buyer paid"
            ),
            Self::CapTooSmall {
                advertised_secs,
                configured_secs,
            } => format!(
                "the relay advertises {SCOPED_TOKEN_CAP_FIELD}={advertised_secs} s but this seat is \
                 configured for container_delivery_token_cap_secs={configured_secs} s, so this seat \
                 would mint tokens the relay refuses"
            ),
            Self::Unknown(reason) => format!(
                "the relay's scoped-token policy could not be read ({reason}), so nothing proves it \
                 honours the NIP-40 expiration tag of a branch-scoped push token"
            ),
        };
        Some(detail)
    }

    /// The remedy every refusal carries: the mode that works with the relay as deployed today, and
    /// the command that measures the relay for yourself. A refusal with no way forward is a refusal
    /// an operator works around by guessing.
    pub const FIX: &'static str = "set [sandbox] container_delivery_token = \"fresh-after-agent\" \
         (the default), which mints a fresh 60 s token after the agent exits and works with the relay \
         as deployed today. To measure the relay yourself, run the canary: `cargo test -p \
         maxplayer-core --features git-delivery --test relay_canary -- --ignored --nocapture`";

    /// The one-sentence boot refusal: the mode, what was measured, and the way forward. `None` when
    /// the mode is safe to use.
    pub fn refusal(&self) -> Option<String> {
        let detail = self.measured()?;
        Some(format!(
            "[sandbox] container_delivery_token = \"long-lived\" is refused: {detail}. {}",
            Self::FIX
        ))
    }
}

/// Fold a relay's answer and the seat's configured cap into the boot verdict. Pure — the caller owns
/// the fetch, so every arm is testable with no network.
pub fn long_lived_verdict(
    support: &ScopedTokenSupport,
    configured_cap_secs: u64,
) -> LongLivedVerdict {
    match support {
        ScopedTokenSupport::Advertised(advertised) if *advertised >= configured_cap_secs => {
            LongLivedVerdict::Supported {
                advertised_secs: *advertised,
            }
        }
        ScopedTokenSupport::Advertised(advertised) => LongLivedVerdict::CapTooSmall {
            advertised_secs: *advertised,
            configured_secs: configured_cap_secs,
        },
        ScopedTokenSupport::Absent => LongLivedVerdict::NotAdvertised,
        ScopedTokenSupport::Unknown(reason) => LongLivedVerdict::Unknown(reason.clone()),
    }
}

/// GET the NIP-11 document at `origin` and read the scoped-token cap out of it.
///
/// ONE request to `origin` and NO redirects. Every failure — DNS, TLS, timeout, any non-200 status
/// including a 3xx, a body that is not the document — comes back as
/// [`ScopedTokenSupport::Unknown`]. The caller decides what an unknown answer means; for
/// `long-lived` it means refuse.
///
/// # Why redirects are refused
///
/// This answer decides whether the seat may mint a token that stays valid for hours. It is only
/// worth anything if it came from the SAME origin the push goes to. A followed redirect breaks
/// exactly that: `origin` could answer `302 Location: https://elsewhere.example`, and a document
/// there advertising `scoped_token_max_lifetime_secs` would pass this gate while the real git relay
/// honours no such thing. The seat would then mint long-lived tokens and meet HTTP 401 at push
/// time, at the end of a paid run — the failure this gate exists to prevent.
///
/// A redirect is not expected here: NIP-11 is served at the relay's own origin. So refusing one
/// costs nothing, and a relay that does redirect reads as `Unknown`, which fails closed in
/// `long-lived` mode instead of trusting a stranger's answer.
///
/// Gated on `git-delivery` because that is the feature that carries `reqwest` (and it is implied by
/// `wallet`, which both callers — the seller boot gate and the doctor row — already require).
#[cfg(feature = "git-delivery")]
pub async fn fetch_scoped_token_support(
    origin: &str,
    timeout: std::time::Duration,
) -> ScopedTokenSupport {
    let client = match reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        // No redirects: see the note above. reqwest's DEFAULT follows up to 10, which would let
        // another host answer for this one, so this is not a default worth inheriting.
        .redirect(reqwest::redirect::Policy::none())
        // Mirrors the push leg's own switch (`git_transport`): a self-signed test relay whose push
        // this seat already trusts must not be refused only on the NIP-11 read.
        .danger_accept_invalid_certs(std::env::var_os("GIT_SSL_NO_VERIFY").is_some())
        .build()
    {
        Ok(client) => client,
        Err(error) => return ScopedTokenSupport::Unknown(format!("http client: {error}")),
    };
    let response = match client
        .get(origin)
        .header(reqwest::header::ACCEPT, NIP11_ACCEPT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return ScopedTokenSupport::Unknown(format!("GET {origin} failed: {error}")),
    };
    let status = response.status();
    // A redirect reaches here as a 3xx, because the client follows none. Name it, so an operator
    // reading the doctor row learns the relay redirected rather than that it "answered HTTP 302".
    if status.is_redirection() {
        return ScopedTokenSupport::Unknown(format!(
            "GET {origin} answered HTTP {} with a redirect; the NIP-11 answer must come from the \
             same origin as the push, so it is not followed",
            status.as_u16()
        ));
    }
    if !status.is_success() {
        return ScopedTokenSupport::Unknown(format!(
            "GET {origin} answered HTTP {}",
            status.as_u16()
        ));
    }
    // Declared length first, then the stream itself, and BOTH are checked. A declared over-cap
    // length is refused before a byte is read; a relay that declares nothing (or lies) is stopped by
    // the accumulating cap below, so the boot gate can never be made to buffer an unbounded body.
    if response
        .content_length()
        .is_some_and(|len| len > NIP11_MAX_BYTES as u64)
    {
        return ScopedTokenSupport::Unknown(format!(
            "GET {origin} declared {} bytes, more than the {NIP11_MAX_BYTES}-byte NIP-11 limit",
            response.content_length().unwrap_or_default()
        ));
    }
    let mut body: Vec<u8> = Vec::new();
    let mut response = response;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > NIP11_MAX_BYTES {
                    return ScopedTokenSupport::Unknown(format!(
                        "GET {origin} answered more than the {NIP11_MAX_BYTES}-byte NIP-11 limit"
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                return ScopedTokenSupport::Unknown(format!("GET {origin} body: {error}"));
            }
        }
    }
    match std::str::from_utf8(&body) {
        Ok(text) => parse_scoped_token_cap(text),
        Err(error) => {
            ScopedTokenSupport::Unknown(format!("GET {origin} body is not UTF-8: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped `[sandbox] container_delivery_token_cap_secs` default, written out rather than
    /// imported: `seller_exec::DEFAULT_CONTAINER_DELIVERY_TOKEN_CAP_SECS` is behind `wallet`, and
    /// this module and its tests compile on every feature set.
    const CAP: u64 = 21_600;

    #[test]
    fn origin_maps_websocket_schemes_and_drops_the_path() {
        assert_eq!(
            nip11_origin("wss://relay.example").expect("wss"),
            "https://relay.example/"
        );
        assert_eq!(
            nip11_origin("wss://relay.example:7777/").expect("port"),
            "https://relay.example:7777/"
        );
        assert_eq!(
            nip11_origin("ws://127.0.0.1:8080").expect("ws"),
            "http://127.0.0.1:8080/"
        );
        assert_eq!(
            nip11_origin("https://relay.example/git/abc/m0123.git").expect("relay-git remote"),
            "https://relay.example/"
        );
        assert_eq!(
            nip11_origin("WSS://Relay.Example").expect("case"),
            "https://Relay.Example/"
        );
    }

    #[test]
    fn origin_refuses_what_carries_no_nip11_document() {
        assert!(nip11_origin("relay.example").is_err(), "no scheme");
        assert!(nip11_origin("ftp://relay.example").is_err(), "wrong scheme");
        assert!(nip11_origin("wss://").is_err(), "no host");
        assert!(
            nip11_origin("wss://user:pass@relay.example").is_err(),
            "credentials in the url"
        );
    }

    #[test]
    fn authority_prefers_the_relay_git_remote_over_the_event_relay() {
        assert_eq!(
            scoped_token_authority(
                "wss://events.example",
                "https://git.example/git/abc/m0123.git"
            )
            .expect("relay-git remote wins"),
            "https://git.example/"
        );
        // A plain https remote takes no scoped token at all, so the event relay is the only origin
        // left to name; the caller is what decides the question is moot there.
        assert_eq!(
            scoped_token_authority("wss://events.example", "https://github.com/owner/repo.git")
                .expect("byo remote"),
            "https://events.example/"
        );
    }

    #[test]
    fn parse_reads_the_advertised_cap() {
        let document = r#"{"name":"r","limitation":{"auth_required":true,"scoped_token_max_lifetime_secs":21600}}"#;
        assert_eq!(
            parse_scoped_token_cap(document),
            ScopedTokenSupport::Advertised(21_600)
        );
        assert_eq!(
            parse_scoped_token_cap(document).advertised_secs(),
            Some(21_600)
        );
    }

    #[test]
    fn parse_reads_an_absent_field_as_absent() {
        // A relay with a limitation object but no scoped-token field: Requirement B is not there.
        assert_eq!(
            parse_scoped_token_cap(r#"{"name":"r","limitation":{"auth_required":true}}"#),
            ScopedTokenSupport::Absent
        );
        // A relay with no limitation object at all.
        assert_eq!(
            parse_scoped_token_cap(r#"{"name":"r","supported_nips":[1,11]}"#),
            ScopedTokenSupport::Absent
        );
        // An explicit JSON null names no cap, which is the same fact as a missing key.
        assert_eq!(
            parse_scoped_token_cap(r#"{"limitation":{"scoped_token_max_lifetime_secs":null}}"#),
            ScopedTokenSupport::Absent
        );
    }

    #[test]
    fn parse_reads_a_malformed_document_as_unknown() {
        assert!(matches!(
            parse_scoped_token_cap("not json at all"),
            ScopedTokenSupport::Unknown(_)
        ));
        assert!(matches!(
            parse_scoped_token_cap("[1,2,3]"),
            ScopedTokenSupport::Unknown(_)
        ));
        assert!(matches!(
            parse_scoped_token_cap(r#"{"limitation":"six hours"}"#),
            ScopedTokenSupport::Unknown(_)
        ));
        assert!(
            matches!(
                parse_scoped_token_cap(
                    r#"{"limitation":{"scoped_token_max_lifetime_secs":"21600"}}"#
                ),
                ScopedTokenSupport::Unknown(_)
            ),
            "a string is not a lifetime"
        );
        assert!(
            matches!(
                parse_scoped_token_cap(r#"{"limitation":{"scoped_token_max_lifetime_secs":-1}}"#),
                ScopedTokenSupport::Unknown(_)
            ),
            "a negative lifetime is not a lifetime"
        );
        assert!(
            matches!(
                parse_scoped_token_cap(r#"{"limitation":{"scoped_token_max_lifetime_secs":0}}"#),
                ScopedTokenSupport::Unknown(_)
            ),
            "zero is not a lifetime"
        );
    }

    #[test]
    fn verdict_accepts_a_cap_at_or_above_the_configured_one() {
        assert_eq!(
            long_lived_verdict(&ScopedTokenSupport::Advertised(CAP), CAP),
            LongLivedVerdict::Supported {
                advertised_secs: CAP
            }
        );
        assert_eq!(
            long_lived_verdict(&ScopedTokenSupport::Advertised(CAP + 1), CAP),
            LongLivedVerdict::Supported {
                advertised_secs: CAP + 1
            }
        );
        assert!(
            long_lived_verdict(&ScopedTokenSupport::Advertised(CAP), CAP)
                .refusal()
                .is_none()
        );
    }

    #[test]
    fn verdict_refuses_absent_small_and_unknown() {
        let absent = long_lived_verdict(&ScopedTokenSupport::Absent, CAP);
        assert_eq!(absent, LongLivedVerdict::NotAdvertised);
        assert!(!absent.is_supported());

        let small = long_lived_verdict(&ScopedTokenSupport::Advertised(CAP - 1), CAP);
        assert_eq!(
            small,
            LongLivedVerdict::CapTooSmall {
                advertised_secs: CAP - 1,
                configured_secs: CAP
            }
        );

        let unknown = long_lived_verdict(
            &ScopedTokenSupport::Unknown("connection refused".to_owned()),
            CAP,
        );
        assert!(matches!(unknown, LongLivedVerdict::Unknown(_)));

        for verdict in [absent, small, unknown] {
            let refusal = verdict
                .refusal()
                .expect("every non-supported verdict refuses");
            assert!(
                refusal.contains("long-lived"),
                "names the refused mode: {refusal}"
            );
            assert!(
                refusal.contains("fresh-after-agent"),
                "names the mode that works: {refusal}"
            );
            assert!(
                refusal.contains("relay_canary"),
                "names the canary: {refusal}"
            );
        }
    }

    /// A one-shot HTTP server on loopback. Answers the FIRST connection with `response`, then stops.
    /// std sockets on a thread, so no tokio net feature is needed for the listener itself.
    #[cfg(feature = "git-delivery")]
    fn one_shot_http(response: String) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0_u8; 2048];
                let _ = stream.read(&mut scratch);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (origin, handle)
    }

    /// The NIP-11 answer must come from the SAME origin as the push, so a redirect is refused
    /// instead of followed.
    ///
    /// The redirect here points at a second server whose document DOES advertise the cap. So this
    /// test is its own red-prove: if the client ever follows redirects again, the first case stops
    /// being `Unknown` and becomes `Advertised`, and the assertion fails. The second case is the
    /// control — the same document, served directly, is read as `Advertised` — which proves the
    /// refusal is about the redirect and not about a broken harness.
    #[cfg(feature = "git-delivery")]
    #[test]
    fn a_redirect_is_refused_and_never_answers_for_another_origin() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let timeout = std::time::Duration::from_secs(5);
        let document =
            format!(r#"{{"name":"elsewhere","limitation":{{"{SCOPED_TOKEN_CAP_FIELD}":21600}}}}"#);
        let body = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/nostr+json\r\nContent-Length: {}\r\n\r\n{}",
            document.len(),
            document
        );

        // The origin a follower would land on, serving a document that WOULD satisfy the gate.
        let (elsewhere, elsewhere_thread) = one_shot_http(body.clone());
        let (redirector, redirector_thread) = one_shot_http(format!(
            "HTTP/1.1 302 Found\r\nLocation: {elsewhere}/\r\nContent-Length: 0\r\n\r\n"
        ));

        let redirected = runtime.block_on(fetch_scoped_token_support(&redirector, timeout));
        match &redirected {
            ScopedTokenSupport::Unknown(reason) => assert!(
                reason.contains("redirect"),
                "the reason must name the redirect, so an operator can act on it: {reason}"
            ),
            other => panic!(
                "a redirect must NOT answer for another origin, got {other:?} —                  the client is following redirects again"
            ),
        }

        // Control: the very same document, served without a redirect, IS accepted.
        let (direct, direct_thread) = one_shot_http(body);
        let served = runtime.block_on(fetch_scoped_token_support(&direct, timeout));
        assert_eq!(
            served,
            ScopedTokenSupport::Advertised(21_600),
            "control: the same document served directly must be read as advertised"
        );

        let _ = redirector_thread.join();
        let _ = direct_thread.join();
        // The redirect was never followed, so nothing connected to `elsewhere`; its accept() is
        // still blocked. Connect once so the thread can finish rather than leak.
        let _ = std::net::TcpStream::connect(elsewhere.trim_start_matches("http://"));
        let _ = elsewhere_thread.join();
    }

    #[test]
    fn a_too_small_refusal_prints_both_numbers() {
        let refusal = long_lived_verdict(&ScopedTokenSupport::Advertised(600), CAP)
            .refusal()
            .expect("refuse");
        assert!(refusal.contains("600"), "advertised cap: {refusal}");
        assert!(
            refusal.contains(&CAP.to_string()),
            "configured cap: {refusal}"
        );
    }
}
