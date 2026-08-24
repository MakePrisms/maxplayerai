//! Host-side credential-containment proxy (#647).
//!
//! A `[sandbox] mode = "docker"` job runs a stranger's code with unrestricted egress. Forwarding the
//! real model credential as `-e ANTHROPIC_API_KEY=…` fixed the *baked-into-the-image* leak, but the
//! job can still read that variable (`printenv`, `/proc/self/environ`) and exfiltrate a reusable
//! secret. This module removes the real credential from the container entirely.
//!
//! ## The mechanism (petar's DECIDED design, #647)
//!
//! One value carries two properties. Per job we mint a **format-plausible placeholder** in the
//! vendor's credential shape (`sk-ant-…`), forward *that* into the container in place of the real
//! key, and point the container's `ANTHROPIC_BASE_URL` at this proxy. The proxy:
//!
//! 1. **identifies the job** by finding the placeholder value in a request header, and
//! 2. **substitutes** the real credential for the placeholder on the way out — **value-based across
//!    header values**, so it matches whichever header a vendor uses (`x-api-key`, `authorization`,
//!    `api-key`, …) without knowing the name. That header-agnosticism is what lets the same proxy
//!    serve `codex`, `cursor`, and any future harness with no per-vendor code.
//!
//! ## Why the BODY is never a substitution surface
//!
//! An earlier revision substituted the placeholder wherever it appeared, body included. That was a
//! **credential-recovery hole**, and closing it is why this module now touches headers only.
//!
//! Value-based matching over header VALUES already gives full header-agnosticism — the reason body
//! substitution was there in the first place. It bought no vendor coverage: every credential this
//! proxy contains travels in a header (`x-api-key` for the Anthropic API key, `Authorization: Bearer`
//! for the Anthropic auth/OAuth tokens and the OpenAI key). What it did buy was an injection surface,
//! because the body is the one part of the request a stranger's job writes freely:
//!
//! 1. The job reads its own placeholder (`/proc/1/environ` — the container env is readable to it).
//! 2. It posts to the proxy with the placeholder in the auth header AND inside a prompt string.
//! 3. The proxy substitutes BOTH, so the REAL credential lands in the prompt.
//! 4. The model repeats the prompt back, and the response reaches the container.
//!
//! The job now holds the real credential and containment is defeated. A variant needs no model
//! cooperation at all: put the placeholder in a field the API rejects and read the real value out of
//! the echoed error.
//!
//! The same reasoning already kept the URL path off the substitution surface; the body belongs in that
//! exclusion for identical reasons, and it is the larger surface of the two. The body is therefore
//! forwarded VERBATIM — it is not even an input to [`ProxyEngine::authorize`], so no future code path
//! can reintroduce the substitution. If some future harness genuinely carries a credential in the
//! body, it BREAKS here rather than leaking: the same fail-closed rule this module applies to derived
//! tokens.
//!
//! Defence in depth on the return leg: [`relay`] scrubs the real credential back to the placeholder in
//! the response headers and body. With the body no longer substituted, the only value that can echo is
//! the header the upstream itself reflects (a `401` quoting the key it rejected, say) — that scrub
//! catches it. It is the second line, never the first.
//!
//! What leaks from the container is now worthless: it only authenticates against *our* proxy, only
//! for the life of the job, and only for an allowlisted upstream.
//!
//! ## Non-negotiable invariants (babu, #647)
//!
//! - **Destination allowlist.** The real credential is substituted ONLY for an approved upstream. A
//!   request whose resolved destination is not on the allowlist is refused WITHOUT substitution —
//!   otherwise a job could point the placeholder at an attacker host and the proxy would hand over the
//!   real key, *worse* than today. See [`ProxyEngine::authorize`].
//! - **No fallback to the real credential.** If the override path cannot be satisfied (no known
//!   placeholder present, destination refused) the request fails; the caller NEVER falls back to
//!   putting the real credential in the container. A silent fallback is the one failure mode invisible
//!   from outside — jobs keep passing while containment is gone.
//! - **Substitute in header values ONLY — never the body, never the URL path.**
//!   [`ProxyEngine::authorize`] rewrites header values and nothing else; the request target and the
//!   body are forwarded verbatim. Both are attacker-authored, so neither may carry a real credential.
//! - **Format-plausible placeholder**, not a readable sentinel — see [`mint_anthropic_placeholder`].
//! - **No secret in any image.** The proxy is a host process; the container receives only the
//!   placeholder and the base-URL override.
//!
//! ## Scope
//!
//! This module builds the proven **API-key path** (the spike confirmed the adapter honours
//! `ANTHROPIC_BASE_URL` and the key rides a single `x-api-key` header verbatim). Substitution is
//! value-based rather than `x-api-key`-specific precisely so the OAuth path folds in later with no
//! change here — pending a separate verification that the OAuth token also travels verbatim.

#![cfg(feature = "wallet")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use tokio::sync::Semaphore;

/// The proxy listener terminates INSIDE the seller daemon (a money-path process), and the caller is a
/// stranger's job. These bound what one job can make the daemon hold or spawn, so an unbounded or
/// trickled request cannot OOM or exhaust it.
///
/// The request body is buffered (it must be, to find and substitute the placeholder before egress);
/// this caps that buffer. 32 MiB is far above any real model request and far below a memory hazard.
pub const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Concurrent connections the proxy will service at once. A per-job proxy serves ONE agent, so a
/// modest ceiling is generous for legitimate use and caps a connection flood. Excess connections wait
/// at the OS accept backlog rather than each spawning an unbounded task.
const MAX_CONCURRENT_CONNECTIONS: usize = 64;
/// Concurrent HTTP/2 streams one connection may open.
///
/// [`MAX_CONCURRENT_CONNECTIONS`] bounds CONNECTIONS, and under HTTP/1 that is also the bound on
/// requests in flight — one connection carries one request at a time. **HTTP/2 breaks that identity:**
/// a single connection multiplexes, so without an explicit stream bound the connection ceiling stops
/// being a ceiling on work. hyper's own default is 200 streams per connection
/// (`hyper-1.10.1 proto/h2/server.rs:69`), which would turn a 64-connection cap into 12,800
/// concurrent requests — a 200x widening of a limit nobody changed.
///
/// Set to mirror [`MAX_CONCURRENT_CONNECTIONS`], so the realistic shape — one agent on one
/// multiplexed connection — gets the concurrency an HTTP/1 client would have had across the whole
/// listener.
///
/// ⚠ **This is a PER-CONNECTION bound and it is not, on its own, a ceiling on work.** 64 connections
/// each running 64 streams is 4096 requests in flight, and at [`MAX_REQUEST_BODY_BYTES`] each that is
/// the very resource hazard the connection cap exists to prevent. The aggregate ceiling is
/// [`MAX_IN_FLIGHT_REQUESTS`]; this constant only stops one connection from monopolising it.
const MAX_CONCURRENT_H2_STREAMS: u32 = 64;
/// Requests in flight across the whole listener, both protocols, whatever the connection count.
///
/// The real ceiling on concurrent work, and the only one of the three that does not change meaning
/// with the protocol. [`MAX_CONCURRENT_CONNECTIONS`] bounds sockets and
/// [`MAX_CONCURRENT_H2_STREAMS`] bounds streams within one socket; under HTTP/1 those coincide with
/// requests, and under HTTP/2 they multiply. **Bounding the product is what keeps the buffered-body
/// memory ceiling honest** — without it, adding h2c support would have silently widened a limit
/// nobody edited, which is exactly the failure mode this module's other bounds exist to avoid.
const MAX_IN_FLIGHT_REQUESTS: usize = 64;
/// How long the head (request line + headers) may take to arrive. hyper enforces this itself; without
/// it a trickled header stream pins a connection open indefinitely.
///
/// HTTP/1 only: HTTP/2 has no equivalent head-read deadline to set, so the body deadline and the
/// stream bound above are what bound a slow HTTP/2 peer.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the request BODY may take to arrive in full. Bounds a slow-loris body that dribbles bytes
/// under the size cap forever. It bounds only the REQUEST read — the upstream RESPONSE (an SSE stream)
/// is relayed without any such deadline, so a long completion is never cut off.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// The default real upstream for the Anthropic API-key path when the operator has not pointed the
/// daemon at a custom gateway. Any resolved destination must appear on the proxy's allowlist before
/// the real credential is substituted, so this constant also seeds the default allowlist entry.
pub const ANTHROPIC_DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

/// The default real upstream for the OpenAI API-key path (codex seats) when no `OPENAI_BASE_URL`
/// override is set. Seeds an allowlist entry the same way [`ANTHROPIC_DEFAULT_UPSTREAM`] does.
pub const OPENAI_DEFAULT_UPSTREAM: &str = "https://api.openai.com";

/// The hostname a container uses to reach the host-side proxy. On Linux the docker launch maps it to
/// the host with `--add-host <alias>:host-gateway`; on macOS docker provides it natively. Shared with
/// [`crate::seller_exec`] so the alias the container is told to use and the alias the launch resolves
/// are the same string.
pub const PROXY_HOST_ALIAS: &str = "host.docker.internal";

/// The address the per-job proxy listens on.
///
/// Every interface, because on Linux `host.docker.internal` maps to the bridge gateway and a
/// loopback-bound service is unreachable from the container. Narrowing this to the bridge address is
/// platform-split — Docker Desktop reaches a loopback-bound host service through the same alias — and
/// is tracked as a follow-up rather than guessed at. Named here so the one place that binds and the
/// comments that reason about the exposure cannot drift apart.
const BIND_ADDRESS: &str = "0.0.0.0";

/// Hop-by-hop headers that must not be copied verbatim onto the forwarded request/response. They
/// describe the *connection* the proxy terminates, not the message, and reqwest/hyper regenerate the
/// ones that still apply (`host`, `content-length`) from the outgoing request itself.
const HOP_BY_HOP: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

/// One job's containment secret: the placeholder the container was handed, the real credential it
/// stands in for, and the single approved upstream that credential may be substituted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCredential {
    /// The format-plausible value inside the container (`sk-ant-…`). Worthless if it leaks.
    pub placeholder: String,
    /// The real credential. Never enters the container; held only in this host process.
    pub real: String,
    /// The approved real upstream base URL (scheme + host[:port]) this credential is valid for.
    pub upstream: String,
}

/// Why a request was refused. Each variant means the real credential was NOT substituted — the
/// request either failed outright or was forwarded with the (worthless) placeholder untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No registered placeholder was found anywhere in the request. The proxy cannot identify a job,
    /// so it will not substitute any credential (no-fallback): the request fails.
    NoKnownPlaceholder,
    /// A placeholder was found, but the resolved destination host is not on the allowlist. The real
    /// credential is withheld and the request is refused — the load-bearing invariant that keeps this
    /// from being worse than the status quo.
    DestinationNotAllowed { host: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKnownPlaceholder => {
                write!(f, "no known per-job placeholder in request; refusing without substitution")
            }
            Self::DestinationNotAllowed { host } => {
                write!(f, "destination {host} not on the credential-substitution allowlist")
            }
        }
    }
}

/// The outcome of authorizing one request against the engine: either a forward plan whose HEADERS
/// carry the real credential (destination approved) or a typed [`Refusal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Forward to `upstream` (base URL) with these substituted headers.
    ///
    /// The body and the path/query are BOTH taken unchanged from the original request by the
    /// transport — they are deliberately absent here so no code path can substitute into either. The
    /// body exclusion is load-bearing, not tidiness: a job authors its own request body, so
    /// substituting there hands it the real credential the moment the upstream echoes the body back
    /// (see the module docs).
    Forward {
        upstream: String,
        headers: Vec<(String, String)>,
    },
    Refuse(Refusal),
}

/// The credential-substitution core: the destination allowlist plus the live per-job registry. Pure
/// decision logic ([`Self::authorize`]) is separated from all socket I/O so the invariants are
/// unit-testable without a container, a network, or a real credential.
#[derive(Debug, Default)]
pub struct ProxyEngine {
    /// Approved upstream hosts (lowercased, `host` or `host:port`). The real credential is substituted
    /// only when the resolved destination's host is in this set.
    allowlist: Vec<String>,
    /// Registered per-job credentials, keyed by placeholder value for O(1) identification.
    creds: Mutex<HashMap<String, JobCredential>>,
}

impl ProxyEngine {
    /// A new engine whose allowlist is exactly `hosts` (each normalized to a lowercased authority).
    pub fn new(hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowlist: hosts.into_iter().map(|h| host_key(&h)).collect(),
            creds: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `host` (an authority, `host` or `host:port`) is approved for substitution.
    ///
    /// The allowlist is the UNION of every present credential's upstream, so this answers "may *some*
    /// registered credential be substituted for this destination". It is a belt on a destination
    /// [`Self::authorize`] has already chosen from the credential itself — it is NOT the question a
    /// redirect asks. See [`allows_paired_redirect`].
    pub fn allows(&self, host: &str) -> bool {
        let key = host_key(host);
        self.allowlist.iter().any(|allowed| same_authority(allowed, &key))
    }

    /// Register a job's credential. Refuses (returns `Err`) if the credential's upstream is not on the
    /// allowlist — a belt that keeps an unapproved destination from ever entering the registry, so a
    /// registration bug cannot defeat the [`Self::authorize`] allowlist check downstream.
    pub fn register(&self, cred: JobCredential) -> Result<(), Refusal> {
        let host = authority_of(&cred.upstream)
            .ok_or_else(|| Refusal::DestinationNotAllowed { host: cred.upstream.clone() })?;
        if !self.allows(&host) {
            return Err(Refusal::DestinationNotAllowed { host });
        }
        self.creds.lock().unwrap().insert(cred.placeholder.clone(), cred);
        Ok(())
    }

    /// Drop a job's credential (job end = revocation point). Returns whether one was present.
    pub fn deregister(&self, placeholder: &str) -> bool {
        self.creds.lock().unwrap().remove(placeholder).is_some()
    }

    /// The decision for one request, from its headers alone.
    ///
    /// The request BODY is deliberately not a parameter, and neither is the path. Both are authored by
    /// a stranger's job, so neither may become a place the real credential is written — see the module
    /// docs for the recovery attack that body substitution enabled. Keeping them out of the signature
    /// is what makes the exclusion structural rather than a rule someone must remember.
    ///
    /// 1. **Identify the job** — find a registered placeholder that appears in a header VALUE. None ⇒
    ///    [`Refusal::NoKnownPlaceholder`] (no-fallback: we will not forward a real credential we cannot
    ///    attribute to a job).
    /// 2. **Allowlist the destination** — the resolved upstream host must be approved, else
    ///    [`Refusal::DestinationNotAllowed`] with NO substitution.
    /// 3. **Substitute** — replace the placeholder with the real credential in every header value.
    pub fn authorize(&self, headers: &[(String, String)]) -> Decision {
        let creds = self.creds.lock().unwrap();
        let Some(cred) = creds
            .values()
            .find(|c| placeholder_present(&c.placeholder, headers))
            .cloned()
        else {
            return Decision::Refuse(Refusal::NoKnownPlaceholder);
        };
        drop(creds);

        let Some(host) = authority_of(&cred.upstream) else {
            return Decision::Refuse(Refusal::DestinationNotAllowed {
                host: cred.upstream.clone(),
            });
        };
        if !self.allows(&host) {
            return Decision::Refuse(Refusal::DestinationNotAllowed { host });
        }

        let headers = headers
            .iter()
            .filter(|(name, _)| !is_hop_by_hop(name))
            .map(|(name, value)| (name.clone(), value.replace(&cred.placeholder, &cred.real)))
            .collect();
        Decision::Forward {
            upstream: cred.upstream,
            headers,
        }
    }

    /// The real credential registered under `placeholder`, for the response scrub in [`relay`].
    ///
    /// Returned as `(real, placeholder)` so the caller can run the substitution in REVERSE on the way
    /// back. Not a leak of anything the caller does not already hold: [`Self::authorize`] just put this
    /// same value into the outgoing headers.
    fn scrub_pair_for(&self, headers: &[(String, String)]) -> Option<(String, String)> {
        let creds = self.creds.lock().unwrap();
        creds
            .values()
            .find(|c| placeholder_present(&c.placeholder, headers))
            .map(|c| (c.real.clone(), c.placeholder.clone()))
    }
}

/// Whether `placeholder` appears in any header value. Header *names* are not searched: the placeholder
/// is a credential value, and matching a name would be a false positive.
///
/// The BODY is not searched either. A body-only match could never authenticate anything (the upstream
/// reads the header), so it identified nothing while widening what counted as "a request from this
/// job" — and it was the first half of the recovery attack the module docs describe.
fn placeholder_present(placeholder: &str, headers: &[(String, String)]) -> bool {
    headers.iter().any(|(_, v)| v.contains(placeholder))
}

/// Replace every occurrence of `needle` with `replacement` in a byte buffer. Bodies are not required to
/// be UTF-8, so this works on raw bytes rather than going through `String`.
///
/// Used on the RESPONSE leg only (real ⇒ placeholder). The request leg deliberately has no body
/// substitution at all.
fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn is_hop_by_hop(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&name.as_str())
}

/// Normalize an authority to a lowercased comparison key.
fn host_key(host: &str) -> String {
    host.trim().to_ascii_lowercase()
}

/// Drop an explicit `:443`/`:80` so a default-port authority compares equal to a bare host.
fn strip_default_port(authority: &str) -> &str {
    authority
        .strip_suffix(":443")
        .or_else(|| authority.strip_suffix(":80"))
        .unwrap_or(authority)
}

/// Whether two authorities name the same service, treating an explicit default port as equivalent to
/// none. The one comparison both the allowlist and a redirect decision go through, so neither can
/// drift from the other on `host` versus `host:443`.
fn same_authority(a: &str, b: &str) -> bool {
    a == b || strip_default_port(a) == strip_default_port(b)
}

/// Whether a redirect may carry the credential minted for `original` on to `target`.
///
/// A job's credential is registered for exactly ONE upstream ([`JobCredential::upstream`]), and
/// [`ProxyEngine::authorize`] forwards to that upstream and no other — the container never names its
/// own destination. A redirect must not widen that: the credential moves only to the authority it was
/// registered for.
///
/// `original` is the ORIGINAL request URL, never the previous hop. Judging each hop against its
/// predecessor would let a chain walk one authority at a time to anywhere.
///
/// **Deliberately a free function, not a [`ProxyEngine`] method.** An engine-scoped check answers the
/// weaker union question and would approve a redirect to a DIFFERENT vendor's registered upstream —
/// handing, say, an Anthropic key to OpenAI's host. The union is a belt on an already-chosen
/// destination; it is not an authorization to move a credential to a new one.
///
/// Either side unresolvable is refused: the credential must not move to a destination this proxy
/// cannot name, and an empty redirect chain reaches here as an unresolvable `original`.
pub fn allows_paired_redirect(original: &str, target: &str) -> bool {
    match (authority_of(original), authority_of(target)) {
        (Some(from), Some(to)) => same_authority(&from, &to),
        _ => false,
    }
}

/// The authority (`host` or `host:port`, lowercased) of a base URL like `https://api.anthropic.com`.
/// Returns `None` for a value that is not a parseable `scheme://authority` URL.
pub fn authority_of(base_url: &str) -> Option<String> {
    let rest = base_url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    Some(host_key(authority))
}

/// Mint a format-plausible Anthropic API-key placeholder: the real `sk-ant-api03-` prefix followed by
/// random url-safe characters to the real length. It is a *plausible* credential, not a readable
/// sentinel — so a job that validates the credential's shape locally does not refuse before the call,
/// and a leaked value looks exactly like the real thing while authenticating against nothing but this
/// proxy.
pub fn mint_anthropic_placeholder() -> String {
    // Real keys are `sk-ant-api03-` + ~93 url-safe chars. Match the prefix and total length.
    mint_placeholder("sk-ant-api03-", 93)
}

/// Mint a format-plausible placeholder: `prefix` followed by `random_len` random url-safe characters.
/// The prefix carries the vendor's credential shape (so a client that shape-validates locally does not
/// refuse before the call) and the random tail makes each per-job value unique. Used per credential
/// variable so every contained secret gets its own distinct placeholder to substitute at egress.
pub fn mint_placeholder(prefix: &str, random_len: usize) -> String {
    format!("{prefix}{}", random_token(random_len))
}

/// Mint a per-job placeholder for a credential the client PARSES rather than carries as an opaque
/// string. A vendor whose credential is a JWT reads its claims before sending it, so a
/// prefix-plus-random placeholder is refused LOCALLY and never reaches the proxy — which presents as
/// "containment broke the seat" rather than "the placeholder was malformed" (#850).
///
/// This is the OPPOSITE constraint to [`mint_placeholder`]'s vendors, whose clients do no local
/// validation at all (three unrelated shapes produce byte-identical rejections there, so prefix and
/// length are free). Do not carry that freedom across: the two carriers of one credential can impose
/// opposite validation.
///
/// The signature segment is random and that is sound rather than lucky: a bearer token's signature is
/// verified by its ISSUER, never by the client carrying it, so there is no local crypto check to
/// satisfy. Measured — a placeholder with a random signature travelled verbatim onto the wire.
///
/// `exp` is minted PER JOB and must never be hardcoded. A fixed placeholder works for months and then
/// begins failing, and the failure presents as an auth error that nobody traces back to a constant.
pub fn mint_jwt_placeholder(claim_type: &str, valid_for: std::time::Duration) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let exp = now.saturating_add(valid_for.as_secs());
    let header = base64url(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload =
        base64url(format!(r#"{{"type":"{claim_type}","time":{now},"exp":{exp}}}"#).as_bytes());
    // 32 bytes is the width of an HS256 MAC, so the segment is shaped like a signature without
    // being one.
    let signature = base64url(&random_bytes(32));
    format!("{header}.{payload}.{signature}")
}

/// Unpadded base64url. JWT segments use this alphabet and carry no padding; a standard-alphabet or
/// padded segment is not a well-formed JWT, and a client that parses before sending refuses it.
///
/// Hand-rolled to keep this off the feature graph: the crate's `base64` dependency is optional and
/// gated behind `git-delivery`, while containment lives under `acp`.
fn base64url(raw: &[u8]) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(raw.len().div_ceil(3) * 4);
    for chunk in raw.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let packed =
            ((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | (bytes[2] as u32);
        let sextets = [
            (packed >> 18) & 63,
            (packed >> 12) & 63,
            (packed >> 6) & 63,
            packed & 63,
        ];
        // A 3-byte chunk emits 4 characters, 2 bytes emit 3, and 1 byte emits 2 — the unpadded form.
        for sextet in sextets.iter().take(chunk.len() + 1) {
            out.push(CHARSET[*sextet as usize] as char);
        }
    }
    out
}

/// `len` bytes from the OS RNG.
fn random_bytes(len: usize) -> Vec<u8> {
    let mut raw = vec![0u8; len];
    getrandom::fill(&mut raw).expect("OS RNG must be available to mint a per-job placeholder");
    raw
}

/// `len` random characters from the url-safe base64 alphabet (the charset vendor keys use).
fn random_token(len: usize) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut raw = vec![0u8; len];
    getrandom::fill(&mut raw).expect("OS RNG must be available to mint a per-job placeholder");
    raw.iter().map(|b| CHARSET[(*b as usize) % CHARSET.len()] as char).collect()
}

/// A running per-job proxy: the bound host address the container reaches (`http://<addr>`), the engine
/// holding this job's credential, and the accept-loop task. Dropping it aborts the task — the job's
/// revocation point.
pub struct RunningProxy {
    addr: SocketAddr,
    engine: Arc<ProxyEngine>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningProxy {
    /// The engine backing this proxy, for registering/deregistering job credentials.
    pub fn engine(&self) -> &Arc<ProxyEngine> {
        &self.engine
    }

    /// The base URL a container uses as `ANTHROPIC_BASE_URL`, reaching this proxy at `host`.
    ///
    /// The host part is a parameter because a job contained in its own network namespace **cannot**
    /// have the `host.docker.internal` alias: `--network=container:<holder>` and `--add-host` are
    /// mutually exclusive (`conflicting options: custom host-to-IP mapping and the network mode`), and
    /// `/etc/hosts` is per-mount-namespace so the holder's copy is invisible to the job. Such a job
    /// gets a literal address, measured from docker itself — see `crate::sandbox_netns`.
    ///
    /// Whatever is passed here is also what the firewall pinhole names, so the two cannot drift: an
    /// ACCEPT for one address and a base URL for another would leave every job unable to reach its
    /// model, with every rule-rendering test still green.
    pub fn container_base_url_via(&self, host: &str) -> String {
        format!("http://{host}:{}", self.addr.port())
    }

    /// The base URL for a container that reaches the host over the docker host-gateway alias — the
    /// shape for a job that is NOT namespace-contained.
    pub fn container_base_url(&self) -> String {
        self.container_base_url_via(PROXY_HOST_ALIAS)
    }

    /// The address the proxy is actually bound to on the host (for tests / diagnostics).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for RunningProxy {
    fn drop(&mut self) {
        // Job end = revocation: kill the listener so the placeholder stops working the instant the job
        // is done, even if a caller forgets to deregister.
        self.task.abort();
    }
}

/// Bind the proxy's listener: an ephemeral port, or the first free port in a configured range.
///
/// The scan walks the range in order and takes the first port that binds. `AddrInUse` is the
/// expected result for a port another job already holds, so it moves on; any other error is a real
/// failure to bind and is returned immediately rather than being retried against 99 more ports.
async fn bind_listener(
    ports: Option<crate::sandbox_net::PortRange>,
) -> std::io::Result<tokio::net::TcpListener> {
    let Some(range) = ports else {
        return tokio::net::TcpListener::bind((BIND_ADDRESS, 0)).await;
    };
    for port in range.start()..=range.end() {
        match tokio::net::TcpListener::bind((BIND_ADDRESS, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!(
            "[sandbox] proxy_port_range {range} is fully occupied ({} ports); a contained job needs \
             one port for its lifetime, so widen the range or run fewer concurrent jobs",
            range.capacity()
        ),
    ))
}

/// Bind a per-job proxy on the host and start its accept loop.
///
/// Binds `0.0.0.0` so the container can reach it via the docker `host.docker.internal:host-gateway`
/// alias (the launch adds `--add-host`). Exposure of the listener is bounded by the three invariants:
/// a caller needs a registered placeholder to get *any* substitution, the destination must be on the
/// allowlist, and the listener dies with the job. Tightening the bind to the docker bridge address is
/// a follow-up that dovetails with the #797 egress work.
///
/// `ports` selects which port it lands on. `None` ⇒ port 0: the kernel picks an ephemeral one, fresh
/// per job. `Some(range)` ⇒ the first free port in the range, which is what makes the listener
/// nameable by a static firewall rule — `#797`'s host-services deny has to express one exception for
/// this proxy, and "whatever port the kernel chose" is not expressible. The range narrows which port
/// is chosen and nothing else; the address, the allowlist and the substitution are untouched by it.
///
/// A configured range that is fully occupied FAILS the job. It does not fall back to an ephemeral
/// port: that would place the listener outside the range the firewall pinhole names, and the job
/// would then fail to reach its model with no indication that the port was the reason. Same
/// no-fallback rule the rest of this path follows.
pub async fn start(
    engine: Arc<ProxyEngine>,
    client: reqwest::Client,
    ports: Option<crate::sandbox_net::PortRange>,
) -> std::io::Result<RunningProxy> {
    // KNOWN EXPOSURE, deliberately unchanged for now. `0.0.0.0` is every interface, so on a seller with
    // a public IP and no firewall this per-job listener is internet-reachable on a random high port.
    // The placeholder is the bearer — a caller without it gets `NoKnownPlaceholder` — so this is not an
    // open relay. But the job HOLDS the placeholder by construction, so it can hand placeholder+port to
    // an outside accomplice, who can then burn the seller's model quota for the job's lifetime. Quota
    // theft, bounded by job duration; not credential theft (the real value never leaves this process).
    //
    // Narrowing the bind is platform-split and must not be guessed: Docker Desktop reaches a
    // loopback-bound host service through `host.docker.internal`, but on Linux the alias maps to the
    // bridge gateway (`--add-host …:host-gateway`), where a `127.0.0.1` bind is unreachable and the
    // correct target is the bridge address. Changing this needs a Linux docker seat to verify against;
    // until then, seller-operators on a public box should firewall inbound ports.
    let listener = bind_listener(ports).await?;
    let addr = listener.local_addr()?;
    let engine_for_task = Arc::clone(&engine);
    let connections = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    // Shared across every connection and both protocols, so multiplexing cannot widen it.
    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS));
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                continue;
            };
            // Bound concurrent connections: acquire a permit before spawning, hold it for the life of
            // the connection. When all permits are out, this awaits — new connections queue at the OS
            // accept backlog instead of each spawning an unbounded task and exhausting the daemon.
            let Ok(permit) = Arc::clone(&connections).acquire_owned().await else {
                continue; // semaphore closed — proxy shutting down
            };
            let engine = Arc::clone(&engine_for_task);
            let client = client.clone();
            let in_flight = Arc::clone(&in_flight);
            tokio::spawn(async move {
                let _permit = permit; // released when the connection ends
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let engine = Arc::clone(&engine);
                    let client = client.clone();
                    let in_flight = Arc::clone(&in_flight);
                    async move {
                        // Held for the life of THIS request, so the ceiling counts requests rather
                        // than sockets. Excess requests wait here instead of each buffering a body.
                        let _in_flight = in_flight.acquire().await;
                        handle_request(req, engine, client).await
                    }
                });
                // Auto-negotiating server: it sniffs the HTTP/2 prior-knowledge preface
                // (`PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n`) and otherwise falls through to HTTP/1 on the
                // same listener. An http1-only server cannot parse that preface, and the failure is
                // silent in the worst way — it surfaces NO request, so a client speaking h2c reads as
                // "nothing ever connected" rather than as a protocol mismatch.
                let mut builder = AutoBuilder::new(TokioExecutor::new());
                builder
                    .http1()
                    // A timer is required for hyper's own timeouts to arm.
                    .timer(TokioTimer::new())
                    // Bound the head read so a trickled header stream cannot pin a connection open.
                    .header_read_timeout(HEADER_READ_TIMEOUT);
                builder
                    .http2()
                    .timer(TokioTimer::new())
                    // Without this, one multiplexed connection escapes the connection ceiling.
                    .max_concurrent_streams(MAX_CONCURRENT_H2_STREAMS);
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });
    Ok(RunningProxy { addr, engine, task })
}

type ProxyBody = BoxBody<Bytes, std::io::Error>;

/// Serve one request: buffer it, run it through [`ProxyEngine::authorize`], and — only on a
/// substituted forward — relay it to the real upstream and stream the response back. A refusal returns
/// a `4xx`/`5xx` to the container with the real credential never in play.
async fn handle_request(
    req: Request<Incoming>,
    engine: Arc<ProxyEngine>,
    client: reqwest::Client,
) -> Result<Response<ProxyBody>, std::convert::Infallible> {
    let method = req.method().clone();
    // The path+query is forwarded verbatim; it is never a substitution input.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect();
    // Buffer the body under a size cap AND a read deadline: the listener lives in the seller daemon,
    // so a stranger's job must not be able to OOM it with an unbounded body or pin memory by
    // trickling one under the cap forever. `Limited` stops reading past the cap (413); the timeout
    // bounds a slow-loris body (408). Neither touches the upstream RESPONSE stream.
    let capped = Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES);
    let body = match tokio::time::timeout(BODY_READ_TIMEOUT, capped.collect()).await {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(error)) => {
            let over_cap = error
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some();
            return Ok(if over_cap {
                refusal_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body exceeds the proxy limit",
                )
            } else {
                refusal_response(StatusCode::BAD_GATEWAY, "failed to read request body")
            });
        }
        Err(_) => {
            return Ok(refusal_response(
                StatusCode::REQUEST_TIMEOUT,
                "request body read timed out",
            ))
        }
    };

    // The reverse-substitution pair for the response leg, resolved from the SAME header match that
    // authorizes the request. Taken before `authorize` consumes nothing — both read the incoming
    // headers, so they agree on which job this is.
    let scrub = engine.scrub_pair_for(&headers);

    match engine.authorize(&headers) {
        Decision::Refuse(reason) => {
            let status = match reason {
                Refusal::DestinationNotAllowed { .. } => StatusCode::FORBIDDEN,
                Refusal::NoKnownPlaceholder => StatusCode::BAD_GATEWAY,
            };
            Ok(refusal_response(status, &reason.to_string()))
        }
        // `body` is the ORIGINAL buffered request body, forwarded verbatim: the decision never
        // carried it, so nothing substituted a credential into it.
        Decision::Forward { upstream, headers } => {
            match relay(&client, &method, &upstream, &path_and_query, headers, body, scrub).await {
                Ok(response) => Ok(response),
                // No-fallback: an upstream failure fails the request; it never resends without the
                // proxy or with the real credential in the container.
                Err(message) => Ok(refusal_response(StatusCode::BAD_GATEWAY, &message)),
            }
        }
    }
}

/// Relay a header-substituted request to the real upstream and stream the response back to the
/// container without buffering — model responses are SSE streams.
///
/// `scrub` is the `(real, placeholder)` pair for this job. Every occurrence of the real credential in
/// the response — headers and body alike — is rewritten back to the placeholder before the container
/// sees it. This is DEFENCE IN DEPTH, not the primary control: the primary control is that the real
/// credential only ever exists in a request HEADER, so the sole way it can return is an upstream that
/// reflects that header (a `401` quoting the key it rejected). The scrub catches that class, plus any
/// echo behaviour a future upstream invents.
async fn relay(
    client: &reqwest::Client,
    method: &hyper::Method,
    upstream: &str,
    path_and_query: &str,
    headers: Vec<(String, String)>,
    body: Bytes,
    scrub: Option<(String, String)>,
) -> Result<Response<ProxyBody>, String> {
    let url = format!("{}{}", upstream.trim_end_matches('/'), path_and_query);
    let method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|error| format!("bad method: {error}"))?;
    let mut request = client.request(method, &url).body(body.to_vec());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let upstream_response = request
        .send()
        .await
        .map_err(|error| format!("upstream request failed: {error}"))?;

    let status = upstream_response.status();
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in upstream_response.headers() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        // Response headers are scrubbed too: an upstream that reflects the credential in a header
        // (`www-authenticate`, a debug echo) must not hand it to the container.
        match (&scrub, value.to_str()) {
            (Some((real, placeholder)), Ok(text)) if text.contains(real.as_str()) => {
                builder = builder.header(name, text.replace(real.as_str(), placeholder));
            }
            _ => builder = builder.header(name, value),
        }
    }
    let upstream_body = Box::pin(upstream_response.bytes_stream());
    let body = match scrub {
        Some((real, placeholder)) => {
            BodyExt::boxed(StreamBody::new(scrub_stream(upstream_body, real, placeholder)))
        }
        None => BodyExt::boxed(StreamBody::new(
            upstream_body.map(|chunk| chunk.map(Frame::data).map_err(std::io::Error::other)),
        )),
    };
    builder
        .body(body)
        .map_err(|error| format!("building proxied response failed: {error}"))
}

/// Rewrite `real` back to `placeholder` across a streamed response body, without buffering the stream.
///
/// A credential can straddle a chunk boundary, so a naive per-chunk replace would miss it. This holds
/// back the last `real.len() - 1` bytes of what it would otherwise emit — the longest possible partial
/// match — and re-examines them once the next chunk arrives. The held-back tail is flushed when the
/// upstream stream ends.
///
/// SSE responses stay streaming: each chunk is forwarded as it arrives, minus that small carry-over.
fn scrub_stream<S>(
    inner: S,
    real: String,
    placeholder: String,
) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    // `None` inner ⇒ the stream is finished (ended or errored); stop yielding.
    futures_util::stream::unfold(
        (Some(inner), Vec::<u8>::new()),
        move |(inner, carry)| {
            let (real, placeholder) = (real.clone(), placeholder.clone());
            async move {
                let mut inner = inner?;
                let mut carry = carry;
                loop {
                    match inner.next().await {
                        Some(Ok(chunk)) => {
                            carry.extend_from_slice(&chunk);
                            let replaced =
                                replace_bytes(&carry, real.as_bytes(), placeholder.as_bytes());
                            // Everything but a possible split match at the tail is safe to emit.
                            let hold = real.len().saturating_sub(1).min(replaced.len());
                            let split = replaced.len() - hold;
                            if split == 0 {
                                // Nothing emittable yet — keep pulling rather than yielding empty.
                                carry = replaced;
                                continue;
                            }
                            let emit = Bytes::copy_from_slice(&replaced[..split]);
                            let rest = replaced[split..].to_vec();
                            return Some((Ok(Frame::data(emit)), (Some(inner), rest)));
                        }
                        Some(Err(error)) => {
                            return Some((Err(std::io::Error::other(error)), (None, Vec::new())));
                        }
                        None => {
                            let tail =
                                replace_bytes(&carry, real.as_bytes(), placeholder.as_bytes());
                            if tail.is_empty() {
                                return None;
                            }
                            return Some((Ok(Frame::data(Bytes::from(tail))), (None, Vec::new())));
                        }
                    }
                }
            }
        },
    )
}

/// A small text response for a refusal or transport error — the container sees a `4xx`/`5xx`, never a
/// substituted credential.
fn refusal_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(format!("{message}\n")))
        .map_err(|never| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .expect("static refusal response is always well-formed")
}

use futures_util::StreamExt as _;

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "sk-ant-api03-REALaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const UPSTREAM: &str = ANTHROPIC_DEFAULT_UPSTREAM;

    fn engine_with_job(placeholder: &str, upstream: &str) -> ProxyEngine {
        let engine = ProxyEngine::new([authority_of(UPSTREAM).unwrap()]);
        // Bypass `register`'s allowlist belt so `authorize`'s own allowlist check can be exercised in
        // isolation, including with a deliberately unapproved upstream.
        engine.creds.lock().unwrap().insert(
            placeholder.to_owned(),
            JobCredential {
                placeholder: placeholder.to_owned(),
                real: REAL.to_owned(),
                upstream: upstream.to_owned(),
            },
        );
        engine
    }

    fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Substring search over raw bytes, for asserting a credential is (or is not) present.
    fn contains(haystack: &[u8], needle: &str) -> bool {
        let needle = needle.as_bytes();
        !needle.is_empty()
            && needle.len() <= haystack.len()
            && haystack.windows(needle.len()).any(|w| w == needle)
    }

    // #647 acceptance #5: the minted placeholder is format-plausible (vendor prefix, vendor charset)
    // yet distinct from any real credential — a leak of it is harmless.
    #[test]
    fn placeholder_is_format_plausible_and_not_the_real_credential() {
        let ph = mint_anthropic_placeholder();
        assert!(ph.starts_with("sk-ant-api03-"), "vendor prefix: {ph}");
        assert!(ph.len() >= "sk-ant-api03-".len() + 80, "vendor-plausible length: {ph}");
        assert!(
            ph.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "vendor charset only: {ph}"
        );
        assert_ne!(ph, REAL, "the placeholder must not equal the real credential");
        assert_ne!(mint_anthropic_placeholder(), ph, "each job gets a fresh placeholder");
    }

    // #647 acceptance #1 (logic half): a request carrying the placeholder in the credential header is
    // authorized, the destination approved, and the placeholder rewritten to the REAL credential in
    // the outgoing headers — value-based, so `x-api-key` is matched without being named.
    #[test]
    fn authorize_substitutes_real_credential_for_an_approved_destination() {
        let ph = mint_anthropic_placeholder();
        let engine = engine_with_job(&ph, UPSTREAM);
        let headers = hdr(&[("x-api-key", &ph), ("anthropic-version", "2023-06-01")]);
        match engine.authorize(&headers) {
            Decision::Forward { upstream, headers, .. } => {
                assert_eq!(upstream, UPSTREAM);
                let api_key = headers.iter().find(|(n, _)| n == "x-api-key").unwrap();
                assert_eq!(api_key.1, REAL, "real credential must be substituted at egress");
                assert!(
                    !headers.iter().any(|(_, v)| v.contains(&ph)),
                    "no placeholder may survive into the forwarded request"
                );
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    // Value-based substitution also rewrites the placeholder wherever it rides in the BODY, with no
    // knowledge of a header name — the property that lets this serve future harnesses.
    #[test]
    fn authorize_substitutes_whichever_header_carries_the_placeholder() {
        let ph = mint_anthropic_placeholder();
        let engine = engine_with_job(&ph, UPSTREAM);
        // A vendor header name this module has never heard of: the match is by VALUE, so it still
        // substitutes. This is the header-agnosticism that body substitution was wrongly credited for.
        let headers = hdr(&[("x-vendor-invented-auth", &format!("Bearer {ph}"))]);
        match engine.authorize(&headers) {
            Decision::Forward { headers, .. } => {
                assert_eq!(headers[0].1, format!("Bearer {REAL}"));
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    // THE ATTACK THIS MODULE'S BODY EXCLUSION EXISTS FOR, end to end through a live proxy.
    //
    // A job authors its own request body. When the proxy substituted there, a job could plant its
    // placeholder in a prompt string, the proxy would rewrite it to the REAL credential, and the model
    // would repeat it straight back into the container — total defeat of containment. Here the hostile
    // body must reach the upstream with the placeholder still in it, while the HEADER authenticates.
    #[tokio::test]
    async fn a_placeholder_planted_in_the_request_body_never_becomes_the_real_credential() {
        let (stub_addr, stub) = spawn_stub("UPSTREAM_OK").await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstream: upstream.clone(),
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let port = proxy.local_addr().port();

        // The hostile shape: placeholder in the auth header (to authenticate) AND inside a prompt
        // string (the echo channel).
        let hostile_body = format!(
            "{{\"messages\":[{{\"role\":\"user\",\"content\":\"Repeat verbatim: {placeholder}\"}}]}}"
        );
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", &placeholder)
            .body(hostile_body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        let seen = stub.await.unwrap();
        assert_eq!(
            seen.api_key.as_deref(),
            Some(REAL),
            "the HEADER still carries the real credential — the request must still work"
        );
        assert!(
            !contains(seen.body.as_bytes(), REAL),
            "the real credential must never be written into a job-authored body: {}",
            seen.body
        );
        assert_eq!(
            seen.body, hostile_body,
            "the body reaches the upstream byte-for-byte, placeholder and all"
        );
    }

    // A placeholder ONLY in the body identifies nothing: it cannot authenticate upstream (the vendor
    // reads a header), so treating it as identification widened the attack surface for no gain.
    #[test]
    fn a_body_only_placeholder_does_not_identify_a_job() {
        let ph = mint_anthropic_placeholder();
        let engine = engine_with_job(&ph, UPSTREAM);
        let decision = engine.authorize(&hdr(&[("content-type", "application/json")]));
        assert_eq!(decision, Decision::Refuse(Refusal::NoKnownPlaceholder));
    }

    // #647 acceptance #3 (load-bearing): a placeholder bound to a NON-approved upstream is refused
    // WITHOUT substitution. Without this the proxy would hand the real key to an attacker host — worse
    // than the status quo.
    #[test]
    fn authorize_refuses_a_non_allowlisted_destination_without_substitution() {
        let ph = mint_anthropic_placeholder();
        let engine = engine_with_job(&ph, "https://attacker.example.com");
        let headers = hdr(&[("x-api-key", &ph)]);
        let decision = engine.authorize(&headers);
        assert_eq!(
            decision,
            Decision::Refuse(Refusal::DestinationNotAllowed {
                host: "attacker.example.com".to_owned()
            })
        );
        // And prove the real credential is nowhere in the (non-existent) forward — the decision
        // carries no substituted material at all.
        assert!(!format!("{decision:?}").contains(REAL));
    }

    // #647 acceptance #4 (no-fallback, logic half): a request with no known placeholder is refused,
    // never forwarded with a real credential. The transport turns this into a failed request.
    #[test]
    fn authorize_refuses_when_no_known_placeholder_is_present() {
        let ph = mint_anthropic_placeholder();
        let engine = engine_with_job(&ph, UPSTREAM);
        let headers = hdr(&[("x-api-key", "sk-ant-api03-someOTHERvaluenotregistered")]);
        assert_eq!(
            engine.authorize(&headers),
            Decision::Refuse(Refusal::NoKnownPlaceholder)
        );
    }

    // The registration belt refuses to admit an unapproved upstream into the registry at all.
    #[test]
    fn register_refuses_an_unapproved_upstream() {
        let engine = ProxyEngine::new([authority_of(UPSTREAM).unwrap()]);
        let bad = JobCredential {
            placeholder: mint_anthropic_placeholder(),
            real: REAL.to_owned(),
            upstream: "https://evil.example.com".to_owned(),
        };
        assert!(matches!(engine.register(bad), Err(Refusal::DestinationNotAllowed { .. })));
        let good = JobCredential {
            placeholder: mint_anthropic_placeholder(),
            real: REAL.to_owned(),
            upstream: UPSTREAM.to_owned(),
        };
        assert!(engine.register(good).is_ok());
    }

    #[test]
    fn allowlist_matches_across_default_port() {
        let engine = ProxyEngine::new(["api.anthropic.com".to_owned()]);
        assert!(engine.allows("api.anthropic.com"));
        assert!(engine.allows("api.anthropic.com:443"));
        assert!(!engine.allows("api.openai.com"));
    }

    #[test]
    fn authority_of_parses_scheme_host_port() {
        assert_eq!(authority_of("https://api.anthropic.com").as_deref(), Some("api.anthropic.com"));
        assert_eq!(authority_of("http://host.docker.internal:8080/v1").as_deref(),
            Some("host.docker.internal:8080"));
        assert_eq!(authority_of("not-a-url"), None);
    }

    // ── Transport integration (#647 acceptance #1, #3, #4 at the socket boundary) ────────────────

    #[derive(Debug)]
    struct StubSeen {
        request_line: String,
        api_key: Option<String>,
        /// The request BODY as the upstream received it — the ground truth for "no credential was
        /// written into a job-authored body".
        body: String,
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// A one-shot plain-HTTP stub upstream standing in for `api.anthropic.com`. It records the request
    /// line (to prove the path is forwarded verbatim) and the `x-api-key` it received (to prove the
    /// real credential was substituted), then answers `200` with `body_out`.
    async fn spawn_stub(body_out: impl Into<String>) -> (SocketAddr, tokio::task::JoinHandle<StubSeen>) {
        spawn_stub_with(body_out, None).await
    }

    /// [`spawn_stub`] plus an optional extra response header line (`"name: value"`), so a test can make
    /// the upstream reflect a credential in a HEADER as well as in the body.
    async fn spawn_stub_with(
        body_out: impl Into<String>,
        extra_header: Option<String>,
    ) -> (SocketAddr, tokio::task::JoinHandle<StubSeen>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let body_out = body_out.into();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if find_subslice(&buf, b"\r\n\r\n").is_some() {
                    break;
                }
            }
            // Read the body too: `content-length` bytes past the header terminator. Without this the
            // stub could not prove what the upstream actually received in the body.
            let head_end = find_subslice(&buf, b"\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
            let head_text = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let content_length = head_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while buf.len() < head_end + content_length {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = String::from_utf8_lossy(&buf[head_end..]).to_string();

            let text = head_text;
            let request_line = text.lines().next().unwrap_or_default().to_owned();
            let api_key = text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-api-key").then(|| value.trim().to_owned())
            });
            let extra = extra_header.map(|h| format!("{h}\r\n")).unwrap_or_default();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: text/plain\r\n{}\r\n{}",
                body_out.len(),
                extra,
                body_out
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            StubSeen { request_line, api_key, body }
        });
        (addr, handle)
    }

    fn arc_engine_with_job(placeholder: &str, upstream: &str) -> Arc<ProxyEngine> {
        Arc::new(engine_with_job(placeholder, upstream))
    }

    /// Drive one request through a live proxy against a stub upstream, returning `(headers, body)` as
    /// the CONTAINER sees them. Shared by the response-scrub tests.
    async fn round_trip(
        stub_body: impl Into<String>,
        stub_header: Option<String>,
    ) -> (reqwest::header::HeaderMap, String) {
        let (stub_addr, _stub) = spawn_stub_with(stub_body, stub_header).await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstream: upstream.clone(),
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let port = proxy.local_addr().port();
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", &placeholder)
            .body("{}")
            .send()
            .await
            .unwrap();
        let headers = response.headers().clone();
        (headers, response.text().await.unwrap())
    }

    // #797: a configured range binds INSIDE it, so the host firewall's pinhole can name the port.
    // Both legs matter — landing in the range is the property, and the unconfigured control is what
    // proves the assertion is not vacuously true of an ephemeral port that happened to fall there.
    #[tokio::test]
    async fn a_configured_port_range_binds_inside_it_and_an_unset_one_stays_ephemeral() {
        let engine = Arc::new(ProxyEngine::new(["api.anthropic.com".to_owned()]));
        let range = crate::sandbox_net::PortRange::new(49200, 49299).unwrap();
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), Some(range))
            .await
            .expect("a free port in the range");
        let port = proxy.local_addr().port();
        assert!(
            (range.start()..=range.end()).contains(&port),
            "a configured range must bind inside itself, or the firewall pinhole names the wrong \
             port and every contained job fails to reach its model: got {port}"
        );

        // A second proxy must take a DIFFERENT port in the range, not collide with the first: the
        // range is walked until a free port is found, and concurrent jobs each hold one for their
        // lifetime.
        let second = start(Arc::clone(&engine), reqwest::Client::new(), Some(range))
            .await
            .expect("a second free port in the range");
        assert_ne!(
            second.local_addr().port(),
            port,
            "two live proxies must not claim the same port"
        );

        // Control: unset ⇒ the shipped ephemeral behaviour. Without this the range assertion above
        // would also pass against a build that ignored the range entirely.
        let ephemeral = start(Arc::clone(&engine), reqwest::Client::new(), None)
            .await
            .expect("an ephemeral port");
        assert_ne!(ephemeral.local_addr().port(), 0, "the kernel must have chosen a real port");
    }

    // A fully-occupied range FAILS rather than falling back to an ephemeral port. A fallback would
    // put the listener outside the range the firewall pinhole names, so the job would fail to reach
    // its model with nothing naming the port as the cause.
    #[tokio::test]
    async fn an_exhausted_port_range_fails_instead_of_falling_back() {
        let engine = Arc::new(ProxyEngine::new(["api.anthropic.com".to_owned()]));
        // A single-port range, occupied by the first proxy.
        let occupied = tokio::net::TcpListener::bind((BIND_ADDRESS, 0)).await.unwrap();
        let taken = occupied.local_addr().unwrap().port();
        let range = crate::sandbox_net::PortRange::new(taken, taken).unwrap();

        // `expect_err` would need `Debug` on `RunningProxy`, which is merged #647 code this ticket
        // does not touch. Matching says the same thing without widening that type.
        let Err(error) = start(Arc::clone(&engine), reqwest::Client::new(), Some(range)).await
        else {
            panic!("an occupied single-port range must fail, not fall back to an ephemeral port");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(
            error.to_string().contains("proxy_port_range"),
            "the failure must name the config key an operator has to widen: {error}"
        );

        // Control: the same range binds once the port is free, so the failure above is about
        // occupancy and not about the range being malformed.
        drop(occupied);
        start(Arc::clone(&engine), reqwest::Client::new(), Some(range))
            .await
            .expect("the same range binds once the port is released");
    }

    // DEFENCE IN DEPTH on the return leg: an upstream that reflects the real credential in its response
    // BODY (a 401 quoting the key it rejected, a debug echo) must not hand it to the container. The
    // scrub rewrites it back to the worthless placeholder.
    #[tokio::test]
    async fn a_real_credential_echoed_in_the_response_body_is_scrubbed_to_the_placeholder() {
        let echoed = format!("upstream said: invalid key {REAL} — retry");
        let (_headers, body) = round_trip(echoed, None).await;
        assert!(
            !body.contains(REAL),
            "the real credential must never reach the container: {body}"
        );
        assert!(
            body.contains("sk-ant-api03-") && body.contains("upstream said: invalid key "),
            "the echo is rewritten in place, not dropped: {body}"
        );
    }

    // The same scrub applies to response HEADERS — `www-authenticate` and friends reflect credentials.
    #[tokio::test]
    async fn a_real_credential_echoed_in_a_response_header_is_scrubbed() {
        let (headers, _body) =
            round_trip("ok", Some(format!("www-authenticate: Bearer key={REAL}"))).await;
        let seen = headers
            .get("www-authenticate")
            .expect("stub set the header")
            .to_str()
            .unwrap();
        assert!(!seen.contains(REAL), "real credential leaked in a response header: {seen}");
        assert!(seen.contains("sk-ant-api03-"), "rewritten to the placeholder: {seen}");
    }

    // The scrub must survive CHUNK BOUNDARIES. A credential split across two stream frames would slip
    // past a naive per-chunk replace, so `scrub_stream` holds back `len(real) - 1` bytes and re-checks.
    // Driven directly with hand-split frames, since a stub cannot reliably force the split.
    #[tokio::test]
    async fn the_response_scrub_catches_a_credential_split_across_chunks() {
        use futures_util::TryStreamExt as _;
        let placeholder = mint_anthropic_placeholder();
        let (head, tail) = REAL.split_at(20);
        // Three frames: the credential straddles frames 1→2, and frame 3 is ordinary trailing data.
        let frames: Vec<reqwest::Result<Bytes>> = vec![
            Ok(Bytes::from(format!("prefix-{head}"))),
            Ok(Bytes::from(format!("{tail}-middle"))),
            Ok(Bytes::from_static(b"-suffix")),
        ];
        let scrubbed = scrub_stream(
            futures_util::stream::iter(frames),
            REAL.to_owned(),
            placeholder.clone(),
        );
        let collected: Vec<u8> = scrubbed
            .try_fold(Vec::new(), |mut acc, frame| async move {
                if let Ok(data) = frame.into_data() {
                    acc.extend_from_slice(&data);
                }
                Ok(acc)
            })
            .await
            .unwrap();
        let text = String::from_utf8(collected).unwrap();
        assert_eq!(
            text,
            format!("prefix-{placeholder}-middle-suffix"),
            "a split credential must still be rewritten, and nothing else may be lost"
        );
        assert!(!text.contains(REAL), "no fragment of the real credential survives");
    }

    // #647 acceptance #1 (plumbing): a request carrying the placeholder is forwarded to the approved
    // upstream with the REAL credential substituted, the path unchanged, and the upstream's response
    // streamed back to the caller.
    #[tokio::test]
    async fn proxy_substitutes_and_forwards_to_an_approved_upstream() {
        let (stub_addr, stub) = spawn_stub("UPSTREAM_OK").await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstream: upstream.clone(),
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let port = proxy.local_addr().port();

        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages?beta=true"))
            .header("x-api-key", &placeholder)
            .body("{\"model\":\"claude\"}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "UPSTREAM_OK");

        let seen = stub.await.unwrap();
        assert_eq!(
            seen.api_key.as_deref(),
            Some(REAL),
            "the stub must receive the REAL credential, not the placeholder"
        );
        assert!(
            seen.request_line.contains("/v1/messages?beta=true"),
            "the path+query must be forwarded verbatim: {}",
            seen.request_line
        );
        assert!(
            !seen.request_line.contains(&placeholder) && !seen.request_line.contains(REAL),
            "no credential may ever appear in the URL path: {}",
            seen.request_line
        );
    }

    // #647 acceptance #3 at the socket: a placeholder bound to a non-allowlisted destination gets a
    // 403 and is never forwarded.
    #[tokio::test]
    async fn proxy_returns_403_for_a_non_allowlisted_destination() {
        let placeholder = mint_anthropic_placeholder();
        let engine = arc_engine_with_job(&placeholder, "https://attacker.example.com");
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let port = proxy.local_addr().port();
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", &placeholder)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }

    // #647 acceptance #4 at the socket (no-fallback): a request with no known placeholder is refused
    // (502) — the proxy never forwards a real credential it cannot attribute to a job.
    #[tokio::test]
    async fn proxy_refuses_when_no_known_placeholder_is_present() {
        let engine = Arc::new(ProxyEngine::new([authority_of(UPSTREAM).unwrap()]));
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let port = proxy.local_addr().port();
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", "sk-ant-api03-neverRegisteredValue")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 502);
    }

    // P1 regression (babu review): the listener lives in the money-path daemon, so a body over the cap
    // must be REFUSED (413), not buffered. `Limited` stops reading past `MAX_REQUEST_BODY_BYTES`, so
    // the proxy never holds more than the cap — the daemon stays under a fixed memory ceiling no matter
    // how large the body a stranger's job sends. (Cap applies BEFORE authorize, so no registration is
    // needed to reach it.)
    #[tokio::test]
    async fn proxy_rejects_an_over_cap_body_with_413() {
        let engine = Arc::new(ProxyEngine::new([authority_of(UPSTREAM).unwrap()]));
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let port = proxy.local_addr().port();
        let over_cap = vec![b'a'; MAX_REQUEST_BODY_BYTES + 1];
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .body(over_cap)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 413, "an over-cap body must be refused, not buffered");
    }

    // A redirect that stays on the credential's own upstream is followed, so a provider's own
    // cross-path redirect keeps working. Default-port equivalence is asserted in BOTH directions,
    // because the allowlist and this check now share one comparison and must not diverge on it.
    #[test]
    fn redirect_that_stays_on_the_paired_upstream_is_approved() {
        for target in [
            "https://api.anthropic.com/v1/messages",
            "https://api.anthropic.com:443/v1/messages",
            "https://API.Anthropic.COM/v1/messages",
            "https://api.anthropic.com/v1/messages?next=%2Fother",
        ] {
            assert!(
                allows_paired_redirect(UPSTREAM, target),
                "a target on the paired upstream must be approved: {target}"
            );
        }
        assert!(
            allows_paired_redirect("https://api.anthropic.com:443", "https://api.anthropic.com/v1/messages"),
            "an upstream carrying the default port must still approve the bare host"
        );
    }

    // The attacker is the hired agent: it controls the full path and query of every proxied request,
    // so it can attempt an open redirect on a host the seller genuinely trusts. The substring case is
    // the one a naive matcher gets wrong.
    #[test]
    fn redirect_off_the_paired_upstream_is_refused() {
        for target in [
            "https://evil.example.com/collect",
            "https://api.anthropic.com.evil.example/v1/messages",
            "https://evil.example.com/?next=https://api.anthropic.com",
            "https://api.anthropic.com:8443/v1/messages",
            "http://169.254.169.254/latest/meta-data/",
        ] {
            assert!(
                !allows_paired_redirect(UPSTREAM, target),
                "a target off the paired upstream must be refused: {target}"
            );
        }
    }

    // THE CASE THIS FUNCTION EXISTS FOR, and the one an allowlist-membership check cannot see. Both
    // hosts are registered upstreams, so BOTH are on the engine's allowlist, while the credential in
    // flight belongs to exactly one of them. The first assertion is the positive control: it shows the
    // union question genuinely answers YES here, so the refusal below is doing real work.
    #[test]
    fn redirect_to_a_different_registered_upstream_is_refused() {
        let engine = ProxyEngine::new([
            authority_of(ANTHROPIC_DEFAULT_UPSTREAM).unwrap(),
            authority_of(OPENAI_DEFAULT_UPSTREAM).unwrap(),
        ]);
        assert!(
            engine.allows(&authority_of(OPENAI_DEFAULT_UPSTREAM).unwrap()),
            "control: a union check approves the cross-vendor host, which is why it cannot gate a redirect"
        );

        assert!(
            !allows_paired_redirect(ANTHROPIC_DEFAULT_UPSTREAM, "https://api.openai.com/v1/chat/completions"),
            "a credential registered for Anthropic must not follow a redirect to another registered upstream"
        );
        assert!(
            !allows_paired_redirect(OPENAI_DEFAULT_UPSTREAM, "https://api.anthropic.com/v1/messages"),
            "and symmetrically, so the guard is not written from one vendor's shape"
        );
    }

    // Every hop is judged against the ORIGINAL upstream, never its predecessor. reqwest pushes the
    // previous URL onto an accumulating chain before calling the policy, so `previous()[0]` is the
    // original request URL on every hop — which is what makes the pairing hold past hop one. The two
    // assertions differ ONLY in which operand is treated as the original, so they show the choice of
    // operand changing the answer: judging hop-against-predecessor would walk to any host in two steps.
    #[test]
    fn a_redirect_chain_is_judged_against_the_original_not_the_previous_hop() {
        assert!(
            allows_paired_redirect("https://mid.example.com/step", "https://mid.example.com/next"),
            "control: measured against the PREVIOUS hop, a second hop on that hop's host is same-authority"
        );
        assert!(
            !allows_paired_redirect(UPSTREAM, "https://mid.example.com/next"),
            "measured against the ORIGINAL upstream, the same hop must be refused"
        );
    }

    // An endpoint whose authority cannot be resolved is refused rather than parsed loosely: the
    // credential must not move to a destination the proxy cannot name. Asserted on BOTH operands,
    // because an empty redirect chain reaches the decision as an unresolvable `original`.
    #[test]
    fn an_unnameable_redirect_endpoint_is_refused() {
        for target in ["", "api.anthropic.com/v1/messages", "https://", "https:///v1/messages"] {
            assert!(
                !allows_paired_redirect(UPSTREAM, target),
                "an unresolvable target must be refused: {target:?}"
            );
        }
        assert!(
            !allows_paired_redirect("", "https://api.anthropic.com/v1/messages"),
            "an unresolvable original must be refused, so an empty chain cannot fail open"
        );
    }

    /// RFC 4648 vectors, unpadded. The encoder is hand-rolled to keep the JWT placeholder off the
    /// crate's optional `base64` feature, so it gets pinned against known values rather than trusted.
    #[test]
    fn base64url_matches_rfc4648_vectors_unpadded() {
        for (raw, expected) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url(raw.as_bytes()), expected, "input {raw:?}");
        }
    }

    /// The url-safe alphabet is the point: a `+` or `/` would make the segment standard base64, which
    /// is not a well-formed JWT segment. These two bytes select exactly the last two code points.
    #[test]
    fn base64url_uses_the_url_safe_alphabet_not_standard() {
        assert_eq!(base64url(&[0xFB, 0xFF]), "-_8");
    }

    #[test]
    fn mint_jwt_placeholder_is_three_unpadded_base64url_segments() {
        let token = mint_jwt_placeholder("session", std::time::Duration::from_secs(30 * 86_400));
        let segments: Vec<&str> = token.split('.').collect();
        assert_eq!(segments.len(), 3, "a JWT is three dot-separated segments: {token}");
        for segment in &segments {
            assert!(!segment.is_empty(), "no segment may be empty: {token}");
            assert!(
                segment.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "segment {segment} must be unpadded base64url — a padded or standard-alphabet \
                 segment is refused by a client that parses before sending"
            );
        }
    }

    /// Per-job, never a constant: a fixed placeholder carries a fixed `exp`, so it would work for
    /// months and then start failing as an auth error nobody traces back to the placeholder.
    #[test]
    fn mint_jwt_placeholder_differs_per_call() {
        let first = mint_jwt_placeholder("session", std::time::Duration::from_secs(3600));
        let second = mint_jwt_placeholder("session", std::time::Duration::from_secs(3600));
        assert_ne!(first, second, "each job must get its own placeholder");
    }

    #[test]
    fn mint_jwt_placeholder_exp_is_in_the_future() {
        let ttl = 30 * 86_400;
        let token = mint_jwt_placeholder("session", std::time::Duration::from_secs(ttl));
        let payload = token.split('.').nth(1).expect("three segments");
        let claims = String::from_utf8(decode_base64url(payload)).expect("payload is utf-8 json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs();
        let exp: u64 = claims
            .split("\"exp\":")
            .nth(1)
            .and_then(|tail| tail.trim_end_matches('}').trim().parse().ok())
            .unwrap_or_else(|| panic!("payload must carry a numeric exp: {claims}"));
        assert!(
            exp > now,
            "exp must be in the future or the client refuses it locally: exp={exp} now={now}"
        );
        assert!(
            exp <= now + ttl + 5,
            "exp must reflect the requested ttl, not an unbounded far future: exp={exp} now={now}"
        );
        assert!(claims.contains("\"type\":\"session\""), "claim shape is mirrored: {claims}");
    }

    /// Test-only decoder, so the exp assertion reads the real bytes rather than trusting the encoder.
    fn decode_base64url(encoded: &str) -> Vec<u8> {
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut bits = 0u32;
        let mut width = 0u32;
        let mut out = Vec::new();
        for byte in encoded.bytes() {
            let index = CHARSET
                .iter()
                .position(|candidate| *candidate == byte)
                .unwrap_or_else(|| panic!("not base64url: {byte:?}")) as u32;
            bits = (bits << 6) | index;
            width += 6;
            if width >= 8 {
                width -= 8;
                out.push(((bits >> width) & 0xFF) as u8);
            }
        }
        out
    }

    // ---- h2c: a client may open its leg with the HTTP/2 preface, not an HTTP/1 request line ------
    //
    // Measured on `cursor-agent 2026.08.11-e8db854`: its agent/inference endpoint opens with the
    // HTTP/2 prior-knowledge preface. An http1-only server cannot parse that and surfaces NO request,
    // so the leg reads as "nothing ever connected" rather than as a protocol mismatch — which is
    // exactly why this went undiagnosed: the proxy's own log stays clean, because traffic that never
    // arrives leaves no trace in it.
    //
    // The assertion is on the WIRE, not on a client library, so it cannot pass by a client quietly
    // falling back to HTTP/1. `PRI * HTTP/2.0` is itself a well-formed HTTP/1 request line with an
    // unacceptable version, so an http1-only server answers `HTTP/1.1 400` — a reply, just the wrong
    // protocol. Checking "did we get bytes back" would therefore pass on stock; only the frame type
    // separates them.
    #[tokio::test]
    async fn the_proxy_accepts_an_h2c_prior_knowledge_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        // 9-byte frame header, zero-length payload: len=0, type=0x04 (SETTINGS), flags=0, stream=0.
        const EMPTY_SETTINGS: &[u8] = &[0, 0, 0, 4, 0, 0, 0, 0, 0];
        const FRAME_TYPE_SETTINGS: u8 = 0x04;

        let engine = Arc::new(ProxyEngine::new([authority_of("http://127.0.0.1:1").unwrap()]));
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();

        let mut stream = tokio::net::TcpStream::connect(proxy.local_addr()).await.unwrap();
        stream.write_all(H2_PREFACE).await.unwrap();
        stream.write_all(EMPTY_SETTINGS).await.unwrap();
        stream.flush().await.unwrap();

        let mut head = [0_u8; 9];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut head))
            .await
            .expect("an h2c server answers its own SETTINGS; a timeout means the preface was dropped")
            .expect("connection closed instead of answering the preface");

        assert!(
            !head.starts_with(b"HTTP/1"),
            "answered in HTTP/1, so the preface was parsed as an HTTP/1 request line: {:?}",
            String::from_utf8_lossy(&head)
        );
        assert_eq!(
            head[3], FRAME_TYPE_SETTINGS,
            "expected an HTTP/2 SETTINGS frame (type 0x04) as the server preface, got {head:?}"
        );
    }

    // The HTTP/1 providers must keep working on the SAME listener — an auto-negotiating server that
    // silently stopped serving HTTP/1 would break every currently-working harness while making the
    // h2c test above pass. Two protocols, one port, both asserted.
    #[tokio::test]
    async fn the_proxy_still_serves_http1_on_the_same_listener() {
        let (stub_addr, stub) = spawn_stub("UPSTREAM_OK").await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstream: upstream.clone(),
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let port = proxy.local_addr().port();

        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", &placeholder)
            .body("{}")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 200, "HTTP/1 must still be served");
        let seen = stub.await.unwrap();
        assert_eq!(
            seen.api_key.as_deref(),
            Some(REAL),
            "and substitution must still happen on the HTTP/1 path"
        );
    }


    // An h2c connection that completes a SETTINGS exchange is not the same thing as an h2c REQUEST
    // that gets served, forwarded and substituted. The preface test above proves only the former, and
    // a proxy that negotiates h2 then fails every h2 request would pass it while contained jobs still
    // failed — so the request path is asserted end to end, on the protocol the agent leg actually
    // speaks, with the same substitution check the HTTP/1 test makes.
    #[tokio::test]
    async fn an_h2c_request_is_served_forwarded_and_substituted() {
        let (stub_addr, stub) = spawn_stub("UPSTREAM_OK").await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstream: upstream.clone(),
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let proxy_addr = proxy.local_addr();

        let io = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        let (send, connection) = h2::client::handshake(io).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let mut send = send.ready().await.unwrap();
        let request = hyper::Request::builder()
            .method("POST")
            .uri(format!("http://{proxy_addr}/v1/messages"))
            .header("x-api-key", &placeholder)
            .body(())
            .unwrap();
        let (response, mut body) = send.send_request(request, false).unwrap();
        body.reserve_capacity(2);
        body.send_data(bytes::Bytes::from_static(b"{}"), true).unwrap();

        let response = tokio::time::timeout(Duration::from_secs(20), response)
            .await
            .expect("the proxy must answer an h2 request, not hang")
            .expect("the h2 stream must not be reset");
        assert!(
            response.status().is_success(),
            "an h2c request must be served, got {}",
            response.status()
        );

        let seen = stub.await.unwrap();
        assert_eq!(
            seen.api_key.as_deref(),
            Some(REAL),
            "substitution must happen on the h2 path exactly as it does on HTTP/1"
        );
    }

    // The aggregate ceiling must survive MULTIPLEXING. `MAX_CONCURRENT_CONNECTIONS` bounds sockets and
    // `MAX_CONCURRENT_H2_STREAMS` bounds streams within one socket; under HTTP/1 those coincide with
    // requests, under HTTP/2 they MULTIPLY. Two connections at the stream cap can attempt 128 requests
    // against a limit of 64, and at `MAX_REQUEST_BODY_BYTES` each that is the resource hazard the
    // connection cap exists to prevent.
    //
    // Deterministic, not timing-based: every request parks at a stub that answers nobody until told
    // to, so the assertion is "exactly the ceiling arrives, the overflow does not, and releasing one
    // admits exactly one more". A `sleep` never decides the outcome.
    #[tokio::test]
    async fn h2_multiplexing_cannot_exceed_the_aggregate_in_flight_ceiling() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::mpsc;

        const OVERFLOW: usize = 4;
        let attempts = MAX_IN_FLIGHT_REQUESTS + OVERFLOW;

        let (arrived_tx, mut arrived_rx) = mpsc::unbounded_channel::<()>();
        let (release_tx, release_rx) = mpsc::unbounded_channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(release_rx));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let arrived_tx = arrived_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                tokio::spawn(async move {
                    let mut buf = [0_u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let _ = arrived_tx.send(());
                    // Parked here, still holding the proxy's in-flight permit.
                    let _ = release_rx.lock().await.recv().await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                        .await;
                });
            }
        });

        let upstream = format!("http://{upstream_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstream: upstream.clone(),
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), reqwest::Client::new(), None).await.unwrap();
        let proxy_addr = proxy.local_addr();

        // TWO connections, so the per-connection stream cap cannot be what bounds the total.
        let mut senders = Vec::new();
        for _ in 0..2 {
            let io = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
            let (send, connection) = h2::client::handshake(io).await.unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });
            senders.push(send);
        }

        let mut responses = Vec::new();
        for i in 0..attempts {
            let mut send = senders[i % senders.len()].clone().ready().await.unwrap();
            let request = hyper::Request::builder()
                .method("POST")
                .uri(format!("http://{proxy_addr}/v1/messages"))
                .header("x-api-key", &placeholder)
                .body(())
                .unwrap();
            let (response, mut body) = send.send_request(request, false).unwrap();
            // Required before `send_data`, or h2 breaks the stream on a flow-control violation.
            body.reserve_capacity(2);
            body.send_data(bytes::Bytes::from_static(b"{}"), true).unwrap();
            responses.push(response);
        }

        // Exactly the ceiling reaches the upstream.
        for n in 0..MAX_IN_FLIGHT_REQUESTS {
            tokio::time::timeout(Duration::from_secs(20), arrived_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("only {n} of {MAX_IN_FLIGHT_REQUESTS} reached upstream"))
                .expect("stub channel closed");
        }
        // The overflow does NOT, while those permits are held. This is the assertion that fails
        // without a shared limiter: 68 multiplexed requests would all be in flight at once.
        assert!(
            tokio::time::timeout(Duration::from_millis(750), arrived_rx.recv())
                .await
                .is_err(),
            "a request beyond MAX_IN_FLIGHT_REQUESTS reached the upstream: {attempts} were \
             multiplexed over 2 connections, so the ceiling is per-connection, not aggregate"
        );

        // Releasing one admits exactly one more — a queue, not a drop.
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(20), arrived_rx.recv())
            .await
            .expect("releasing one in-flight request must admit the next")
            .expect("stub channel closed");

        for _ in 0..attempts {
            let _ = release_tx.send(());
        }
    }
}
