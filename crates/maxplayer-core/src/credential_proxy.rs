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
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
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
/// There is deliberately **no request-body size limit**. The body is relayed as it arrives and never
/// accumulates, so its size is bounded by the network rather than by memory — see [`handle_request`].
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
/// each running 64 streams is 4096 requests in flight, each holding an upstream connection and a
/// credential — the very resource hazard the connection cap exists to prevent. The aggregate ceiling
/// is [`MAX_IN_FLIGHT_REQUESTS`]; this constant only stops one connection from monopolising it.
const MAX_CONCURRENT_H2_STREAMS: u32 = 64;
/// Requests in flight across the whole listener, both protocols, whatever the connection count.
///
/// The real ceiling on concurrent work, and the only one of the three that does not change meaning
/// with the protocol. [`MAX_CONCURRENT_CONNECTIONS`] bounds sockets and
/// [`MAX_CONCURRENT_H2_STREAMS`] bounds streams within one socket; under HTTP/1 those coincide with
/// requests, and under HTTP/2 they multiply. **Bounding the product is what keeps the count of
/// simultaneously-live upstream connections honest** — without it, adding h2c support would have
/// silently widened a limit nobody edited, which is exactly the failure mode this module's other
/// bounds exist to avoid.
///
/// This is now the ONLY aggregate bound on concurrent work: with the body streamed rather than
/// buffered, an in-flight request costs a socket pair and a credential in a header, not megabytes.
const MAX_IN_FLIGHT_REQUESTS: usize = 64;
/// How long the head (request line + headers) may take to arrive. hyper enforces this itself; without
/// it a trickled header stream pins a connection open indefinitely.
///
/// HTTP/1 only: HTTP/2 has no equivalent head-read deadline to set, so the body deadline and the
/// stream bound above are what bound a slow HTTP/2 peer.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the request body may go **silent** before the relay gives up on it.
///
/// This is an INACTIVITY deadline, not a total one, and the distinction is the whole point. A total
/// deadline cannot tell a stalled client from a healthy long upload: both exceed it, so any value
/// large enough to permit the second is too large to catch the first. Measuring the GAP between chunks
/// separates them — a client that keeps sending is never cut off however long it takes, and one that
/// stops is dropped `BODY_IDLE_TIMEOUT` later regardless of how much it already sent.
///
/// It bounds only the REQUEST read. The upstream RESPONSE (an SSE stream) is relayed with no deadline
/// at all, so a long completion is never cut off.
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// The largest request body one job may relay through an approved credential.
///
/// The proxy listener terminates INSIDE the seller daemon (a money-path process), and the caller is a
/// stranger's job. This bounds what one job can push through it.
///
/// The body is no longer buffered, so this is no longer a bound on memory — [`size_bounded`] counts
/// the stream and fails the request the moment it passes the cap, holding one chunk either way. It is
/// kept because the OOM ceiling was never the only thing it did: it is also the ceiling on what a
/// stranger's job may send upstream on the seller's credential. 32 MiB is far above any real model
/// request.
pub const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Period of the response-body heartbeat: [`BodyMeter`] emits a log-only `body_idle` line this often
/// while the upstream response body is producing no bytes.
///
/// It is a CLOCK, not a deadline — contrast [`BODY_IDLE_TIMEOUT`], which cancels the request body. The
/// heartbeat has no power to cancel anything; it only EMITS, so it cannot manufacture the terminator it
/// exists to observe. Its whole purpose is to turn "nothing arrived for N seconds" from an ABSENCE — a
/// reading indistinguishable from an instrument that was never reached — into an explicit line with a
/// denominator. That is the exact failure under investigation: a response header, then silence.
const BODY_HEARTBEAT_PERIOD: Duration = Duration::from_secs(5);

/// Inactivity after which [`scrub_stream`] flushes the provably-safe part of its holdback instead of
/// waiting for an upstream EOF that a stalled response never sends. Unlike [`BODY_IDLE_TIMEOUT`] this
/// CANCELS nothing: it releases only bytes that cannot begin a split credential (see
/// [`straddle_prefix_len`]) and keeps holding the trailing bytes that could. So the value bounds how long
/// a stalled body is withheld; it is never a containment knob — any value is equally safe.
const RESPONSE_HOLDBACK_FLUSH_IDLE: Duration = Duration::from_secs(15);

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
/// stands in for, and the approved upstream base URLs that credential may be substituted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCredential {
    /// The format-plausible value inside the container (`sk-ant-…`). Worthless if it leaks.
    pub placeholder: String,
    /// The real credential. Never enters the container; held only in this host process.
    pub real: String,
    /// The approved real upstream base URLs (scheme + host[:port]) this credential is valid for.
    ///
    /// The FIRST entry is the primary: it is where [`ProxyEngine::authorize`] — the primary
    /// listener's path — forwards, exactly as a single-upstream credential always has. Every entry
    /// past the first exists for a client whose separate legs go to separate hosts (measured on
    /// cursor-agent: `--endpoint` moves the control plane and `--agent-endpoint` moves the agent
    /// leg, and the two legs' true destinations differ). Each such entry is reachable only through
    /// [`ProxyEngine::authorize_via`] from the leg listener bound for its authority.
    ///
    /// [`ProxyEngine::register`] refuses an empty list and refuses two entries naming the same
    /// authority, so order inside the list never has to break a tie: at most one entry can match
    /// any listener.
    pub upstreams: Vec<String>,
}

/// One Codex ChatGPT session whose two required headers must move as one unit.
///
/// This type has no `Debug` implementation. Both real fields must stay out of logs and errors.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexSessionCredential {
    pub access_placeholder: String,
    pub access_token: String,
    pub account_placeholder: String,
    pub account_id: String,
    pub upstream: String,
}

/// Why a request was refused. Each variant means the real credential was NOT substituted — the
/// request either failed outright or was forwarded with the (worthless) placeholder untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No registered placeholder was found anywhere in the request. The proxy cannot identify a job,
    /// so it will not substitute any credential (no-fallback): the request fails.
    NoKnownPlaceholder,
    /// A placeholder was found, but the resolved destination host is not on the allowlist — or, on a
    /// leg listener, not on the identified credential's own upstream list. The real credential is
    /// withheld and the request is refused — the load-bearing invariant that keeps this from being
    /// worse than the status quo.
    DestinationNotAllowed { host: String },
    /// A request carried a known Codex access placeholder but did not carry both exact auth headers.
    InvalidCodexHeaders,
    /// A known Codex session tried a method or path outside its narrow backend route.
    RequestNotAllowed { method: String, path: String },
    /// A credential was offered for registration with no upstream at all. Refused at registration so
    /// the registry can never hold a credential with nowhere approved to go.
    NoUpstream,
    /// Two entries of one credential's upstream list resolve to the same authority. Refused rather
    /// than tie-broken: a duplicate is a config error, and any tie-break rule would silently pick a
    /// base URL the operator may not have meant (`https://a` vs `https://a:443/v2`).
    DuplicateUpstream { host: String },
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
            Self::InvalidCodexHeaders => write!(
                f,
                "Codex session request did not carry both exact per-job placeholder headers"
            ),
            Self::RequestNotAllowed { method, path } => {
                write!(f, "Codex session request {method} {path} is not allowed")
            }
            Self::NoUpstream => {
                write!(f, "credential lists no upstream; refusing registration")
            }
            Self::DuplicateUpstream { host } => {
                write!(f, "credential lists {host} more than once; refusing registration")
            }
        }
    }
}

/// The outcome of authorizing one request against the engine: either a forward plan whose HEADERS
/// carry the real credential (destination approved) or a typed [`Refusal`].
#[derive(Clone, PartialEq, Eq)]
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
        /// Real-to-placeholder substitutions for the response leg only.
        scrub: Vec<(String, String)>,
        /// The redirect rule for this credential type.
        redirects: RedirectMode,
    },
    Refuse(Refusal),
}

/// The redirect rule for one authorized request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectMode {
    /// Follow redirects only while they stay on the credential's paired authority.
    PairedAuthority,
    /// Return the redirect response without another upstream request.
    Disabled,
}

impl std::fmt::Debug for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forward {
                upstream,
                headers,
                scrub,
                redirects,
            } => f
                .debug_struct("Forward")
                .field("upstream", upstream)
                .field("header_count", &headers.len())
                .field("scrub_count", &scrub.len())
                .field("redirects", redirects)
                .finish(),
            Self::Refuse(reason) => f.debug_tuple("Refuse").field(reason).finish(),
        }
    }
}

/// The credential-substitution core: the destination allowlist plus the live per-job registry. Pure
/// decision logic ([`Self::authorize`]) is separated from all socket I/O so the invariants are
/// unit-testable without a container, a network, or a real credential.
#[derive(Default)]
pub struct ProxyEngine {
    /// Approved upstream hosts (lowercased, `host` or `host:port`). The real credential is substituted
    /// only when the resolved destination's host is in this set.
    allowlist: Vec<String>,
    /// Registered per-job credentials, keyed by placeholder value for O(1) identification.
    creds: Mutex<HashMap<String, JobCredential>>,
    /// Typed Codex sessions, keyed by the access placeholder that identifies the job.
    codex_sessions: Mutex<HashMap<String, CodexSessionCredential>>,
}

impl ProxyEngine {
    /// A new engine whose allowlist is exactly `hosts` (each normalized to a lowercased authority).
    pub fn new(hosts: impl IntoIterator<Item = String>) -> Self {
        Self {
            allowlist: hosts.into_iter().map(|h| host_key(&h)).collect(),
            creds: Mutex::new(HashMap::new()),
            codex_sessions: Mutex::new(HashMap::new()),
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

    /// Register a job's credential. Refuses (returns `Err`) unless EVERY listed upstream is a
    /// parseable base URL on the allowlist — a belt that keeps an unapproved destination from ever
    /// entering the registry, so a registration bug cannot defeat the [`Self::authorize`] allowlist
    /// check downstream. An empty list and two entries naming the same authority are refused here
    /// too, which is what lets every later lookup treat the list as non-empty and tie-free.
    pub fn register(&self, cred: JobCredential) -> Result<(), Refusal> {
        if cred.upstreams.is_empty() {
            return Err(Refusal::NoUpstream);
        }
        let mut authorities: Vec<String> = Vec::with_capacity(cred.upstreams.len());
        for upstream in &cred.upstreams {
            let host = authority_of(upstream)
                .ok_or_else(|| Refusal::DestinationNotAllowed { host: upstream.clone() })?;
            if !self.allows(&host) {
                return Err(Refusal::DestinationNotAllowed { host });
            }
            if authorities.iter().any(|seen| same_authority(seen, &host)) {
                return Err(Refusal::DuplicateUpstream { host });
            }
            authorities.push(host);
        }
        self.creds.lock().unwrap().insert(cred.placeholder.clone(), cred);
        Ok(())
    }

    /// Drop a job's credential (job end = revocation point). Returns whether one was present.
    pub fn deregister(&self, placeholder: &str) -> bool {
        self.creds.lock().unwrap().remove(placeholder).is_some()
    }

    /// Register one two-header Codex session. The access placeholder is the sole route identifier.
    pub fn register_codex_session(&self, cred: CodexSessionCredential) -> Result<(), Refusal> {
        let host = authority_of(&cred.upstream).ok_or_else(|| Refusal::DestinationNotAllowed {
            host: cred.upstream.clone(),
        })?;
        if !self.allows(&host) {
            return Err(Refusal::DestinationNotAllowed { host });
        }
        if [
            cred.access_placeholder.as_str(),
            cred.access_token.as_str(),
            cred.account_placeholder.as_str(),
            cred.account_id.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            return Err(Refusal::InvalidCodexHeaders);
        }
        let placeholders = [&cred.access_placeholder, &cred.account_placeholder];
        let real_values = [&cred.access_token, &cred.account_id];
        if placeholders
            .iter()
            .any(|placeholder| real_values.contains(placeholder))
        {
            return Err(Refusal::InvalidCodexHeaders);
        }
        self.codex_sessions
            .lock()
            .unwrap()
            .insert(cred.access_placeholder.clone(), cred);
        Ok(())
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
        self.decide(headers, |cred| match cred.upstreams.first() {
            Some(upstream) => Ok(upstream.clone()),
            // Unreachable through `register`, which refuses an empty list. Refused rather than
            // panicked so a future construction path turns a config error into a 4xx, not a crash.
            None => Err(Refusal::NoUpstream),
        })
    }

    /// The decision for one request arriving on the LEG listener bound for `via_authority`.
    ///
    /// Same steps as [`Self::authorize`], one difference: the destination is the identified
    /// credential's OWN entry whose authority matches `via_authority`. The listener the request
    /// physically arrived on is the selector — the container picks a destination only by which of
    /// the base URLs it was handed it dials, every one of which this host chose. A credential that
    /// never listed this authority is refused: a leg listener serves exactly the credentials that
    /// named its leg, so one job's extra leg never widens where another job's credential may go.
    pub fn authorize_via(&self, headers: &[(String, String)], via_authority: &str) -> Decision {
        let via = host_key(via_authority);
        self.decide(headers, move |cred| {
            cred.upstreams
                .iter()
                .find(|u| authority_of(u).is_some_and(|a| same_authority(&a, &via)))
                .cloned()
                .ok_or(Refusal::DestinationNotAllowed { host: via })
        })
    }

    /// Shared spine of [`Self::authorize`] and [`Self::authorize_via`]: identify the job by its
    /// placeholder, let `select` pick the destination from the credential itself, hold that
    /// destination to the allowlist, then substitute in header values only.
    fn decide(
        &self,
        headers: &[(String, String)],
        select: impl FnOnce(&JobCredential) -> Result<String, Refusal>,
    ) -> Decision {
        let creds = self.creds.lock().unwrap();
        let Some(cred) = creds
            .values()
            .find(|c| placeholder_present(&c.placeholder, headers))
            .cloned()
        else {
            return Decision::Refuse(Refusal::NoKnownPlaceholder);
        };
        drop(creds);

        let upstream = match select(&cred) {
            Ok(upstream) => upstream,
            Err(refusal) => return Decision::Refuse(refusal),
        };
        let Some(host) = authority_of(&upstream) else {
            return Decision::Refuse(Refusal::DestinationNotAllowed { host: upstream });
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
            upstream,
            headers,
            scrub: vec![(cred.real, cred.placeholder)],
            redirects: RedirectMode::PairedAuthority,
        }
    }

    /// Authorize a transport request. A typed Codex session takes the narrow route; all other
    /// requests use the existing generic credential behavior.
    ///
    /// `via_authority` is the LEG the request arrived on: `None` for the primary listener, and the
    /// leg's authority for a leg listener. It selects which generic entry point decides — see
    /// [`Self::authorize_via`]. A typed Codex session is checked BEFORE either and is not routed by a
    /// leg: its destination is the one fixed upstream its session was registered for, so a leg listener
    /// carries no Codex session and falls through to the generic path exactly as it did before.
    pub fn authorize_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        via_authority: Option<&str>,
    ) -> Decision {
        let sessions = self.codex_sessions.lock().unwrap();
        let matched = sessions
            .values()
            .find(|session| {
                headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("authorization")
                        && value == &format!("Bearer {}", session.access_placeholder)
                })
            })
            .cloned();
        drop(sessions);
        let Some(session) = matched else {
            return match via_authority {
                None => self.authorize(headers),
                Some(authority) => self.authorize_via(headers, authority),
            };
        };

        if !codex_request_allowed(method, path) {
            return Decision::Refuse(Refusal::RequestNotAllowed {
                method: method.to_owned(),
                path: path.to_owned(),
            });
        }
        let authorization = format!("Bearer {}", session.access_placeholder);
        let auth_values: Vec<&str> = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str())
            .collect();
        let account_values: Vec<&str> = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
            .map(|(_, value)| value.as_str())
            .collect();
        if auth_values.as_slice() != [authorization.as_str()]
            || account_values.as_slice() != [session.account_placeholder.as_str()]
        {
            return Decision::Refuse(Refusal::InvalidCodexHeaders);
        }

        let Some(host) = authority_of(&session.upstream) else {
            return Decision::Refuse(Refusal::DestinationNotAllowed {
                host: session.upstream.clone(),
            });
        };
        if !self.allows(&host) {
            return Decision::Refuse(Refusal::DestinationNotAllowed { host });
        }

        let mut outgoing: Vec<(String, String)> = headers
            .iter()
            .filter(|(name, _)| {
                !is_hop_by_hop(name)
                    && !name.eq_ignore_ascii_case("authorization")
                    && !name.eq_ignore_ascii_case("chatgpt-account-id")
            })
            .cloned()
            .collect();
        outgoing.push((
            "authorization".to_owned(),
            format!("Bearer {}", session.access_token),
        ));
        outgoing.push(("chatgpt-account-id".to_owned(), session.account_id.clone()));
        Decision::Forward {
            upstream: session.upstream,
            headers: outgoing,
            scrub: vec![
                (session.access_token, session.access_placeholder),
                (session.account_id, session.account_placeholder),
            ],
            redirects: RedirectMode::Disabled,
        }
    }
}

fn codex_request_allowed(method: &str, path: &str) -> bool {
    (method.eq_ignore_ascii_case("POST") && matches!(path, "/responses" | "/responses/compact"))
        || (method.eq_ignore_ascii_case("GET") && path == "/models")
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

fn replace_bytes_many(haystack: &[u8], substitutions: &[(String, String)]) -> Vec<u8> {
    let mut buffer = haystack.to_vec();
    scrub_buffer(&mut buffer, substitutions, true)
}

/// Scrub the safe prefix of an ORIGINAL byte buffer.
///
/// When `finish` is false, the function retains enough original bytes to match a value across the
/// next stream chunk. It never puts replacement bytes back into `buffer`, so a replacement length
/// cannot change the next input boundary.
fn scrub_buffer(
    buffer: &mut Vec<u8>,
    substitutions: &[(String, String)],
    finish: bool,
) -> Vec<u8> {
    let max_real_len = substitutions
        .iter()
        .map(|(real, _)| real.len())
        .max()
        .unwrap_or(0);
    if max_real_len == 0 {
        return std::mem::take(buffer);
    }

    let safe_start_limit = if finish {
        buffer.len()
    } else {
        buffer.len().saturating_sub(max_real_len.saturating_sub(1))
    };
    let mut output = Vec::with_capacity(buffer.len());
    let mut cursor = 0;
    while cursor < safe_start_limit {
        let matched = substitutions
            .iter()
            .filter(|(real, _)| !real.is_empty() && buffer[cursor..].starts_with(real.as_bytes()))
            .max_by_key(|(real, _)| real.len());
        if let Some((real, placeholder)) = matched {
            output.extend_from_slice(placeholder.as_bytes());
            cursor += real.len();
        } else {
            output.push(buffer[cursor]);
            cursor += 1;
        }
    }
    buffer.drain(..cursor);
    output
}

/// Length of the longest suffix of `buf` that is a proper prefix of `needle` — the only trailing bytes
/// that a not-yet-arrived chunk could complete into a `needle` occurrence. [`scrub_buffer`] has already
/// removed every COMPLETE occurrence, so a split match can hold at most `needle.len() - 1` bytes here;
/// everything before this suffix is provably not part of any `needle`, whole or split, whatever arrives
/// next. [`scrub_stream`] uses it to flush a stalled holdback without ever releasing a partial credential.
fn straddle_prefix_len(buf: &[u8], needle: &[u8]) -> usize {
    let max = buf.len().min(needle.len().saturating_sub(1));
    (1..=max).rev().find(|&j| buf[buf.len() - j..] == needle[..j]).unwrap_or(0)
}

/// The widest [`straddle_prefix_len`] across every substitution.
///
/// A byte must stay held if ANY real value could still be completed through it, so the hold is the
/// maximum over the set, never the first match.
fn straddle_prefix_len_many(buf: &[u8], substitutions: &[(String, String)]) -> usize {
    substitutions
        .iter()
        .map(|(real, _)| straddle_prefix_len(buf, real.as_bytes()))
        .max()
        .unwrap_or(0)
}

/// Drain from `carry` everything that provably cannot begin a split credential, and return the
/// scrubbed bytes ready to forward. What stays behind is ONLY the longest suffix that is still a
/// proper prefix of some real value — the sole bytes a later chunk could complete into a match.
///
/// Two passes, and the order matters. The first is the ordinary fixed-width scrub, which leaves
/// `max_real_len - 1` bytes held. The second asks what those held bytes actually ARE:
/// [`straddle_prefix_len_many`] finds the longest tail that could still start a real value, so
/// everything ahead of it is safe and is scrubbed to completion and released. The bytes still held
/// are a strict subset of what the EOF flush would hold, for ANY input — which is why this is safe to
/// run on every chunk, not only after an idle gap.
///
/// Running it per chunk is what keeps a slow keepalive stream flowing: the fixed-width scrub alone
/// withholds up to `max_real_len - 1` bytes, so a response that dribbles fewer than that between
/// flushes (a contained cursor agent leg sends ~9-byte keepalives) forwards NOTHING and the client
/// times the connection out. Here the hold shrinks to the real straddling prefix — normally zero.
fn scrub_release_safe(carry: &mut Vec<u8>, substitutions: &[(String, String)]) -> Vec<u8> {
    let mut emit = scrub_buffer(carry, substitutions, false);
    let held = straddle_prefix_len_many(carry, substitutions);
    if held < carry.len() {
        let mut releasable: Vec<u8> = carry.drain(..carry.len() - held).collect();
        emit.extend_from_slice(&scrub_buffer(&mut releasable, substitutions, true));
    }
    emit
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
/// The client every credential-bearing relay forwards through, and the only one [`start`] will use.
///
/// The proxy is header-agnostic by design: it forwards whatever the container sent, and the forwarded
/// agent credential rides `x-api-key`. reqwest's default redirect policy is `Policy::limited(10)`, and
/// its cross-host scrub covers only AUTHORIZATION, COOKIE, cookie2, PROXY_AUTHORIZATION and
/// WWW_AUTHENTICATE (`src/redirect.rs:239-251` in 0.12.28) — `x-api-key` is in none of them. So under
/// the default policy a `3xx` from an allowlisted host carries the real credential onward to a host the
/// allowlist never approved: the destination is decided BEFORE the redirect moves it.
///
/// That is measured, not inferred. `the_streamed_body_redirect_matrix_is_pinned_per_status` was first
/// written against a default client and recorded the real credential arriving at an unapproved host on
/// a cross-authority `301`. This construction is why that is not what ships, and it lives in this module
/// rather than at the call site because a safety property a caller can forget is not a safety property.
///
/// A redirect is followed only while it stays on the upstream the credential in flight was registered
/// for. That upstream is not the allowlist: [`ProxyEngine::authorize`] picks the destination from the
/// credential itself, and the allowlist is the UNION of every present credential's upstream. A union
/// check would approve a `3xx` from one registered vendor to ANOTHER — handing an Anthropic key to
/// OpenAI's host — because both are on it. A refused attempt is `stop()`, which returns the `3xx` for
/// the proxy to relay to the container unchanged.
///
/// `301`, `302` and `303` are the only statuses this policy ever judges. `307` and `308` preserve the
/// request body, and a relayed body is a stream that cannot be replayed, so `tower-http` surfaces those
/// two unchanged without consulting any policy — see
/// [`the_streamed_body_redirect_matrix_is_pinned_per_status`], which measures all five. The container
/// receives such a `3xx` verbatim and may act on it with its own placeholder-bearing client; what it
/// cannot do is have this proxy carry the real credential there.
///
/// The pairing is available without per-request state, which is why one client serves every request:
/// `Policy::custom` closes over nothing but the pure predicate, so no credential is reachable from
/// here — while the attempt carries its own chain, whose FIRST entry is the original request URL that
/// [`relay`] built from that credential's upstream.
///
/// EVERY HOP, not just the first, and each judged against the ORIGINAL rather than its predecessor:
/// judging hop-against-predecessor would let a chain walk one authority at a time to anywhere. Verified
/// in reqwest 0.12.28 rather than assumed — `TowerRedirectPolicy::redirect` (`src/redirect.rs:306`)
/// pushes the previous URL onto an accumulating chain (`:315`) and then calls this policy with THAT
/// hop's target (`:317`), so `previous()[0]` is the original request URL on every hop. An empty chain
/// yields no original and is refused rather than followed.
fn forwarding_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let original = attempt.previous().first().map(|url| url.as_str()).unwrap_or("");
            if allows_paired_redirect(original, attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
}

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
    /// `(authority, bound address)` for each leg listener, in the order handed to
    /// [`start_with_legs`]. Empty for a proxy started without legs.
    leg_addrs: Vec<(String, SocketAddr)>,
    engine: Arc<ProxyEngine>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
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

    /// The base URL a container uses to reach the LEG listener bound for `authority`, reaching this
    /// proxy at `host` — or `None` when no such leg was started. The same host/pinhole reasoning as
    /// [`Self::container_base_url_via`] applies: every leg listener sits on the same address, only
    /// the port differs, so the one pinholed address covers them all.
    pub fn leg_base_url_via(&self, host: &str, authority: &str) -> Option<String> {
        let want = host_key(authority);
        self.leg_addrs
            .iter()
            .find(|(a, _)| same_authority(&host_key(a), &want))
            .map(|(_, addr)| format!("http://{host}:{}", addr.port()))
    }

    /// The bound address of the leg listener for `authority` (for tests / diagnostics).
    pub fn leg_addr(&self, authority: &str) -> Option<SocketAddr> {
        let want = host_key(authority);
        self.leg_addrs
            .iter()
            .find(|(a, _)| same_authority(&host_key(a), &want))
            .map(|(_, addr)| *addr)
    }
}

impl Drop for RunningProxy {
    fn drop(&mut self) {
        // JOB END IS A REVOCATION, and it has to reach connections that are ALREADY OPEN — not only
        // the listener. Aborting each task drops that accept loop's `JoinSet`, and dropping a `JoinSet`
        // aborts every task in it, so every accepted connection dies here too.
        //
        // Closing the listener alone would not be revocation. It stops the NEXT client and says
        // nothing about the current one: an h2 or keep-alive connection accepted a moment earlier
        // still holds `Arc<ProxyEngine>`, so its placeholder still resolves and it can keep a
        // credential-bearing stream open, or start new requests, for as long as it likes. With a
        // relayed body and no byte cap, "as long as it likes" is unbounded — see
        // `dropping_the_proxy_revokes_connections_it_has_already_accepted`.
        //
        // The legs die with the primary: a leg outliving the job would be a credential-bearing
        // listener nobody owns.
        for task in &self.tasks {
            task.abort();
        }
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
///
/// The forwarding client is built HERE and cannot be supplied. See [`forwarding_client`] for why: the
/// redirect policy is the control that keeps the real credential on the upstream it was issued for, and
/// a caller able to pass a client is a caller able to omit it.
pub async fn start(
    engine: Arc<ProxyEngine>,
    ports: Option<crate::sandbox_net::PortRange>,
) -> std::io::Result<RunningProxy> {
    start_with_legs(engine, ports, &[]).await
}

/// [`start`], plus one LEG listener per entry of `leg_authorities`.
///
/// A leg listener is the routing selector for a multi-upstream credential: a request arriving on it
/// is authorized via [`ProxyEngine::authorize_via`] with that leg's authority, so it forwards to the
/// identified credential's own entry for that authority — see [`JobCredential::upstreams`]. The
/// primary listener keeps the exact semantics [`start`] always had.
///
/// Every listener binds from the SAME `ports` range, so a namespace-contained job's firewall
/// pinhole — rendered from the range, never from a bound port — covers the legs with no new rules.
/// One job therefore consumes `1 + leg_authorities.len()` ports from the range for its lifetime.
/// All listeners share one connection ceiling and one in-flight ceiling: the bounds stay job-global,
/// so an extra leg never widens what one job may hold open.
pub async fn start_with_legs(
    engine: Arc<ProxyEngine>,
    ports: Option<crate::sandbox_net::PortRange>,
    leg_authorities: &[String],
) -> std::io::Result<RunningProxy> {
    // Two clients, because the redirect rule is a property of the CREDENTIAL, not of the listener.
    // `paired_redirects` follows a 3xx only while it stays on the upstream the credential in flight was
    // registered for; `no_redirects` follows none at all, and is what a typed Codex session forwards
    // through, its credential being valid for a fixed set of paths. Neither is supplied by a caller —
    // see [`forwarding_client`].
    let clients = ForwardClients {
        paired_redirects: forwarding_client().map_err(std::io::Error::other)?,
        no_redirects: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(std::io::Error::other)?,
    };
    // KNOWN EXPOSURE, deliberately unchanged for now. `0.0.0.0` is every interface, so on a seller with
    // a public IP and no firewall each per-job listener is internet-reachable on a random high port.
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
    let connections = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    // Shared across every connection, every listener, and both protocols, so neither multiplexing nor
    // an extra leg can widen it.
    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS));

    let listener = bind_listener(ports).await?;
    let addr = listener.local_addr()?;
    let mut tasks = Vec::with_capacity(1 + leg_authorities.len());
    tasks.push(spawn_accept_loop(
        listener,
        Arc::clone(&engine),
        clients.clone(),
        Arc::clone(&connections),
        Arc::clone(&in_flight),
        None,
    ));

    let mut leg_addrs = Vec::with_capacity(leg_authorities.len());
    for authority in leg_authorities {
        let listener = bind_listener(ports).await?;
        leg_addrs.push((authority.clone(), listener.local_addr()?));
        tasks.push(spawn_accept_loop(
            listener,
            Arc::clone(&engine),
            clients.clone(),
            Arc::clone(&connections),
            Arc::clone(&in_flight),
            Some(authority.clone()),
        ));
    }

    eprintln!("proxy: listener kind=primary port={}", addr.port());
    for (authority, leg_addr) in &leg_addrs {
        eprintln!("proxy: listener kind=leg authority={authority} port={}", leg_addr.port());
    }

    Ok(RunningProxy { addr, leg_addrs, engine, tasks })
}

/// One listener's accept loop. `via_authority` is `None` for the primary listener and the leg's
/// authority for a leg listener — it decides which authorize entry point every request on this
/// listener goes through, and nothing else about the connection handling differs.
fn spawn_accept_loop(
    listener: tokio::net::TcpListener,
    engine: Arc<ProxyEngine>,
    clients: ForwardClients,
    connections: Arc<Semaphore>,
    in_flight: Arc<Semaphore>,
    via_authority: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // EVERY CONNECTION IS OWNED BY THIS TASK, and that ownership is what makes job end a
        // revocation rather than a request to stop.
        //
        // A detached `tokio::spawn` per connection would not be: tokio tasks are not scoped to their
        // spawner, so aborting this accept loop would close the listener while every ALREADY-ACCEPTED
        // connection kept running — each holding `Arc<ProxyEngine>` and so the live credential map.
        // An h2 or keep-alive client could then hold a credential-bearing stream open past job end and
        // start new requests on it, and the listener being shut would say nothing about any of it.
        // Dropping a `JoinSet` aborts every task in it, so aborting this task now reaches them all.
        let mut live: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        loop {
            let (stream, _peer) = tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(_) => continue,
                },
                // Reap finished connections so a long-lived proxy does not accumulate their handles.
                // Guarded, because `join_next` on an empty set is instantly `None` and would spin.
                _ = live.join_next(), if !live.is_empty() => continue,
            };
            // Bound concurrent connections: acquire a permit before spawning, hold it for the life of
            // the connection. When all permits are out, this awaits — new connections queue at the OS
            // accept backlog instead of each spawning an unbounded task and exhausting the daemon.
            let Ok(permit) = Arc::clone(&connections).acquire_owned().await else {
                continue; // semaphore closed — proxy shutting down
            };
            let engine = Arc::clone(&engine);
            let clients = clients.clone();
            let in_flight = Arc::clone(&in_flight);
            let via_authority = via_authority.clone();
            live.spawn(async move {
                let _permit = permit; // released when the connection ends
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let engine = Arc::clone(&engine);
                    let clients = clients.clone();
                    let in_flight = Arc::clone(&in_flight);
                    let via_authority = via_authority.clone();
                    async move {
                        // Held for the life of THIS request, so the ceiling counts requests rather
                        // than sockets. Excess requests wait here instead of each buffering a body.
                        let _in_flight = in_flight.acquire().await;
                        handle_request(req, engine, clients, via_authority).await
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
    })
}

type ProxyBody = BoxBody<Bytes, std::io::Error>;

/// The two forwarding clients, selected per request by the authorized [`RedirectMode`].
#[derive(Clone)]
struct ForwardClients {
    paired_redirects: reqwest::Client,
    no_redirects: reqwest::Client,
}

// --- Per-request diagnostic logging (leg-level observability) --------------------------------------
//
// Emitted to stderr as `proxy: key=value` lines, matching the daemon's existing `sandbox:` convention:
// no logging dependency, no subscriber, so a line always reaches the seat's captured stderr. A line
// NEVER carries a credential value, a placeholder, or any header/body byte — only authorities, hosts,
// ports, protocols, phases, wall-clock timings and outcomes.
//
// The question this exists to answer is WHO ended a stalled await — the container that gave up, or the
// upstream that dropped the request. A log recording only "no response" cannot tell those apart: they
// are byte-identical downstream. `terminated_by=` is that field.
static PROXY_REQ_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Milliseconds since the Unix epoch. The FORWARD line's stamp is the anchor every wait is measured
/// from: the container's outbound send time is not otherwise observable, so a duration differenced
/// against the nearest visible ACP event (a title notification, say) is anchored to the wrong instant.
fn proxy_log_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

/// Records `terminated_by=downstream` if dropped before the upstream terminated the await.
///
/// A forwarded request holds one of these across [`relay`]. [`AwaitGuard::settle`] is called the instant
/// the upstream returns a response header OR an error, disarming it. If instead the container closes its
/// connection while the proxy is still awaiting the upstream, hyper drops the request future, this value
/// is dropped un-settled, and the drop records that the CONTAINER (downstream) ended the wait. That is
/// the single field separating a client-side timeout from an upstream-side one.
struct AwaitGuard {
    req: u64,
    host: String,
    settled: bool,
}

impl AwaitGuard {
    fn settle(&mut self) {
        self.settled = true;
    }
}

impl Drop for AwaitGuard {
    fn drop(&mut self) {
        if !self.settled {
            eprintln!(
                "proxy: req#{} ts={} phase=await_response terminated_by=downstream host={}",
                self.req,
                proxy_log_now_ms(),
                self.host,
            );
        }
    }
}

/// Formats an optional timestamp as the number or `none`, so a `body_end` line for a stream that never
/// yielded a byte still carries both fields — a missing field would read as a logging bug, not as "zero".
struct OptTs(Option<u128>);

impl std::fmt::Display for OptTs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(ts) => write!(f, "{ts}"),
            None => f.write_str("none"),
        }
    }
}

/// Formats an optional byte length as the number or `none`, so a `forward_end` line for a leg with no
/// credential to scrub still carries the `real_len` field. A missing field would read as a logging bug,
/// and a `0` would read as "an empty credential" rather than "no credential on this leg". This is a
/// LENGTH only — never the credential value; a length is not a secret, a prefix is.
struct OptLen(Option<usize>);

impl std::fmt::Display for OptLen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(len) => write!(f, "{len}"),
            None => f.write_str("none"),
        }
    }
}

/// Observes the RESPONSE BODY — the region after the response header that [`AwaitGuard`] and the
/// `await_response` line structurally cannot see. Wraps the raw upstream byte stream and is PASSIVE: it
/// forwards every chunk unchanged and records only timings and the terminator.
///
/// It answers the body-phase form of the WHO-ended question:
/// - `terminated_by=upstream kind=eof completed=true` — the upstream body ended naturally.
/// - `terminated_by=upstream kind=error` — the upstream body errored (reset / GOAWAY / decode).
/// - `terminated_by=downstream` — dropped before either terminal, i.e. hyper dropped the body because
///   the container closed its connection mid-stream. This is the body-phase analogue of [`AwaitGuard`],
///   and the ONLY way to observe a client that gives up AFTER the header.
///
/// `last_byte_ts` is the discriminator for a mid-stream stall: the gap from it to the `body_end` line is
/// the stall interval, both ends real events in this same log. And while the body produces no bytes at
/// all, a [`BODY_HEARTBEAT_PERIOD`] clock emits `body_idle` lines — so "a header then N seconds of
/// nothing" (the failure under investigation) is a POSITIVE reading with a denominator, not the absence
/// of a `body_end` line, which is indistinguishable from a request that never reached the body phase.
/// The clock only EMITS; it never cancels, so it cannot manufacture the terminator this meter finds.
struct BodyMeter {
    // `+ Sync` as well as `+ Send`: the boxed response body (`BoxBody` via `BodyExt::boxed`) requires
    // `Send + Sync`, and the concrete upstream stream satisfies both — erasing to `dyn … + Send` alone
    // would drop `Sync` and fail the bound.
    inner: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send + Sync>>,
    req: u64,
    host: String,
    // The observed response-header instant: the anchor `body_first_byte` and every `body_idle` gap are
    // measured from it, both ends real events in this log rather than a value differenced against the
    // nearest ACP notification.
    header_ts: u128,
    bytes: u64,
    first_byte_ts: Option<u128>,
    last_byte_ts: Option<u128>,
    // Log-only clock: fires while the upstream body yields nothing, so an idle stall is an explicit line
    // rather than silence. Polled but NEVER used to cancel — see [`BODY_HEARTBEAT_PERIOD`].
    heartbeat: tokio::time::Interval,
    // Set once the UPSTREAM terminates (EOF or error) so Drop does not then record a spurious
    // `terminated_by=downstream` — the upstream terminal and the eventual drop are the same stream's end.
    ended: bool,
}

impl futures_util::Stream for BodyMeter {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // `Self: Unpin` (every field is), so unwrapping the pin needs no projection crate.
        let this = self.get_mut();
        match futures_util::Stream::poll_next(this.inner.as_mut(), cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                let now = proxy_log_now_ms();
                if this.first_byte_ts.is_none() {
                    this.first_byte_ts = Some(now);
                    eprintln!(
                        "proxy: req#{} ts={now} phase=body_first_byte host={} gap_from_header_ms={}",
                        this.req,
                        this.host,
                        now.saturating_sub(this.header_ts),
                    );
                }
                this.bytes += chunk.len() as u64;
                this.last_byte_ts = Some(now);
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                this.ended = true;
                eprintln!(
                    "proxy: req#{} ts={} phase=body_end terminated_by=upstream kind=error host={} bytes={} completed=false first_byte_ts={} last_byte_ts={} msg={error}",
                    this.req,
                    proxy_log_now_ms(),
                    this.host,
                    this.bytes,
                    OptTs(this.first_byte_ts),
                    OptTs(this.last_byte_ts),
                );
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                this.ended = true;
                let now = proxy_log_now_ms();
                eprintln!(
                    "proxy: req#{} ts={now} phase=body_end terminated_by=upstream kind=eof host={} bytes={} completed=true first_byte_ts={} last_byte_ts={}",
                    this.req,
                    this.host,
                    this.bytes,
                    OptTs(this.first_byte_ts),
                    OptTs(this.last_byte_ts),
                );
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => {
                // Upstream has nothing right now. Drive the heartbeat so a silent body is an explicit
                // reading rather than an absence. Polling the interval registers its waker on `cx`, so
                // the task is re-polled each period even while the upstream stays parked; the upstream's
                // own waker is already registered from the `poll_next` above, so a real byte still wakes
                // us. Draining every ready tick keeps it to one line per elapsed period.
                while this.heartbeat.poll_tick(cx).is_ready() {
                    let now = proxy_log_now_ms();
                    eprintln!(
                        "proxy: req#{} ts={now} phase=body_idle host={} bytes={} gap_from_header_ms={}",
                        this.req,
                        this.host,
                        this.bytes,
                        now.saturating_sub(this.header_ts),
                    );
                }
                std::task::Poll::Pending
            }
        }
    }
}

impl Drop for BodyMeter {
    fn drop(&mut self) {
        // Un-ended at drop ⇒ the body pipeline was torn down before the upstream itself finished, which
        // happens when hyper drops the response because the container closed its connection mid-stream.
        if !self.ended {
            eprintln!(
                "proxy: req#{} ts={} phase=body_end terminated_by=downstream host={} bytes={} completed=false first_byte_ts={} last_byte_ts={}",
                self.req,
                proxy_log_now_ms(),
                self.host,
                self.bytes,
                OptTs(self.first_byte_ts),
                OptTs(self.last_byte_ts),
            );
        }
    }
}

/// Observes the DOWNSTREAM-FORWARDED body — the frames that actually leave the proxy for the container,
/// AFTER [`scrub_stream`] (or the no-scrub `map`) has run. [`BodyMeter`] sits BEFORE scrub and counts
/// bytes RECEIVED from the upstream; this counts bytes FORWARDED to hyper. Reading `body_*` beside
/// `forward_*` for one request separates "the upstream sent little" from "the proxy forwarded little of
/// what the upstream sent".
///
/// The gap between the two meters is the credential-scrub holdback: [`scrub_stream`] withholds the last
/// `real.len() - 1` bytes of the stream — the longest possible partial credential match — and flushes it
/// only when the upstream ENDS. On an upstream STALL (bytes then no EOF), bytes that BodyMeter has
/// already counted may not yet be forwarded here. This meter only MEASURES that gap; it changes nothing
/// about when the carry flushes, because that is a containment boundary, not a streaming knob.
///
/// PASSIVE, exactly like [`BodyMeter`]: forwards every frame unchanged, records only counts, timings and
/// the terminator, and its heartbeat only EMITS — it never cancels, so it cannot manufacture the
/// terminator it observes. LOGS NO CREDENTIAL MATERIAL: `real_len` is the LENGTH of the credential
/// string only, never its value, never the carry buffer, never a matched region or offset.
struct ForwardMeter {
    inner: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> + Send + Sync>,
    >,
    req: u64,
    host: String,
    // The same observed response-header instant [`BodyMeter`] measures from, so `forward_*` gaps and
    // `body_*` gaps share one origin and read directly against each other.
    header_ts: u128,
    // Bytes ACTUALLY forwarded downstream: the running sum of forwarded `Frame::data` payload lengths.
    bytes: u64,
    first_ts: Option<u128>,
    last_ts: Option<u128>,
    // LENGTH of the credential scrubbed on this leg (`None` on a leg with no scrub). The holdback is
    // `real_len - 1` bytes, so this is the parameter that predicts how much a stalled stream withholds.
    real_len: Option<usize>,
    // Log-only clock, same discipline as [`BodyMeter`]'s heartbeat: emits `forward_idle` while nothing is
    // forwarded, so "the upstream trickled bytes but the proxy forwarded none" is a POSITIVE reading with
    // a denominator rather than an absence. Polled but NEVER used to cancel.
    heartbeat: tokio::time::Interval,
    // Set once the forwarded stream terminates (its own EOF or error) so Drop does not then record a
    // spurious `terminated_by=downstream`.
    ended: bool,
}

impl futures_util::Stream for ForwardMeter {
    type Item = Result<Frame<Bytes>, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // `Self: Unpin` (every field is), so unwrapping the pin needs no projection crate.
        let this = self.get_mut();
        match futures_util::Stream::poll_next(this.inner.as_mut(), cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                let now = proxy_log_now_ms();
                if this.first_ts.is_none() {
                    this.first_ts = Some(now);
                    eprintln!(
                        "proxy: req#{} ts={now} phase=forward_first host={} gap_from_header_ms={}",
                        this.req,
                        this.host,
                        now.saturating_sub(this.header_ts),
                    );
                }
                // Only DATA frames carry body bytes; a trailers frame contributes zero.
                this.bytes += frame.data_ref().map_or(0, Bytes::len) as u64;
                this.last_ts = Some(now);
                std::task::Poll::Ready(Some(Ok(frame)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                this.ended = true;
                eprintln!(
                    "proxy: req#{} ts={} phase=forward_end terminated_by=upstream kind=error host={} bytes={} real_len={} first_ts={} last_ts={} msg={error}",
                    this.req,
                    proxy_log_now_ms(),
                    this.host,
                    this.bytes,
                    OptLen(this.real_len),
                    OptTs(this.first_ts),
                    OptTs(this.last_ts),
                );
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                this.ended = true;
                let now = proxy_log_now_ms();
                eprintln!(
                    "proxy: req#{} ts={now} phase=forward_end terminated_by=upstream kind=eof host={} bytes={} real_len={} first_ts={} last_ts={}",
                    this.req,
                    this.host,
                    this.bytes,
                    OptLen(this.real_len),
                    OptTs(this.first_ts),
                    OptTs(this.last_ts),
                );
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => {
                // The forwarded stream has nothing right now — either the upstream is parked or scrub is
                // holding its carry back. Drive the heartbeat so the withheld case is an explicit line
                // rather than an absence. Same waker mechanics as [`BodyMeter`]: polling the interval
                // registers its waker, and the inner stream's waker is already registered from the poll
                // above, so a real frame still wakes us immediately.
                while this.heartbeat.poll_tick(cx).is_ready() {
                    let now = proxy_log_now_ms();
                    eprintln!(
                        "proxy: req#{} ts={now} phase=forward_idle host={} bytes={} gap_from_header_ms={}",
                        this.req,
                        this.host,
                        this.bytes,
                        now.saturating_sub(this.header_ts),
                    );
                }
                std::task::Poll::Pending
            }
        }
    }
}

impl Drop for ForwardMeter {
    fn drop(&mut self) {
        // Un-ended at drop ⇒ the forwarded stream was torn down before it finished, i.e. hyper dropped
        // the body because the container closed its connection mid-stream. `bytes` is then the total the
        // proxy managed to forward before the client gave up — the number the holdback hypothesis predicts.
        if !self.ended {
            eprintln!(
                "proxy: req#{} ts={} phase=forward_end terminated_by=downstream host={} bytes={} real_len={} first_ts={} last_ts={}",
                self.req,
                proxy_log_now_ms(),
                self.host,
                self.bytes,
                OptLen(self.real_len),
                OptTs(self.first_ts),
                OptTs(self.last_ts),
            );
        }
    }
}

/// Serve one request: run its HEADERS through [`ProxyEngine::authorize`] and — only on a substituted
/// forward — stream its body to the real upstream and stream the response back. A refusal returns a
/// `4xx`/`5xx` to the container with the real credential never in play, and without reading the body.
///
/// Nothing here accumulates a request. The body is a stream from the container's socket to the
/// upstream's, which is what lets a client that holds its request open — an agent streaming a turn —
/// be served at all: waiting for such a body to END is waiting forever.
async fn handle_request(
    req: Request<Incoming>,
    engine: Arc<ProxyEngine>,
    clients: ForwardClients,
    via_authority: Option<String>,
) -> Result<Response<ProxyBody>, std::convert::Infallible> {
    let method = req.method().clone();
    let proxy_req = PROXY_REQ_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // `req.version()` is the NEGOTIATED protocol (HTTP/2.0 for the h2c legs, HTTP/1.1 otherwise) — it
    // decides whether an http1-only timeout like `header_read_timeout` is even in scope for this leg.
    let listener_label = via_authority.as_deref().unwrap_or("primary").to_owned();
    eprintln!(
        "proxy: req#{proxy_req} ts={} phase=arrived listener={listener_label} proto={:?} method={method}",
        proxy_log_now_ms(),
        req.version(),
    );
    // The path+query is forwarded verbatim; it is never a substitution input.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let request_path = req.uri().path().to_owned();
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect();
    // AUTHORIZE BEFORE READING A SINGLE BODY BYTE. `authorize_request` takes the request line and the
    // headers only, and the body is deliberately not a substitution surface (see "Why the BODY is never
    // a substitution surface" in the module header), so the body was never an input to this decision.
    // Deciding first means a request we refuse costs us nothing at all: we answer from the headers and
    // drop the body unread.
    //
    // Which entry point decides is a property of the LISTENER the request arrived on: the primary
    // listener forwards to the credential's primary upstream, a leg listener to the credential's own
    // entry for that leg's authority. Same steps, same refusals — only the selector differs. A typed
    // Codex session takes its own narrow route ahead of both, on the primary listener only.
    let decision = engine.authorize_request(
        method.as_str(),
        &request_path,
        &headers,
        via_authority.as_deref(),
    );
    match decision {
        Decision::Refuse(reason) => {
            eprintln!(
                "proxy: req#{proxy_req} ts={} phase=authorize outcome=refuse reason={reason}",
                proxy_log_now_ms(),
            );
            let status = match reason {
                Refusal::DestinationNotAllowed { .. } => StatusCode::FORBIDDEN,
                Refusal::NoKnownPlaceholder => StatusCode::BAD_GATEWAY,
                Refusal::InvalidCodexHeaders | Refusal::RequestNotAllowed { .. } => {
                    StatusCode::FORBIDDEN
                }
                // Registration-time refusals; reachable here only through `authorize`'s defensive
                // empty-list arm, never through a credential `register` accepted.
                Refusal::NoUpstream | Refusal::DuplicateUpstream { .. } => StatusCode::FORBIDDEN,
            };
            Ok(refusal_response(status, &reason.to_string()))
        }
        Decision::Forward {
            upstream,
            headers,
            scrub,
            redirects,
        } => {
            let host = authority_of(&upstream).unwrap_or_else(|| upstream.clone());
            eprintln!(
                "proxy: req#{proxy_req} ts={} phase=forward host={host}",
                proxy_log_now_ms(),
            );
            // The redirect rule belongs to the CREDENTIAL, not to the listener: a paired-authority
            // credential may follow a 3xx that stays on its own upstream, and a typed Codex session may
            // not follow one at all, because its credential is valid for fixed paths only.
            let client = match redirects {
                RedirectMode::PairedAuthority => &clients.paired_redirects,
                RedirectMode::Disabled => &clients.no_redirects,
            };
            // The body is RELAYED, never accumulated: each chunk goes upstream as it arrives, so the
            // daemon holds one chunk rather than a whole request, and a client that never closes its
            // body is no longer a client we wait for forever. It is forwarded VERBATIM — the decision
            // did not carry it, so nothing substituted a credential into it.
            //
            // Both bounds the buffered read used to provide are KEPT, now enforced on the stream
            // instead of on a collected buffer: the size cap below, and `idle_bounded`, which fails the
            // request after BODY_IDLE_TIMEOUT of silence. Streaming answers the OOM half of the old cap
            // by itself — the daemon holds one chunk — but a ceiling on what one job may push through an
            // approved credential is a separate control, so it stays.
            //
            // The cap is checked TWICE, because a stream can only answer one of the two questions early.
            // A declared `content-length` is known before a byte moves, so an over-cap body is refused
            // with the same `413` the buffered read gave it, and nothing is sent upstream. A body with no
            // declared length (chunked, or HTTP/2) cannot be judged until it arrives, so `size_bounded`
            // is the backstop: it fails the request the moment the cap is passed, mid-flight, which
            // surfaces as a `502` because the upstream request is already open by then.
            let declared = req
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|text| text.parse::<u64>().ok());
            if declared.is_some_and(|length| length > MAX_REQUEST_BODY_BYTES as u64) {
                eprintln!(
                    "proxy: req#{proxy_req} ts={} phase=forward outcome=refuse reason=body_over_cap host={host}",
                    proxy_log_now_ms(),
                );
                return Ok(refusal_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body exceeds the proxy limit",
                ));
            }
            let body = reqwest::Body::wrap_stream(size_bounded(
                idle_bounded(req.into_body().into_data_stream(), BODY_IDLE_TIMEOUT),
                MAX_REQUEST_BODY_BYTES,
            ));
            // Held across the await. `settle` disarms it the instant the upstream terminates the await
            // (a response header or an error); if the CONTAINER closes first, this drops un-settled and
            // records `terminated_by=downstream` — the field that tells a client timeout from an
            // upstream drop, which are otherwise byte-identical as "no response".
            let mut await_guard = AwaitGuard { req: proxy_req, host: host.clone(), settled: false };
            let outcome = relay(
                client,
                &method,
                &upstream,
                &path_and_query,
                headers,
                body,
                scrub,
                proxy_req,
                &host,
            )
            .await;
            await_guard.settle();
            match outcome {
                Ok(response) => Ok(response),
                // No-fallback: an upstream failure fails the request; it never resends without the
                // proxy or with the real credential in the container.
                Err(message) => {
                    eprintln!(
                        "proxy: req#{proxy_req} ts={} phase=await_response terminated_by=upstream kind=error host={host} msg={message}",
                        proxy_log_now_ms(),
                    );
                    Ok(refusal_response(StatusCode::BAD_GATEWAY, &message))
                }
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
    body: reqwest::Body,
    scrub: Vec<(String, String)>,
    req: u64,
    host: &str,
) -> Result<Response<ProxyBody>, String> {
    let url = format!("{}{}", upstream.trim_end_matches('/'), path_and_query);
    let method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|error| format!("bad method: {error}"))?;
    let mut request = client.request(method, &url).body(body);
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        request = request.header(name, value);
    }
    request = request.header(reqwest::header::ACCEPT_ENCODING, "identity");
    // The anchor for this request's wait: the outbound send, otherwise unobservable. Every duration
    // derived downstream is measured from THIS `ts`, never from the nearest visible event.
    eprintln!(
        "proxy: req#{req} ts={} phase=await_send host={host}",
        proxy_log_now_ms(),
    );
    let upstream_response = request
        .send()
        .await
        .map_err(|error| format!("upstream request failed: {error}"))?;

    let supported_encoding = upstream_response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter()
        .all(|value| {
            value.to_str().is_ok_and(|text| {
                let mut encodings = text.split(',').map(str::trim);
                let first = encodings.next();
                first.is_some_and(|encoding| encoding.eq_ignore_ascii_case("identity"))
                    && encodings.all(|encoding| encoding.eq_ignore_ascii_case("identity"))
            })
        });
    if !supported_encoding {
        return Err("upstream response uses an unsupported Content-Encoding".to_owned());
    }

    let status = upstream_response.status();
    // The upstream terminated the await by answering. Pairs with `await_send` above: the interval
    // between these two `ts` values IS the send->response-header wait for this request. Held as the
    // anchor `BodyMeter` measures time-to-first-byte from — the same event, not a nearby proxy for it.
    let header_ts = proxy_log_now_ms();
    eprintln!(
        "proxy: req#{req} ts={header_ts} phase=await_response terminated_by=upstream kind=response_header host={host} status={}",
        status.as_u16(),
    );
    let mut builder = Response::builder().status(status.as_u16());
    for (name, value) in upstream_response.headers() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        // Scrub raw bytes. A valid HTTP header can contain non-UTF-8 `obs-text`, so a text-only branch
        // would pass the real credential unchanged when any neighboring byte was non-UTF-8.
        let scrubbed = replace_bytes_many(value.as_bytes(), &scrub);
        let scrubbed = hyper::header::HeaderValue::from_bytes(&scrubbed)
            .map_err(|_| "upstream response has an unsafe header value".to_owned())?;
        builder = builder.header(name, scrubbed);
    }
    // Instrument the response body — the region after the header that `await_response` cannot see.
    // Passive: it forwards every chunk unchanged and records only timings and the terminator; on Drop
    // (hyper dropping the body because the container closed mid-stream) it records
    // `terminated_by=downstream`, the body-phase analogue of `AwaitGuard`.
    // First tick one full period out, not immediately; `Skip` so a delayed poll collapses missed ticks
    // to a single line rather than a catch-up burst.
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + BODY_HEARTBEAT_PERIOD,
        BODY_HEARTBEAT_PERIOD,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let metered = BodyMeter {
        inner: Box::pin(upstream_response.bytes_stream()),
        req,
        host: host.to_owned(),
        header_ts,
        bytes: 0,
        first_byte_ts: None,
        last_byte_ts: None,
        heartbeat,
        ended: false,
    };
    // Second meter, DOWNSTREAM of scrub: `metered` (BodyMeter) counted what the upstream SENT; this
    // counts what the proxy FORWARDS after scrub. The credential length is captured before `scrub` is
    // consumed by the match. Same heartbeat discipline as the upstream meter so "received bytes, forwarded
    // none" is a positive `forward_idle` reading rather than silence.
    // The WIDEST real value in play, not the only one: it is what sets `scrub_stream`'s holdback, so
    // it is the number that explains a short forward. Captured before `scrub` is consumed below.
    let real_len = scrub.iter().map(|(real, _)| real.len()).max();
    let forwarded: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> + Send + Sync>,
    > = if scrub.is_empty() {
        Box::pin(metered.map(|chunk| chunk.map(Frame::data).map_err(std::io::Error::other)))
    } else {
        Box::pin(scrub_stream(metered, scrub, RESPONSE_HOLDBACK_FLUSH_IDLE))
    };
    let mut forward_heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + BODY_HEARTBEAT_PERIOD,
        BODY_HEARTBEAT_PERIOD,
    );
    forward_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let forward_metered = ForwardMeter {
        inner: forwarded,
        req,
        host: host.to_owned(),
        header_ts,
        bytes: 0,
        first_ts: None,
        last_ts: None,
        real_len,
        heartbeat: forward_heartbeat,
        ended: false,
    };
    let body = BodyExt::boxed(StreamBody::new(forward_metered));
    builder
        .body(body)
        .map_err(|error| format!("building proxied response failed: {error}"))
}

/// Relay a request body chunk-by-chunk, dropping a client that goes SILENT without cutting off one
/// that is merely slow.
///
/// The deadline is measured on the GAP between chunks, so it does not care how long a body takes or
/// how large it is — only that it is still arriving. A client that keeps sending is never interrupted;
/// one that stops errors the stream `idle` later, and that error aborts the upstream request, so a
/// credential-bearing connection never outlives the client that justified it. Cancellation the other
/// way is already covered: if the upstream dies, `relay`'s response stream errors and hyper closes the
/// container's connection.
///
/// Trailers are dropped. This is a byte relay onto a fresh upstream request, whose framing is hyper's
/// to generate; forwarding the container's trailers onto it would be forwarding metadata about a
/// different HTTP exchange.
/// Takes a chunk STREAM rather than the [`Incoming`] body it is used on, because `Incoming` cannot be
/// constructed outside hyper — a synthetic one is the only way to test this at all, and an untested
/// deadline is the one thing standing between a silent client and a pinned daemon connection.
fn idle_bounded<S, E>(
    chunks: S,
    idle: Duration,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: futures_util::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    // `None` inner ⇒ the stream is finished (ended, errored, or timed out); stop yielding.
    futures_util::stream::unfold(
        Some(Box::pin(chunks)),
        move |inner| async move {
            let mut inner = inner?;
            match tokio::time::timeout(idle, inner.next()).await {
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "request body went silent",
                    )),
                    None,
                )),
                Ok(None) => None,
                Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(inner))),
                Ok(Some(Err(error))) => Some((Err(std::io::Error::other(error)), None)),
            }
        },
    )
}

/// Fail a request body that runs past `max` bytes, counting as it streams.
///
/// This is the streaming form of the size cap the buffered read used to apply with `Limited`. It is
/// the same limit and the same refusal point — the request dies the moment the cap is passed — but it
/// no longer needs the body in memory to enforce it: the count is a running total, and the bytes
/// themselves are already gone upstream. A body that stays under the cap is never touched.
///
/// The cap is on the RELAYED request only. It says nothing about the upstream's response, which is a
/// stream this proxy never bounds by size.
fn size_bounded<S>(
    chunks: S,
    max: usize,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    // `None` inner ⇒ the stream is finished (ended, errored, or over the cap); stop yielding.
    futures_util::stream::unfold(
        (Some(Box::pin(chunks)), 0usize),
        move |(inner, seen)| async move {
            let mut inner = inner?;
            match inner.next().await {
                None => None,
                Some(Err(error)) => Some((Err(error), (None, seen))),
                Some(Ok(chunk)) => {
                    let seen = seen.saturating_add(chunk.len());
                    if seen > max {
                        return Some((
                            Err(std::io::Error::other("request body exceeds the proxy limit")),
                            (None, seen),
                        ));
                    }
                    Some((Ok(chunk), (Some(inner), seen)))
                }
            }
        },
    )
}

/// Rewrite every real value back to its placeholder across a streamed response body, without
/// buffering the stream.
///
/// A credential can straddle a chunk boundary, so a naive per-chunk replace would miss it. On every
/// chunk [`scrub_release_safe`] forwards everything that provably cannot begin a split credential and
/// holds back ONLY the longest tail that is still a proper prefix of some real value — normally
/// nothing. The held-back tail is flushed in full when the upstream stream ends, because no later
/// chunk can then straddle into it.
///
/// Holding only the true straddling prefix — rather than a flat `max_real_len - 1` bytes every chunk —
/// is what lets a slow stream flow. A response that dribbles fewer than `max_real_len - 1` bytes
/// between reads (a contained cursor agent leg sends ~9-byte keepalives, real value 424 B) would
/// otherwise forward NOTHING and the client would time the connection out mid-body; here each
/// keepalive passes straight through.
///
/// A stalled upstream — bytes received, then silence with NO EOF — is handled by the same release on
/// an `idle` timer, a backstop for the case where a partial arrived and then nothing did. That
/// release never forwards a byte the EOF flush would withhold, for any value of `idle`.
///
/// SSE responses stay streaming: each chunk is forwarded as it arrives, minus that small carry-over.
fn scrub_stream<S>(
    inner: S,
    substitutions: Vec<(String, String)>,
    idle: Duration,
) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    // `None` inner ⇒ the stream is finished (ended or errored); stop yielding.
    // One turn of the loop: the upstream yielded (or ended), or the idle timer fired first.
    enum Step {
        Upstream(Option<reqwest::Result<Bytes>>),
        Idle,
    }
    futures_util::stream::unfold(
        (Some(inner), Vec::<u8>::new()),
        move |(inner, carry)| {
            let substitutions = substitutions.clone();
            async move {
                let mut inner = inner?;
                let mut carry = carry;
                loop {
                    // Race the upstream against an inactivity timer. `biased` prefers real progress, so
                    // only a genuine gap of `idle` with nothing ready reaches `Idle`; a fresh sleep each
                    // iteration measures inactivity since the last chunk, not elapsed total.
                    let step = tokio::select! {
                        biased;
                        item = inner.next() => Step::Upstream(item),
                        _ = tokio::time::sleep(idle) => Step::Idle,
                    };
                    match step {
                        Step::Upstream(Some(Ok(chunk))) => {
                            carry.extend_from_slice(&chunk);
                            // Release everything provably safe on THIS chunk, holding only a genuine
                            // straddling prefix (normally nothing) — not the flat `max_real_len - 1`
                            // tail, which starves a slow keepalive stream (see `scrub_release_safe`).
                            let emit = scrub_release_safe(&mut carry, &substitutions);
                            if emit.is_empty() {
                                // Nothing provably safe yet — keep pulling rather than yielding empty.
                                continue;
                            }
                            return Some((
                                Ok(Frame::data(Bytes::from(emit))),
                                (Some(inner), carry),
                            ));
                        }
                        Step::Upstream(Some(Err(error))) => {
                            return Some((Err(std::io::Error::other(error)), (None, Vec::new())));
                        }
                        Step::Upstream(None) => {
                            let tail = scrub_buffer(&mut carry, &substitutions, true);
                            if tail.is_empty() {
                                return None;
                            }
                            return Some((Ok(Frame::data(Bytes::from(tail))), (None, Vec::new())));
                        }
                        Step::Idle => {
                            // Backstop for a stall with NO EOF: flush whatever became provably safe
                            // since the last chunk, instead of waiting for an EOF that never comes.
                            // Same release the chunk arm runs, so it can only ever hold a genuine
                            // straddling prefix; a body received in full has already been forwarded.
                            let emit = scrub_release_safe(&mut carry, &substitutions);
                            if emit.is_empty() {
                                // Carry empty or entirely a credential prefix — nothing provably safe to
                                // release. Keep holding; forwarding here would leak a partial credential.
                                continue;
                            }
                            return Some((
                                Ok(Frame::data(Bytes::from(emit))),
                                (Some(inner), carry),
                            ));
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
    const CODEX_ACCESS_PLACEHOLDER: &str = "synthetic.header.access-placeholder";
    const CODEX_ACCOUNT_PLACEHOLDER: &str = "account-placeholder";
    const CODEX_REAL_ACCESS: &str = "real-access-token-sentinel";
    const CODEX_REAL_ACCOUNT: &str = "real-account-sentinel";

    fn engine_with_job(placeholder: &str, upstream: &str) -> ProxyEngine {
        let engine = ProxyEngine::new([authority_of(UPSTREAM).unwrap()]);
        // Bypass `register`'s allowlist belt so `authorize`'s own allowlist check can be exercised in
        // isolation, including with a deliberately unapproved upstream.
        engine.creds.lock().unwrap().insert(
            placeholder.to_owned(),
            JobCredential {
                placeholder: placeholder.to_owned(),
                real: REAL.to_owned(),
                upstreams: vec![upstream.to_owned()],
            },
        );
        engine
    }

    fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn register_codex_session(engine: &ProxyEngine, upstream: &str) {
        engine
            .register_codex_session(CodexSessionCredential {
                access_placeholder: CODEX_ACCESS_PLACEHOLDER.to_owned(),
                access_token: CODEX_REAL_ACCESS.to_owned(),
                account_placeholder: CODEX_ACCOUNT_PLACEHOLDER.to_owned(),
                account_id: CODEX_REAL_ACCOUNT.to_owned(),
                upstream: upstream.to_owned(),
            })
            .expect("register the synthetic Codex session");
    }

    #[test]
    fn codex_session_rewrites_both_exact_headers_for_an_approved_request() {
        let engine = ProxyEngine::new(["chatgpt.com".to_owned()]);
        register_codex_session(&engine, "https://chatgpt.com/backend-api/codex");
        let headers = hdr(&[
            ("Authorization", &format!("Bearer {CODEX_ACCESS_PLACEHOLDER}")),
            ("ChatGPT-Account-ID", CODEX_ACCOUNT_PLACEHOLDER),
            ("content-type", "application/json"),
            ("connection", "keep-alive"),
        ]);

        match engine.authorize_request("POST", "/responses", &headers, None) {
            Decision::Forward {
                upstream,
                headers,
                scrub,
                redirects,
            } => {
                assert_eq!(upstream, "https://chatgpt.com/backend-api/codex");
                assert_eq!(redirects, RedirectMode::Disabled);
                assert_eq!(
                    headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                        .map(|(_, value)| value.as_str()),
                    Some("Bearer real-access-token-sentinel")
                );
                assert_eq!(
                    headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
                        .map(|(_, value)| value.as_str()),
                    Some(CODEX_REAL_ACCOUNT)
                );
                assert!(
                    !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("connection")),
                    "the typed route must still remove hop-by-hop headers"
                );
                assert_eq!(
                    scrub,
                    vec![
                        (CODEX_REAL_ACCESS.to_owned(), CODEX_ACCESS_PLACEHOLDER.to_owned()),
                        (CODEX_REAL_ACCOUNT.to_owned(), CODEX_ACCOUNT_PLACEHOLDER.to_owned()),
                    ]
                );
            }
            other => panic!("expected the Codex session to forward, got {other:?}"),
        }
    }

    #[test]
    fn codex_session_refuses_unapproved_methods_paths_and_headers() {
        let engine = ProxyEngine::new(["chatgpt.com".to_owned()]);
        register_codex_session(&engine, "https://chatgpt.com/backend-api/codex");
        let valid = hdr(&[
            ("authorization", &format!("Bearer {CODEX_ACCESS_PLACEHOLDER}")),
            ("chatgpt-account-id", CODEX_ACCOUNT_PLACEHOLDER),
        ]);

        for (method, path) in [("DELETE", "/responses"), ("POST", "/other")] {
            assert_eq!(
                engine.authorize_request(method, path, &valid, None),
                Decision::Refuse(Refusal::RequestNotAllowed {
                    method: method.to_owned(),
                    path: path.to_owned(),
                })
            );
        }

        for headers in [
            hdr(&[(
                "authorization",
                &format!("Bearer {CODEX_ACCESS_PLACEHOLDER}"),
            )]),
            hdr(&[
                ("authorization", &format!("Bearer {CODEX_ACCESS_PLACEHOLDER}")),
                ("chatgpt-account-id", "wrong-account-placeholder"),
            ]),
            hdr(&[
                ("authorization", &format!("Bearer {CODEX_ACCESS_PLACEHOLDER}")),
                ("authorization", "Bearer second-value"),
                ("chatgpt-account-id", CODEX_ACCOUNT_PLACEHOLDER),
            ]),
            hdr(&[
                ("authorization", &format!("Bearer {CODEX_ACCESS_PLACEHOLDER}")),
                ("chatgpt-account-id", CODEX_ACCOUNT_PLACEHOLDER),
                ("chatgpt-account-id", "second-account"),
            ]),
        ] {
            assert_eq!(
                engine.authorize_request("POST", "/responses", &headers, None),
                Decision::Refuse(Refusal::InvalidCodexHeaders)
            );
        }

        assert!(matches!(
            engine.authorize_request("GET", "/models", &valid, None),
            Decision::Forward { .. }
        ));

        let unsafe_placeholder = CodexSessionCredential {
            access_placeholder: CODEX_REAL_ACCESS.to_owned(),
            access_token: CODEX_REAL_ACCESS.to_owned(),
            account_placeholder: CODEX_ACCOUNT_PLACEHOLDER.to_owned(),
            account_id: CODEX_REAL_ACCOUNT.to_owned(),
            upstream: "https://chatgpt.com/backend-api/codex".to_owned(),
        };
        assert_eq!(
            engine.register_codex_session(unsafe_placeholder),
            Err(Refusal::InvalidCodexHeaders),
            "a placeholder must never equal a real session value"
        );
    }

    #[test]
    fn codex_session_decision_debug_text_redacts_real_values() {
        let engine = ProxyEngine::new(["chatgpt.com".to_owned()]);
        register_codex_session(&engine, "https://chatgpt.com/backend-api/codex");
        let headers = hdr(&[
            ("authorization", &format!("Bearer {CODEX_ACCESS_PLACEHOLDER}")),
            ("chatgpt-account-id", CODEX_ACCOUNT_PLACEHOLDER),
        ]);

        let debug = format!("{:?}", engine.authorize_request("POST", "/responses", &headers, None));
        assert!(!debug.contains(CODEX_REAL_ACCESS));
        assert!(!debug.contains(CODEX_REAL_ACCOUNT));
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

    // BACK-COMPAT IS THE ACCEPTANCE: a credential listing ONE upstream is decided exactly as it
    // always was — same Forward, same substitution target, same refusal shapes. The claude/codex
    // path IS this case (every env-sourced credential registers a one-entry list), so this test is
    // the direct answer to "does multi-upstream change anything for the existing seats".
    #[test]
    fn a_single_upstream_credential_is_decided_exactly_as_before() {
        let upstream = "https://api.anthropic.com".to_owned();
        let engine = ProxyEngine::new([authority_of(&upstream).unwrap()]);
        engine
            .register(JobCredential {
                placeholder: "PLACEHOLDER".into(),
                real: "REAL_VALUE".into(),
                upstreams: vec![upstream.clone()],
            })
            .expect("a single-upstream credential must register exactly as before");
        let headers = vec![("authorization".to_owned(), "Bearer PLACEHOLDER".to_owned())];
        match engine.authorize(&headers) {
            Decision::Forward { upstream: chosen, headers, .. } => {
                assert_eq!(chosen, upstream, "the primary path must forward to the one upstream");
                assert_eq!(
                    headers,
                    vec![("authorization".to_owned(), "Bearer REAL_VALUE".to_owned())],
                    "substitution must be unchanged"
                );
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    // The primary path never looks past the first entry: extra legs change nothing for traffic on
    // the primary listener.
    #[test]
    fn the_primary_path_ignores_extra_legs() {
        let a = "https://api2.cursor.sh".to_owned();
        let b = "https://agent.cursor.example".to_owned();
        let engine =
            ProxyEngine::new([authority_of(&a).unwrap(), authority_of(&b).unwrap()]);
        engine
            .register(JobCredential {
                placeholder: "PLACEHOLDER".into(),
                real: "REAL_VALUE".into(),
                upstreams: vec![a.clone(), b],
            })
            .expect("a two-upstream credential must register");
        let headers = vec![("authorization".to_owned(), "PLACEHOLDER".to_owned())];
        match engine.authorize(&headers) {
            Decision::Forward { upstream, .. } => {
                assert_eq!(upstream, a, "authorize must pick the FIRST entry, always");
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    // A leg listener serves exactly the credentials that listed its authority: one job's extra leg
    // never widens where another job's credential may go, even with both authorities allowlisted.
    #[test]
    fn a_leg_listener_serves_only_credentials_that_listed_its_authority() {
        let a = "https://api2.cursor.sh".to_owned();
        let b = "https://agent.cursor.example".to_owned();
        let b_authority = authority_of(&b).unwrap();
        let engine =
            ProxyEngine::new([authority_of(&a).unwrap(), b_authority.clone()]);
        engine
            .register(JobCredential {
                placeholder: "LISTS_BOTH".into(),
                real: "REAL_BOTH".into(),
                upstreams: vec![a.clone(), b.clone()],
            })
            .expect("the two-upstream credential must register");
        engine
            .register(JobCredential {
                placeholder: "LISTS_A_ONLY".into(),
                real: "REAL_A".into(),
                upstreams: vec![a],
            })
            .expect("the one-upstream credential must register");

        let both = vec![("authorization".to_owned(), "LISTS_BOTH".to_owned())];
        match engine.authorize_via(&both, &b_authority) {
            Decision::Forward { upstream, .. } => {
                assert_eq!(upstream, b, "the leg must forward to the credential's OWN entry for it");
            }
            other => panic!("expected Forward, got {other:?}"),
        }

        let a_only = vec![("authorization".to_owned(), "LISTS_A_ONLY".to_owned())];
        match engine.authorize_via(&a_only, &b_authority) {
            Decision::Refuse(Refusal::DestinationNotAllowed { host }) => {
                assert_eq!(host, b_authority, "the refusal must name the leg it refused");
            }
            other => panic!(
                "a credential that never listed this leg must be refused on it, got {other:?}"
            ),
        }
    }

    #[test]
    fn register_refuses_an_empty_upstream_list() {
        let engine = ProxyEngine::new(["api.anthropic.com".to_owned()]);
        let refusal = engine
            .register(JobCredential {
                placeholder: "PLACEHOLDER".into(),
                real: "REAL_VALUE".into(),
                upstreams: vec![],
            })
            .expect_err("an upstream-less credential must be refused");
        assert_eq!(refusal, Refusal::NoUpstream);
    }

    // Two entries naming one authority are refused rather than tie-broken — including the shape
    // where they differ only by an explicit default port and a path, which is how a real config
    // would most plausibly produce the duplicate.
    #[test]
    fn register_refuses_two_entries_naming_one_authority() {
        let engine = ProxyEngine::new(["api2.cursor.sh".to_owned()]);
        let refusal = engine
            .register(JobCredential {
                placeholder: "PLACEHOLDER".into(),
                real: "REAL_VALUE".into(),
                upstreams: vec![
                    "https://api2.cursor.sh".to_owned(),
                    "https://api2.cursor.sh:443/v2".to_owned(),
                ],
            })
            .expect_err("two entries for one authority must be refused");
        assert_eq!(
            refusal,
            Refusal::DuplicateUpstream { host: "api2.cursor.sh:443".to_owned() }
        );
    }

    // MULTI-UPSTREAM END TO END: WHICH LISTENER A REQUEST ARRIVES ON IS THE SELECTOR. One
    // credential lists two upstreams; the SAME placeholder sent to the primary listener reaches the
    // primary stub and sent to the leg listener reaches the leg stub, each with the REAL credential
    // substituted. The container-side story in one test: the base URL a leg was handed is the only
    // thing that routes it, and it can reach nothing the credential did not list.
    #[tokio::test]
    async fn the_listener_a_request_arrives_on_selects_the_upstream() {
        let (primary_addr, _primary_stub) = spawn_stub("PRIMARY_OK").await;
        let (leg_addr, leg_stub) = spawn_stub("LEG_OK").await;
        let primary = format!("http://{primary_addr}");
        let leg = format!("http://{leg_addr}");
        let leg_authority = authority_of(&leg).unwrap();
        let engine = Arc::new(ProxyEngine::new([
            authority_of(&primary).unwrap(),
            leg_authority.clone(),
        ]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstreams: vec![primary, leg],
            })
            .expect("a two-upstream credential must register");
        let running = start_with_legs(Arc::clone(&engine), None, &[leg_authority.clone()])
            .await
            .expect("a proxy with one leg must start");

        let client = reqwest::Client::new();
        let primary_answer = client
            .get(format!("http://127.0.0.1:{}/v1/messages", running.local_addr().port()))
            .header("x-api-key", &placeholder)
            .send()
            .await
            .expect("the primary listener must answer");
        assert_eq!(primary_answer.text().await.unwrap(), "PRIMARY_OK");

        let leg_port = running
            .leg_addr(&leg_authority)
            .expect("the leg listener must exist")
            .port();
        let leg_answer = client
            .get(format!("http://127.0.0.1:{leg_port}/v1/agent"))
            .header("x-api-key", &placeholder)
            .send()
            .await
            .expect("the leg listener must answer");
        assert_eq!(leg_answer.text().await.unwrap(), "LEG_OK");

        let seen_by_leg = leg_stub.await.expect("the leg stub must have been reached");
        assert_eq!(
            seen_by_leg.api_key.as_deref(),
            Some(REAL),
            "the leg upstream must see the REAL credential, substituted from the placeholder"
        );
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
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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

    #[tokio::test]
    async fn codex_session_proxy_keeps_the_body_and_scrubs_both_response_values() {
        let response_body = format!("access={CODEX_REAL_ACCESS};account={CODEX_REAL_ACCOUNT}");
        let response_header = format!(
            "x-session-echo: Bearer {CODEX_REAL_ACCESS}; account={CODEX_REAL_ACCOUNT}"
        );
        let (stub_addr, stub) = spawn_stub_with(response_body, Some(response_header)).await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        register_codex_session(&engine, &upstream);
        let proxy = start(Arc::clone(&engine), None)
            .await
            .expect("start the local proxy");
        let hostile_body = format!(
            "{{\"input\":\"{CODEX_ACCESS_PLACEHOLDER}:{CODEX_ACCOUNT_PLACEHOLDER}\"}}"
        );

        let response = reqwest::Client::new()
            .post(format!("http://{}/responses?test=1", proxy.local_addr()))
            .header(
                "authorization",
                format!("Bearer {CODEX_ACCESS_PLACEHOLDER}"),
            )
            .header("chatgpt-account-id", CODEX_ACCOUNT_PLACEHOLDER)
            .body(hostile_body.clone())
            .send()
            .await
            .expect("send through the local proxy");

        assert_eq!(response.status(), 200);
        let echoed_header = response
            .headers()
            .get("x-session-echo")
            .expect("the upstream response header")
            .to_str()
            .expect("text response header")
            .to_owned();
        let returned_body = response.text().await.expect("response body");
        assert!(!echoed_header.contains(CODEX_REAL_ACCESS));
        assert!(!echoed_header.contains(CODEX_REAL_ACCOUNT));
        assert!(echoed_header.contains(CODEX_ACCESS_PLACEHOLDER));
        assert!(echoed_header.contains(CODEX_ACCOUNT_PLACEHOLDER));
        assert_eq!(
            returned_body,
            format!("access={CODEX_ACCESS_PLACEHOLDER};account={CODEX_ACCOUNT_PLACEHOLDER}")
        );

        let seen = stub.await.expect("stub result");
        assert_eq!(seen.authorization.as_deref(), Some("Bearer real-access-token-sentinel"));
        assert_eq!(seen.account_id.as_deref(), Some(CODEX_REAL_ACCOUNT));
        assert_eq!(seen.body, hostile_body, "the request body must stay byte-identical");
        assert!(seen.request_line.contains("/responses?test=1"));
    }

    #[tokio::test]
    async fn codex_session_proxy_does_not_follow_a_same_host_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind redirect stub");
        let stub_addr = listener.local_addr().expect("redirect stub address");
        let stub = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.expect("first request");
            let mut request = [0_u8; 8192];
            let first_len = first.read(&mut request).await.expect("read first request");
            assert!(
                String::from_utf8_lossy(&request[..first_len]).contains("/responses"),
                "the first request must use the approved Codex route"
            );
            first
                .write_all(
                    b"HTTP/1.1 307 Temporary Redirect\r\nlocation: /outside\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("write redirect");
            first.flush().await.expect("flush redirect");

            match tokio::time::timeout(Duration::from_millis(500), listener.accept()).await {
                Ok(Ok((mut second, _))) => {
                    let second_len = second.read(&mut request).await.expect("read redirect target");
                    second
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await
                        .expect("write redirect target response");
                    Some(String::from_utf8_lossy(&request[..second_len]).into_owned())
                }
                _ => None,
            }
        });

        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        register_codex_session(&engine, &upstream);
        let proxy = start(Arc::clone(&engine), None)
            .await
            .expect("start the local proxy");
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build no-redirect test client");
        let response = client
            .post(format!("http://{}/responses", proxy.local_addr()))
            .header(
                "authorization",
                format!("Bearer {CODEX_ACCESS_PLACEHOLDER}"),
            )
            .header("chatgpt-account-id", CODEX_ACCOUNT_PLACEHOLDER)
            .body("{}")
            .send()
            .await
            .expect("send through the local proxy");

        assert_eq!(
            response.status(),
            reqwest::StatusCode::TEMPORARY_REDIRECT,
            "the proxy must return the redirect without another upstream request"
        );
        assert!(
            stub.await.expect("redirect stub result").is_none(),
            "the real Codex headers must not reach a redirected path"
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
            upstreams: vec!["https://evil.example.com".to_owned()],
        };
        assert!(matches!(engine.register(bad), Err(Refusal::DestinationNotAllowed { .. })));
        let good = JobCredential {
            placeholder: mint_anthropic_placeholder(),
            real: REAL.to_owned(),
            upstreams: vec![UPSTREAM.to_owned()],
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
        authorization: Option<String>,
        account_id: Option<String>,
        accept_encoding: Option<String>,
        /// The request BODY as the upstream received it — the ground truth for "no credential was
        /// written into a job-authored body".
        body: String,
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Decode an HTTP/1.1 chunked body, or `None` while the terminal zero-length chunk is still to
    /// come. `None` means "read more", never "empty" — collapsing those two is how a stub ends up
    /// asserting against a body it merely has not finished receiving.
    fn decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut rest = raw;
        loop {
            let line_end = find_subslice(rest, b"\r\n")?;
            // A chunk-size line may carry extensions after a `;`; the size is what precedes it.
            let size_text = std::str::from_utf8(&rest[..line_end]).ok()?;
            let size_text = size_text.split(';').next()?.trim();
            let size = usize::from_str_radix(size_text, 16).ok()?;
            rest = &rest[line_end + 2..];
            if size == 0 {
                return Some(out);
            }
            if rest.len() < size + 2 {
                return None;
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
    }

    /// Whatever decoded before the stream cut out. Only for the hung-up-mid-body path, so a test sees a
    /// short body and can assert on the shortfall instead of the stub blocking forever.
    fn decode_chunked_prefix(raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = raw;
        while let Some(line_end) = find_subslice(rest, b"\r\n") {
            let Some(size) = std::str::from_utf8(&rest[..line_end])
                .ok()
                .and_then(|t| t.split(';').next().map(str::trim).map(str::to_owned))
                .and_then(|t| usize::from_str_radix(&t, 16).ok())
            else {
                break;
            };
            rest = &rest[line_end + 2..];
            if size == 0 || rest.len() < size {
                break;
            }
            out.extend_from_slice(&rest[..size]);
            if rest.len() < size + 2 {
                break;
            }
            rest = &rest[size + 2..];
        }
        out
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
            // Read the body too — without this the stub could not prove what the upstream actually
            // received in the body.
            //
            // BOTH FRAMINGS, because the proxy relays the request body as a stream and so cannot
            // declare a length it does not know: reqwest frames such a body as `transfer-encoding:
            // chunked`. A content-length-only stub reads ZERO body bytes from a chunked request, then
            // answers and closes the socket while the sender is still writing — which surfaces as a
            // truncated body on a small request and a broken pipe (502) on a large one. Neither is a
            // fault in the thing under test.
            let head_end = find_subslice(&buf, b"\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
            let head_text = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let header_value = |wanted: &str| -> Option<String> {
                head_text.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case(wanted).then(|| value.trim().to_owned())
                })
            };
            let chunked = header_value("transfer-encoding")
                .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
            let raw_body = if chunked {
                loop {
                    if let Some(decoded) = decode_chunked(&buf[head_end..]) {
                        break decoded;
                    }
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        // Sender hung up mid-body: return what decoded so an assertion can see the
                        // truncation rather than the stub hanging on it.
                        break decode_chunked_prefix(&buf[head_end..]);
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
            } else {
                let content_length =
                    header_value("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                while buf.len() < head_end + content_length {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                buf[head_end..].to_vec()
            };
            let body = String::from_utf8_lossy(&raw_body).to_string();

            let text = head_text;
            let request_line = text.lines().next().unwrap_or_default().to_owned();
            let api_key = text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-api-key").then(|| value.trim().to_owned())
            });
            let authorization = text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            });
            let account_id = text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("chatgpt-account-id")
                    .then(|| value.trim().to_owned())
            });
            let accept_encoding = text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("accept-encoding")
                    .then(|| value.trim().to_owned())
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
            StubSeen {
                request_line,
                api_key,
                authorization,
                account_id,
                accept_encoding,
                body,
            }
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
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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
        let proxy = start(Arc::clone(&engine), Some(range))
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
        let second = start(Arc::clone(&engine), Some(range))
            .await
            .expect("a second free port in the range");
        assert_ne!(
            second.local_addr().port(),
            port,
            "two live proxies must not claim the same port"
        );

        // Control: unset ⇒ the shipped ephemeral behaviour. Without this the range assertion above
        // would also pass against a build that ignored the range entirely.
        let ephemeral = start(Arc::clone(&engine), None)
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
        let Err(error) = start(Arc::clone(&engine), Some(range)).await
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
        start(Arc::clone(&engine), Some(range))
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

    #[tokio::test]
    async fn an_encoded_upstream_response_is_refused_and_identity_is_requested() {
        let (stub_addr, stub) = spawn_stub_with(
            format!("encoded credential echo: {REAL}"),
            Some("content-encoding: x-maxplayer-test".to_owned()),
        )
        .await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstreams: vec![upstream],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None)
            .await
            .expect("start the local proxy");

        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/messages", proxy.local_addr()))
            .header("x-api-key", placeholder)
            .header("accept-encoding", "gzip")
            .body("{}")
            .send()
            .await
            .expect("send through the local proxy");
        let status = response.status();
        let body = response.bytes().await.expect("read refusal body");
        let seen = stub.await.expect("encoded stub result");

        assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY);
        assert!(!contains(&body, REAL), "the refusal must not contain the credential");
        assert_eq!(
            seen.accept_encoding.as_deref(),
            Some("identity"),
            "the proxy must replace a caller-selected encoding"
        );
    }

    #[tokio::test]
    async fn a_non_utf8_response_header_is_scrubbed_as_raw_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind raw-header stub");
        let stub_addr = listener.local_addr().expect("raw-header stub address");
        let stub = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("raw-header request");
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.expect("read raw-header request");
            let mut response = b"HTTP/1.1 200 OK\r\nx-raw-echo: prefix-\x80-".to_vec();
            response.extend_from_slice(REAL.as_bytes());
            response.extend_from_slice(
                b"-suffix\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
            );
            socket
                .write_all(&response)
                .await
                .expect("write raw response header");
            socket.flush().await.expect("flush raw response header");
        });

        let upstream = format!("http://{stub_addr}");
        let response = relay(
            &reqwest::Client::new(),
            &hyper::Method::POST,
            &upstream,
            "/v1/messages",
            vec![("x-api-key".to_owned(), REAL.to_owned())],
            reqwest::Body::from("{}"),
            vec![(REAL.to_owned(), "safe-placeholder".to_owned())],
            0,
            "test-upstream",
        )
        .await
        .expect("relay the raw response header");
        let raw = response
            .headers()
            .get("x-raw-echo")
            .expect("raw response header")
            .as_bytes();

        assert!(!contains(raw, REAL), "the raw response header must not contain the credential");
        assert!(
            contains(raw, "safe-placeholder"),
            "the raw response header must contain the placeholder"
        );
        stub.await.expect("raw-header stub result");
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
            vec![(REAL.to_owned(), placeholder.clone())],
            RESPONSE_HOLDBACK_FLUSH_IDLE,
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

    /// Regression for the contained-cursor stall (#: agent leg): a response that dribbles chunks
    /// SMALLER than `max_real_len - 1` and never sends EOF must still forward each chunk as it
    /// arrives. The stream yields three keepalives and then pends forever (no EOF, no idle gap
    /// reached — idle is set to an hour). Before the per-chunk release, `scrub_stream` held every
    /// sub-`max_real_len` chunk in the fixed-width carry and forwarded nothing until the idle flush,
    /// so this collected zero and the real client timed out. Now each keepalive passes straight
    /// through.
    #[tokio::test]
    async fn small_keepalive_chunks_stream_through_without_waiting_for_eof_or_idle() {
        use futures_util::StreamExt as _;
        let placeholder = mint_anthropic_placeholder();
        let keepalive = Bytes::from_static(b": keepalive\n"); // 12 B, well under REAL's length
        let count = 3usize;
        let frames: Vec<reqwest::Result<Bytes>> =
            (0..count).map(|_| Ok(keepalive.clone())).collect();
        // Keepalives, then a stream that never ends — the exact shape of a stalled SSE body.
        let stream = futures_util::stream::iter(frames).chain(futures_util::stream::pending());
        let mut scrubbed = Box::pin(scrub_stream(
            stream,
            vec![(REAL.to_owned(), placeholder.clone())],
            Duration::from_secs(3600), // idle backstop must NOT be what releases these bytes
        ));

        let want = keepalive.len() * count;
        let mut got: Vec<u8> = Vec::new();
        // Poll with a short real timeout: the per-chunk release yields in microseconds; the old
        // hold-until-idle path would yield nothing here and this loop would collect zero.
        while got.len() < want {
            match tokio::time::timeout(Duration::from_millis(500), scrubbed.next()).await {
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        got.extend_from_slice(&data);
                    }
                }
                Ok(Some(Err(error))) => panic!("unexpected stream error: {error}"),
                Ok(None) => break,
                Err(_) => break, // timed out with nothing more forthcoming
            }
        }
        assert_eq!(
            got.len(),
            want,
            "each keepalive must be forwarded as it arrives, not held for EOF or the idle flush"
        );
        assert_eq!(got, b": keepalive\n: keepalive\n: keepalive\n");
    }

    #[tokio::test]
    async fn codex_session_response_scrub_catches_both_values_across_chunks() {
        use futures_util::TryStreamExt as _;

        let (access_head, access_tail) = CODEX_REAL_ACCESS.split_at(8);
        let (account_head, account_tail) = CODEX_REAL_ACCOUNT.split_at(7);
        let frames: Vec<reqwest::Result<Bytes>> = vec![
            Ok(Bytes::from(format!("start-{access_head}"))),
            Ok(Bytes::from(format!("{access_tail}-middle-{account_head}"))),
            Ok(Bytes::from(format!("{account_tail}-end"))),
        ];
        let scrubbed = scrub_stream(
            futures_util::stream::iter(frames),
            vec![
                (CODEX_REAL_ACCESS.to_owned(), CODEX_ACCESS_PLACEHOLDER.to_owned()),
                (CODEX_REAL_ACCOUNT.to_owned(), CODEX_ACCOUNT_PLACEHOLDER.to_owned()),
            ],
            RESPONSE_HOLDBACK_FLUSH_IDLE,
        );
        let collected: Vec<u8> = scrubbed
            .try_fold(Vec::new(), |mut acc, frame| async move {
                if let Ok(data) = frame.into_data() {
                    acc.extend_from_slice(&data);
                }
                Ok(acc)
            })
            .await
            .expect("collect scrubbed frames");
        assert_eq!(
            String::from_utf8(collected).expect("UTF-8 result"),
            format!(
                "start-{CODEX_ACCESS_PLACEHOLDER}-middle-{CODEX_ACCOUNT_PLACEHOLDER}-end"
            )
        );
    }

    #[tokio::test]
    async fn the_response_scrub_handles_adjacent_values_without_reprocessing_output() {
        use futures_util::TryStreamExt as _;

        let frames: Vec<reqwest::Result<Bytes>> = vec![
            Ok(Bytes::from_static(b"bb")),
            Ok(Bytes::from_static(b"bb")),
        ];
        let scrubbed = scrub_stream(
            futures_util::stream::iter(frames),
            vec![("bb".to_owned(), "ab".to_owned())],
            RESPONSE_HOLDBACK_FLUSH_IDLE,
        );
        let collected: Vec<u8> = scrubbed
            .try_fold(Vec::new(), |mut acc, frame| async move {
                if let Ok(data) = frame.into_data() {
                    acc.extend_from_slice(&data);
                }
                Ok(acc)
            })
            .await
            .expect("collect adjacent scrub result");

        assert_eq!(collected, b"abab");
        assert!(!contains(&collected, "bb"));
    }

    // A STALLED upstream — bytes received, then silence with NO EOF — once left scrub_stream's holdback
    // withheld forever: the tail flushed ONLY on the `None`/EOF arm, which a stalled stream never
    // reaches, so the container got `received - (real.len() - 1)` bytes and no more — the credential-
    // scrub half of the "200 header, short-or-empty body, client gives up" symptom. Now the per-chunk
    // release forwards every provably-safe byte the instant the chunk arrives, so an ordinary body of
    // any size reaches the container IN FULL on arrival — no EOF, and without waiting for the idle
    // deadline at all. Driven with a large idle to prove the idle timer is NOT what delivers it.
    #[tokio::test]
    async fn a_stalled_ordinary_body_is_forwarded_in_full_on_arrival_without_eof_or_idle() {
        use futures_util::StreamExt as _;
        let placeholder = mint_anthropic_placeholder();
        let idle = Duration::from_secs(3600); // must NOT be what releases these bytes
        let hold = REAL.len() - 1; // the old fixed holdback width — deliberately exceeded
        // Ordinary bytes — no suffix of them can begin the credential, so the whole body is safe now.
        let body = vec![b'x'; hold + 27];
        let stalled = futures_util::stream::iter(vec![Ok(Bytes::from(body.clone()))])
            .chain(futures_util::stream::pending::<reqwest::Result<Bytes>>());
        let mut scrubbed =
            Box::pin(scrub_stream(stalled, vec![(REAL.to_owned(), placeholder.clone())], idle));

        // The entire body is forwarded on the chunk itself — the old code held `real.len() - 1` bytes
        // here until an EOF or idle flush that a stalled stream never reaches.
        let first = tokio::time::timeout(Duration::from_millis(500), scrubbed.next())
            .await
            .expect("the safe body must be forwarded on arrival, not on the idle deadline")
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        assert_eq!(
            first.len(),
            body.len(),
            "a stalled ordinary body reaches the container in full on arrival, with no EOF and no idle wait"
        );
    }

    // CONTAINMENT — the idle flush must never release a byte that could still begin a credential. A
    // stalled body ending in a proper PREFIX of the real credential is the exact straddle the holdback
    // guards: the flush releases only the bytes AHEAD of that prefix, and the prefix stays held even
    // though the stream never EOFs. The prefix leaves the boundary only by being completed into a full
    // credential (then replaced — see the split-across-chunks test) or on a real EOF; never on idle.
    #[tokio::test]
    async fn a_stalled_credential_prefix_is_never_released_by_the_idle_flush() {
        use futures_util::StreamExt as _;
        let placeholder = mint_anthropic_placeholder();
        let idle = Duration::from_millis(50);
        let k = 12; // "sk-ant-api03" — a proper, non-empty prefix of REAL
        let prefix = &REAL.as_bytes()[..k];
        let mut body = vec![b'x'; 40];
        body.extend_from_slice(prefix);
        let stalled = futures_util::stream::iter(vec![Ok(Bytes::from(body.clone()))])
            .chain(futures_util::stream::pending::<reqwest::Result<Bytes>>());
        let mut scrubbed =
            Box::pin(scrub_stream(stalled, vec![(REAL.to_owned(), placeholder.clone())], idle));

        // The only frame the idle flush will release: exactly the 40 bytes ahead of the prefix. The k
        // prefix bytes are withheld, so no fragment of the credential is ever forwarded.
        let flushed = scrubbed.next().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(
            flushed.as_ref(),
            vec![b'x'; 40].as_slice(),
            "the idle flush releases only the bytes that cannot begin a credential; the prefix stays held"
        );
        // A further await is intentionally omitted: with the prefix held and no more data and no EOF,
        // the stream never yields again — that permanent hold on the prefix IS the containment property.
    }

    #[test]
    fn straddle_prefix_len_finds_the_longest_suffix_that_is_a_credential_prefix() {
        let needle = b"sk-ant-api03-xyz";
        // Nothing at the tail could begin the needle ⇒ 0 (the whole buffer is safe to flush).
        assert_eq!(straddle_prefix_len(b"hello world", needle), 0);
        assert_eq!(straddle_prefix_len(b"", needle), 0);
        // A trailing partial prefix is detected at exactly its length.
        assert_eq!(straddle_prefix_len(b"xxxxsk-ant", needle), 6);
        // The LONGEST qualifying suffix wins, not a shorter coincidence earlier in the buffer.
        assert_eq!(straddle_prefix_len(b"sk-sk-ant", needle), 6);
        // The full proper prefix reports its own length — the cap is needle.len() - 1, so a complete
        // occurrence (which scrub_buffer strips first) is never reported as an unbreakable hold.
        assert_eq!(straddle_prefix_len(&needle[..needle.len() - 1], needle), needle.len() - 1);
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
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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

    // THE BUG THIS CHANGE EXISTS FOR, in miniature. A client that holds its request body OPEN — which
    // is what a streaming agent turn looks like on the wire — used to be unanswerable: the proxy
    // collected the body to completion before authorizing, so a body that never ends meant a decision
    // that never came. The client gave up first and the request was never forwarded at all.
    //
    // Authorizing from the headers makes the refusal reachable while the body is still open. The
    // outer timeout is the assertion: on the buffering code this test does not fail, it HANGS.
    #[tokio::test]
    async fn a_request_is_refused_from_its_headers_while_its_body_is_still_open() {
        let engine = Arc::new(ProxyEngine::new([authority_of(UPSTREAM).unwrap()]));
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
        let port = proxy.local_addr().port();

        // One chunk, then never another and never an end — the shape of a client mid-turn.
        let never_ends = futures_util::stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"{\"messages\":["))
        })
        .chain(futures_util::stream::pending());

        let response = tokio::time::timeout(
            Duration::from_secs(10),
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .body(reqwest::Body::wrap_stream(never_ends))
                .send(),
        )
        .await
        .expect("the proxy must answer from the headers, not wait for the body to end")
        .unwrap();

        // No placeholder anywhere, so this is `NoKnownPlaceholder` — and crucially it is decided
        // without a single body byte being required.
        assert_eq!(response.status(), 502);
    }

    // A large body relays in full and UNBUFFERED. The old cap bounded a buffer that no longer exists,
    // so the interesting size is no longer "what fits in memory" — it is that nothing between the
    // container and the upstream truncates. Sized just under MAX_REQUEST_BODY_BYTES so it proves the
    // relay path carries a large body byte-for-byte while leaving the cap itself to the two tests
    // below; the assertion is on byte-for-byte arrival rather than on a status alone.
    #[tokio::test]
    async fn a_large_body_relays_in_full() {
        let (stub_addr, stub) = spawn_stub("UPSTREAM_OK").await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
        let port = proxy.local_addr().port();

        // Distinctive bytes so a truncation cannot be mistaken for success.
        let big = vec![b'z'; MAX_REQUEST_BODY_BYTES - 1];
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", &placeholder)
            .body(big.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "an under-cap body must be relayed, not refused");

        let seen = stub.await.unwrap();
        assert_eq!(
            seen.body.len(),
            big.len(),
            "the upstream must receive every byte, not a capped prefix"
        );
        assert_eq!(seen.api_key.as_deref(), Some(REAL));
    }

    // The size cap SURVIVED the move to a streamed body. Streaming removed the reason the cap was
    // sized the way it was — there is no buffer to overflow — but not the cap itself: it is also the
    // ceiling on what a stranger's job may push through the seller's credential in one request.
    //
    // A declared `content-length` is refused with the buffered path's own `413`, and refused BEFORE
    // anything reaches the upstream, which is the half of the old behaviour a mid-stream check cannot
    // reproduce.
    #[tokio::test]
    async fn a_declared_over_cap_body_is_refused_before_the_upstream_sees_it() {
        let (stub_addr, stub) = spawn_stub("UPSTREAM_OK").await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
        let port = proxy.local_addr().port();

        // One byte over the cap, declared up front: `reqwest` sets `content-length` for a sized body.
        let over = vec![b'z'; MAX_REQUEST_BODY_BYTES + 1];
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", &placeholder)
            .body(over)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            413,
            "a declared over-cap body keeps the buffered path's refusal"
        );

        // The upstream must never have been dialled: the refusal is decided from the header alone, so
        // the real credential was never put on the wire for this request. Checked WITHOUT a timer —
        // the stub completes only once it has served a connection, and the `413` above is already in
        // hand, so "still running" is a settled fact here rather than a race against a deadline.
        assert!(
            !stub.is_finished(),
            "an over-cap request must not reach the upstream at all"
        );
    }

    // The backstop for a body whose length is NOT declared: a chunked stream cannot be judged from its
    // headers, so the cap has to be enforced as the bytes arrive. The request fails mid-flight, which
    // is a `502` rather than a `413` because the upstream request is already open by then — the cap
    // still holds, and that difference is the price of not buffering to find out.
    #[tokio::test]
    async fn an_undeclared_over_cap_body_is_cut_off_mid_stream() {
        let (stub_addr, _stub) = spawn_stub("UPSTREAM_OK").await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
        let port = proxy.local_addr().port();

        // A chunk stream carries no `content-length`, so this passes the header check and is caught by
        // `size_bounded` only once the running total passes the cap.
        let chunks = futures_util::stream::iter(
            (0..9).map(|_| Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'z'; 4 * 1024 * 1024]))),
        );
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", &placeholder)
            .body(reqwest::Body::wrap_stream(chunks))
            .send()
            .await;
        // Either the proxy answers 502, or the connection dies under the aborted upstream request.
        // Both are the request failing; neither is the body being relayed past the cap.
        match response {
            Ok(response) => assert_eq!(
                response.status(),
                502,
                "an over-cap chunked body must fail, not relay"
            ),
            Err(_) => {}
        }
    }

    // The deadline is an INACTIVITY one, and this is the test that tells the two apart: five chunks
    // spaced under the deadline run PAST it in total. A total-body deadline fails here; a gap deadline
    // does not. Without this assertion, `BODY_IDLE_TIMEOUT` could be a total deadline under a new name
    // and every other test would still pass.
    #[tokio::test]
    async fn a_slow_but_talking_body_is_not_cut_off_by_the_idle_deadline() {
        let idle = Duration::from_millis(120);
        let chunks = futures_util::stream::unfold(0u8, |n| async move {
            if n == 5 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            Some((Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'a'; 4])), n + 1))
        });

        let relayed: Vec<Bytes> = idle_bounded(chunks, idle)
            .map(|chunk| chunk.expect("a body that keeps arriving must never hit the idle deadline"))
            .collect()
            .await;

        assert_eq!(relayed.len(), 5, "every chunk must be relayed");
        assert_eq!(relayed.iter().map(Bytes::len).sum::<usize>(), 20);
        // 5 x 40ms = 200ms elapsed against a 120ms deadline: a TOTAL deadline would have fired.
    }

    // The other half of the same guard: a body that goes SILENT is dropped, so a client cannot pin a
    // credential-bearing upstream connection open by simply stopping. Errors rather than hanging.
    #[tokio::test]
    async fn a_body_that_goes_silent_is_dropped_at_the_idle_deadline() {
        let one_then_silence = futures_util::stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(b"partial"))
        })
        .chain(futures_util::stream::pending());

        let outcome: Vec<Result<Bytes, std::io::Error>> =
            idle_bounded(one_then_silence, Duration::from_millis(80)).collect().await;

        assert_eq!(outcome.len(), 2, "the chunk that did arrive, then the failure");
        assert_eq!(outcome[0].as_ref().unwrap(), &Bytes::from_static(b"partial"));
        let error = outcome[1].as_ref().expect_err("silence must end the stream in an error");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
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
        let proxy = start(Arc::clone(&engine), None).await.unwrap();

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
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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
    // against a limit of 64, and each admitted request holds a relay open for as long as its body keeps
    // talking — which is the resource this ceiling exists to bound.
    //
    // It bounds CONCURRENCY, not bytes. A relayed body has no size ceiling by design, so the number of
    // simultaneous relays is the only quantity left to limit, and the per-job lifetime is what bounds
    // how long any one of them can last.
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
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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

    /// What an early-answering upstream had in hand at the moment it chose to answer.
    struct EarlySeen {
        api_key: Option<String>,
        body_prefix: String,
    }

    /// A stub upstream that answers as soon as it holds the head and `want` body bytes, WITHOUT waiting
    /// for the request body to end.
    ///
    /// This is the only upstream shape that can witness the property under test. Every other stub here
    /// drains to the terminal chunk before answering, and once a body has ended, a relayed one and a
    /// buffered-then-relayed one are indistinguishable — so those stubs cannot tell the two apart no
    /// matter what is asserted afterwards.
    ///
    /// It reports through a channel rather than its `JoinHandle` because the handle does not resolve
    /// until the socket drains, which is exactly the wait the test exists to avoid.
    async fn spawn_early_answering_stub(
        want: usize,
        body_out: &'static str,
    ) -> (SocketAddr, tokio::sync::oneshot::Receiver<EarlySeen>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (report_tx, report_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0_u8; 4096];

            while find_subslice(&buf, b"\r\n\r\n").is_none() {
                let n = sock.read(&mut tmp).await.unwrap();
                assert!(n > 0, "upstream connection closed before a request head arrived");
                buf.extend_from_slice(&tmp[..n]);
            }
            let head_end = find_subslice(&buf, b"\r\n\r\n").unwrap() + 4;
            let head_text = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let api_key = head_text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-api-key").then(|| value.trim().to_owned())
            });

            // The body PREFIX only. The terminal chunk is never awaited.
            while decode_chunked_prefix(&buf[head_end..]).len() < want {
                let n = sock.read(&mut tmp).await.unwrap();
                assert!(
                    n > 0,
                    "upstream connection closed holding {} of {want} body bytes",
                    decode_chunked_prefix(&buf[head_end..]).len()
                );
                buf.extend_from_slice(&tmp[..n]);
            }
            let body_prefix =
                String::from_utf8_lossy(&decode_chunked_prefix(&buf[head_end..])).to_string();

            // Answer now, with the client's request body still open.
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: text/plain\r\n\r\n{}",
                body_out.len(),
                body_out
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            let _ = report_tx.send(EarlySeen { api_key, body_prefix });

            // Keep draining, so finishing the request body upstream never faults on a closed socket.
            while let Ok(n) = sock.read(&mut tmp).await {
                if n == 0 {
                    break;
                }
            }
        });
        (addr, report_rx)
    }

    // THE PROPERTY THIS CHANGE EXISTS TO DELIVER: a request body that is still open has already been
    // relayed, and its answer comes back before that body ends. Streaming is what makes this possible;
    // buffering is what made it impossible.
    //
    // No other test in this file can substitute for it, because every other one closes the request body
    // before asserting anything — and after the body has ended, a streamed relay and a buffered one look
    // identical from every vantage point a test has. This test cannot pass while buffered: the client
    // withholds END_STREAM until it is answered, and a proxy that waits for END_STREAM before opening
    // the upstream connection deadlocks against that, which is the red-prove.
    //
    // Each assertion names the production code it requires to have executed, so that coverage is a
    // property of the assertions and not of the author's confidence:
    //   - the credential requires `authorize` to have returned `Forward` AND `relay` to have reached a
    //     real upstream socket, since a refusal never opens one;
    //   - the body prefix requires the open body to have been relayed AS A STREAM, since the only bytes
    //     in existence at that moment arrived before END_STREAM;
    //   - the status requires the upstream's own answer to have travelled back through that relay.
    #[tokio::test]
    async fn a_body_still_open_is_already_relayed_and_answered_by_the_upstream() {
        const PREFIX: &str = r#"{"model":"claude","stream":true,"messages":["#;

        let (stub_addr, seen_rx) = spawn_early_answering_stub(PREFIX.len(), "UPSTREAM_OK").await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
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
        // END_STREAM clear on the HEADERS...
        let (response, mut body) = send.send_request(request, false).unwrap();
        // ...and clear on the DATA frame as well. The request is deliberately unfinished.
        body.reserve_capacity(PREFIX.len());
        body.send_data(Bytes::from_static(PREFIX.as_bytes()), false).unwrap();

        let response = tokio::time::timeout(Duration::from_secs(10), response)
            .await
            .expect(
                "no response arrived while the request body was still open, so the proxy is waiting \
                 for the end of a body the client will not end until it has been answered",
            )
            .expect("the h2 stream must not be reset");

        // Nothing above this line ends the request stream, so the response necessarily arrived first.
        // The ordering is structural — a statement that has not run yet cannot have sent END_STREAM —
        // and so it holds on any machine at any speed, which a sleep or an elapsed-time check would not.
        assert!(
            response.status().is_success(),
            "expected the upstream's 200, got {}",
            response.status()
        );

        let seen = tokio::time::timeout(Duration::from_secs(10), seen_rx)
            .await
            .expect("the upstream must have been reached while the body was open")
            .expect("the stub upstream panicked");
        assert_eq!(
            seen.api_key.as_deref(),
            Some(REAL),
            "the upstream must see the REAL credential, substituted from the placeholder"
        );
        assert_eq!(
            seen.body_prefix, PREFIX,
            "the upstream must have received the still-open body's first chunk"
        );

        // Only now does the client finish its request.
        body.send_data(Bytes::new(), true).unwrap();
    }

    /// What a never-answering upstream held when the request reached it.
    struct UpstreamArrival {
        api_key: Option<String>,
        body_prefix: String,
    }

    /// A stub upstream that never answers, reporting TWICE: once when the head and `want` body bytes
    /// have arrived, and again when its own socket closes.
    ///
    /// Both reports are load-bearing. The close tells a test that the upstream request was torn down;
    /// the arrival is what stops that from being vacuous, because a request that never reached the
    /// upstream at all also leaves a closed socket and no completed body. Without the first report, a
    /// client abandoned too early passes the teardown assertion for the wrong reason.
    ///
    /// The second report carries whether the chunked request body ever reached its terminal chunk —
    /// `false` means the upstream saw an INCOMPLETE request, which is the property being asserted.
    /// "Socket closed" alone cannot distinguish a torn-down relay from one that was quietly completed
    /// on behalf of a client that had already gone.
    async fn spawn_never_answering_stub(
        want: usize,
    ) -> (
        SocketAddr,
        tokio::sync::oneshot::Receiver<UpstreamArrival>,
        tokio::sync::oneshot::Receiver<bool>,
    ) {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0_u8; 4096];

            while find_subslice(&buf, b"\r\n\r\n").is_none() {
                let n = sock.read(&mut tmp).await.unwrap();
                assert!(n > 0, "upstream connection closed before a request head arrived");
                buf.extend_from_slice(&tmp[..n]);
            }
            let head_end = find_subslice(&buf, b"\r\n\r\n").unwrap() + 4;
            let head_text = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let api_key = head_text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-api-key").then(|| value.trim().to_owned())
            });
            while decode_chunked_prefix(&buf[head_end..]).len() < want {
                let n = sock.read(&mut tmp).await.unwrap();
                assert!(
                    n > 0,
                    "upstream connection closed holding {} of {want} body bytes",
                    decode_chunked_prefix(&buf[head_end..]).len()
                );
                buf.extend_from_slice(&tmp[..n]);
            }
            let body_prefix =
                String::from_utf8_lossy(&decode_chunked_prefix(&buf[head_end..])).to_string();
            let _ = arrived_tx.send(UpstreamArrival { api_key, body_prefix });

            // Now wait to be torn down, answering nothing.
            loop {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            let _ = closed_tx.send(decode_chunked(&buf[head_end..]).is_some());
        });
        (addr, arrived_rx, closed_rx)
    }

    // CANCELLATION, CLIENT SIDE. A relayed body is only safe if abandoning the client tears the upstream
    // request down with it. That request carries the REAL credential, so a relay still running after the
    // job that authorized it has gone is an in-flight credential nobody is waiting for.
    //
    // Buffering never had to answer this: there was no upstream request until the body had ended, so
    // there was nothing to tear down. Streaming opens the upstream while the client can still vanish,
    // which makes the teardown a new obligation rather than an inherited one.
    #[tokio::test]
    async fn abandoning_the_client_tears_down_the_credential_bearing_upstream_request() {
        const PREFIX: &str = r#"{"model":"claude","stream":true,"messages":["#;

        let (stub_addr, arrived_rx, closed_rx) = spawn_never_answering_stub(PREFIX.len()).await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
        let proxy_addr = proxy.local_addr();

        let io = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        let (send, connection) = h2::client::handshake(io).await.unwrap();
        let connection = tokio::spawn(async move {
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
        body.reserve_capacity(PREFIX.len());
        body.send_data(Bytes::from_static(PREFIX.as_bytes()), false).unwrap();

        // POSITIVE CONTROL, and the reason this test is not vacuous: there IS a live upstream request,
        // carrying the real credential, before anything is abandoned.
        let arrival = tokio::time::timeout(Duration::from_secs(10), arrived_rx)
            .await
            .expect("the upstream must be reached while the body is open")
            .expect("the stub upstream panicked");
        assert_eq!(
            arrival.api_key.as_deref(),
            Some(REAL),
            "the request under test must be the credential-bearing one"
        );
        assert_eq!(arrival.body_prefix, PREFIX, "the open body must have been relayed");

        // The job dies: the container's socket goes away mid-body, with no END_STREAM and no reset.
        drop(body);
        drop(send);
        drop(response);
        connection.abort();

        let request_completed = tokio::time::timeout(Duration::from_secs(10), closed_rx)
            .await
            .expect(
                "the upstream request outlived the client that authorized it — a credential-bearing \
                 relay is still running for a job that has gone",
            )
            .expect("the stub upstream panicked");
        assert!(
            !request_completed,
            "the abandoned request was relayed upstream as a COMPLETE request: the client vanished \
             mid-body, so finishing it upstream sends the real credential on a request nobody asked \
             to finish. A closed socket alone would have hidden this."
        );
    }

    // CANCELLATION, UPSTREAM SIDE. An upstream that dies mid-relay must release BOTH the client and the
    // in-flight permit.
    //
    // The permit is the half a single-request test cannot see. One leaked permit per failure is
    // invisible until the ceiling is reached, and then the proxy stops serving anything at all while
    // every visible signal — process alive, port listening, no errors — still reads as healthy. So the
    // assertion is a COUNT, not a case: more failures than the ceiling, all of which must be answered.
    // With permits leaking, the first `MAX_IN_FLIGHT_REQUESTS` are answered and the remainder hang.
    #[tokio::test]
    async fn an_upstream_that_dies_mid_relay_releases_both_the_client_and_its_permit() {
        use tokio::io::AsyncReadExt;

        const OVERFLOW: usize = 4;
        let attempts = MAX_IN_FLIGHT_REQUESTS + OVERFLOW;

        // An upstream that accepts, reads, and hangs up without ever answering.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut tmp = [0_u8; 4096];
                    let _ = sock.read(&mut tmp).await;
                    // Drop the socket mid-request: the relay fails with the client still waiting.
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
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
        let port = proxy.local_addr().port();

        let mut requests = Vec::new();
        for _ in 0..attempts {
            let placeholder = placeholder.clone();
            requests.push(tokio::spawn(async move {
                reqwest::Client::new()
                    .post(format!("http://127.0.0.1:{port}/v1/messages"))
                    .header("x-api-key", &placeholder)
                    .body("{}")
                    .send()
                    .await
                    .map(|response| response.status())
            }));
        }

        // Every one of them must be ANSWERED. The client-release half is that none hangs; the
        // permit-release half is that there are more of them than the ceiling.
        for (n, request) in requests.into_iter().enumerate() {
            let status = tokio::time::timeout(Duration::from_secs(30), request)
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "request {n} of {attempts} never got an answer. Past \
                         {MAX_IN_FLIGHT_REQUESTS}, that is a permit not released on the upstream \
                         error path, and the proxy is now wedged while looking healthy"
                    )
                })
                .expect("the request task panicked")
                .expect("a failed upstream must produce a response, not a transport error");
            assert_eq!(
                status,
                StatusCode::BAD_GATEWAY,
                "an upstream that dies mid-relay must surface as 502 to the container"
            );
        }
    }

    /// A stub upstream that never answers and reports EVERY arrival, plus each socket close.
    ///
    /// Multi-arrival is the point: revocation is a claim about a SECOND request, and a stub that can
    /// only witness one cannot tell "the second was refused" from "the stub stopped listening".
    #[allow(clippy::type_complexity)]
    async fn spawn_arrival_counting_stub() -> (
        SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        tokio::sync::mpsc::UnboundedReceiver<bool>,
    ) {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (arrived_tx, arrived_rx) = tokio::sync::mpsc::unbounded_channel();
        let (closed_tx, closed_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let arrived_tx = arrived_tx.clone();
                let closed_tx = closed_tx.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0_u8; 4096];
                    while find_subslice(&buf, b"\r\n\r\n").is_none() {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let head_end = find_subslice(&buf, b"\r\n\r\n").unwrap() + 4;
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let key = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("x-api-key")
                                .then(|| value.trim().to_owned())
                        })
                        .unwrap_or_else(|| "<absent>".to_owned());
                    let _ = arrived_tx.send(key);
                    // Answer nothing; wait to be torn down, then say whether the body ever finished.
                    loop {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let _ = closed_tx.send(decode_chunked(&buf[head_end..]).is_some());
                });
            }
        });
        (addr, arrived_rx, closed_rx)
    }

    // REVOCATION MUST REACH CONNECTIONS ALREADY ACCEPTED, not just the listener.
    //
    // Every connection used to be a detached `tokio::spawn`, and tokio tasks are not scoped to their
    // spawner — so `RunningProxy::drop` aborting the accept loop closed the listener while every open
    // connection kept running with `Arc<ProxyEngine>` in hand. The placeholder still resolved on such a
    // connection, which makes "the placeholder stops working at job end" false for exactly the client
    // that already has one. With a relayed body and no byte cap, that outlives the job indefinitely.
    //
    // Proving the listener refuses NEW connections would not test this at all: the whole failure lives
    // on a socket accepted before the drop. So the client here is deliberately kept alive across it.
    #[tokio::test]
    async fn dropping_the_proxy_revokes_connections_it_has_already_accepted() {
        const PREFIX: &str = r#"{"model":"claude","stream":true,"messages":["#;

        let (stub_addr, mut arrived, mut closed) = spawn_arrival_counting_stub().await;
        let upstream = format!("http://{stub_addr}");
        let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
        let placeholder = mint_anthropic_placeholder();
        engine
            .register(JobCredential {
                placeholder: placeholder.clone(),
                real: REAL.to_owned(),
                upstreams: vec![upstream.clone()],
            })
            .unwrap();
        let proxy = start(Arc::clone(&engine), None).await.unwrap();
        let proxy_addr = proxy.local_addr();

        let io = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        let (send, connection) = h2::client::handshake(io).await.unwrap();
        let connection = tokio::spawn(async move {
            let _ = connection.await;
        });

        let open_request = |send: &h2::client::SendRequest<Bytes>| {
            let request = hyper::Request::builder()
                .method("POST")
                .uri(format!("http://{proxy_addr}/v1/messages"))
                .header("x-api-key", &placeholder)
                .body(())
                .unwrap();
            let mut send = send.clone();
            let (response, mut body) = send.send_request(request, false).unwrap();
            body.reserve_capacity(PREFIX.len());
            body.send_data(Bytes::from_static(PREFIX.as_bytes()), false).unwrap();
            (response, body)
        };

        // POSITIVE CONTROL: an authorized stream, credential-bearing, upstream, body still open.
        let send = send.ready().await.unwrap();
        let (_first_response, first_body) = open_request(&send);
        let first_key = tokio::time::timeout(Duration::from_secs(10), arrived.recv())
            .await
            .expect("the first request must reach the upstream while its body is open")
            .expect("the stub upstream closed");
        assert_eq!(
            first_key, REAL,
            "the control stream must be the credential-bearing one, or nothing below is about \
             revoking a credential"
        );

        // JOB END, with the client's socket deliberately still alive and still connected.
        drop(proxy);

        // (a) The credential-bearing upstream request is torn down, and torn down INCOMPLETE.
        let completed = tokio::time::timeout(Duration::from_secs(10), closed.recv())
            .await
            .expect(
                "the upstream request survived job end: an already-accepted connection is still \
                 relaying under the real credential after the proxy was dropped",
            )
            .expect("the stub upstream closed");
        assert!(
            !completed,
            "the revoked request was finished upstream rather than cut, so job end completed a \
             credential-bearing request instead of revoking it"
        );

        // (b) A FURTHER stream on that same already-accepted connection reaches no upstream. The
        // assertion is an absence, which is only worth anything because the identical call above
        // produced an arrival on this very connection moments earlier.
        let second = send.ready().await.map(|ready| open_request(&ready));
        assert!(
            tokio::time::timeout(Duration::from_millis(750), arrived.recv()).await.is_err(),
            "a new stream on a connection accepted BEFORE job end still reached the upstream, so the \
             placeholder outlives the job on any connection that was already open"
        );

        drop(first_body);
        drop(second);
        connection.abort();
    }

    /// Serve requests on an already-bound listener, answering the first with `status` and a `Location`
    /// of `location`, and any later one with `200`. The returned counter is the number of requests that
    /// reached this host, which is how a test sees whether a redirect was followed to it.
    ///
    /// One request per connection, every response `connection: close`. A keep-alive stub would have to
    /// resynchronise past the bytes of a request body that was aborted mid-flight when the redirect was
    /// answered, and a stub that mis-parses a leftover chunk would report a request count that no
    /// assertion could interpret.
    fn serve_counting(
        listener: tokio::net::TcpListener,
        status: u16,
        location: Option<String>,
    ) -> (Arc<std::sync::atomic::AtomicUsize>, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let hits = Arc::new(AtomicUsize::new(0));
        let keys: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let counter = Arc::clone(&hits);
        let seen_keys = Arc::clone(&keys);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let counter = Arc::clone(&counter);
                let seen_keys = Arc::clone(&seen_keys);
                let location = location.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0_u8; 4096];
                    while find_subslice(&buf, b"\r\n\r\n").is_none() {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    // Capture the credential this host was handed. A hit count says a host was
                    // CONTACTED; only the header says what it was told, and on a credential path those
                    // are different findings.
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let request_line = head.lines().next().unwrap_or_default().to_owned();
                    let key = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("x-api-key")
                                .then(|| value.trim().to_owned())
                        })
                        .unwrap_or_else(|| "<absent>".to_owned());
                    seen_keys.lock().unwrap().push(format!("[{request_line}] x-api-key={key}"));
                    let nth = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    // The first request gets the redirect; a followed one gets a plain 200, so a test
                    // can tell "followed" from "surfaced" by the count alone.
                    let response = match (nth, &location) {
                        (1, Some(location)) => format!(
                            "HTTP/1.1 {status} Redirect\r\nlocation: {location}\r\n\
                             content-length: 0\r\nconnection: close\r\n\r\n"
                        ),
                        _ => "HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\n\
                              FOLLOWED"
                            .to_owned(),
                    };
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (hits, keys)
    }

    // THE STREAMED-BODY REDIRECT MATRIX, measured on sockets rather than deduced from library source.
    //
    // A streamed request body cannot be replayed, so a redirect that must resend the body cannot be
    // followed. That is a behaviour change, so it is pinned here per status instead of described in a
    // comment, and it is measured end to end: the deduction from `tower-http`'s source is what suggested
    // these numbers, and only this test establishes them.
    //
    // The column that must never move is the last one. A host the credential was never registered for
    // receives NOTHING, at every status. Losing a follow is a functional regression; sending `x-api-key`
    // to an unapproved authority would be an incident, so the two are asserted separately and the
    // security clause is asserted for all ten cases rather than for the interesting ones.
    #[tokio::test]
    async fn the_streamed_body_redirect_matrix_is_pinned_per_status() {
        use std::sync::atomic::Ordering;

        let mut measured = Vec::new();
        for status in [301_u16, 302, 303, 307, 308] {
            for cross_authority in [false, true] {
                // Bind both hosts first: the same-authority Location has to name the upstream's own
                // address, which does not exist until it is bound.
                let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr_a = listener_a.local_addr().unwrap();
                let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr_b = listener_b.local_addr().unwrap();
                let target = if cross_authority { addr_b } else { addr_a };
                let (hits_a, keys_a) = serve_counting(
                    listener_a,
                    status,
                    Some(format!("http://{target}/redirected")),
                );
                let (hits_b, keys_b) = serve_counting(listener_b, 200, None);

                let upstream = format!("http://{addr_a}");
                let engine = Arc::new(ProxyEngine::new([authority_of(&upstream).unwrap()]));
                let placeholder = mint_anthropic_placeholder();
                engine
                    .register(JobCredential {
                        placeholder: placeholder.clone(),
                        real: REAL.to_owned(),
                        upstreams: vec![upstream.clone()],
                    })
                    .unwrap();
                let proxy = start(Arc::clone(&engine), None).await.unwrap();
                let port = proxy.local_addr().port();

                // THE CONTAINER'S CLIENT MUST NOT FOLLOW REDIRECTS, or this test cannot measure the
                // proxy at all. With the default policy it follows every surfaced `3xx` ITSELF, direct
                // to the named host and bypassing the proxy entirely — which registers as a hit on
                // that host and a `200` at the container, and reads exactly like the proxy having
                // followed the redirect. Measured before this line existed: same-authority 307/308
                // showed two upstream hits and looked followed, and the second hit was this client.
                //
                // The tell was one layer out and had to be asked for: only the proxy holds the REAL
                // credential, so the second request bearing the PLACEHOLDER named the container as its
                // author. Hit counts alone cannot separate the two actors.
                let container = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap();
                let observed = tokio::time::timeout(
                    Duration::from_secs(20),
                    container
                        .post(format!("http://127.0.0.1:{port}/v1/messages"))
                        .header("x-api-key", &placeholder)
                        .body(r#"{"model":"claude"}"#)
                        .send(),
                )
                .await
                .expect("the proxy must answer a redirected request, not hang")
                .expect("the container must get a response")
                .status()
                .as_u16();

                // Settle: a follow that is going to happen has already happened by the time the
                // container holds a response, because the container's response IS the followed one.
                let a = hits_a.load(Ordering::SeqCst);
                let b = hits_b.load(Ordering::SeqCst);
                let leaked = keys_b.lock().unwrap().iter().any(|seen| seen.contains(REAL));
                // PROVENANCE, so this test can never again mistake another actor's request for the
                // proxy's: every request the upstream received must bear the REAL credential, which
                // only the proxy holds. A placeholder-bearing hit would mean something other than the
                // relay reached the upstream, and the counts below would be counting the wrong thing.
                for seen in keys_a.lock().unwrap().iter() {
                    assert!(
                        seen.contains(REAL),
                        "status {status} (cross={cross_authority}): the upstream was reached by a \
                         request the proxy did not make, so the hit counts do not describe the \
                         relay. Saw: {seen}"
                    );
                }
                eprintln!(
                    "MATRIX status={status} cross={cross_authority} \
                     container_saw={observed} upstream_hits={a} unapproved_host_hits={b} \
                     REAL_CREDENTIAL_LEAKED={leaked}"
                );
                measured.push((status, cross_authority, observed, a, b));

                // THE CLAUSE THAT MUST NOT MOVE, asserted for all ten cases rather than the
                // interesting ones. Two separate failures, because they are separate findings: being
                // contacted at all is a policy breach, and being handed the credential is an incident.
                assert!(
                    !leaked,
                    "status {status} (cross={cross_authority}): the REAL credential reached a host it \
                     was never registered for. reqwest's cross-host scrub does not cover `x-api-key`, \
                     so the paired-upstream redirect policy is the only thing standing between a `3xx` \
                     and a leak"
                );
                assert_eq!(
                    b, 0,
                    "status {status} (cross={cross_authority}): an unapproved host was contacted, so \
                     the paired-upstream check no longer bounds where a redirect can send this request"
                );
            }
        }

        // Pinned from the measurement above.
        assert_eq!(
            measured,
            vec![
                // 301/302/303 drop the body by specification, so there is nothing to replay and the
                // paired-upstream policy still decides: same-authority is followed, cross is refused.
                (301, false, 200, 2, 0),
                (301, true, 301, 1, 0),
                (302, false, 200, 2, 0),
                (302, true, 302, 1, 0),
                (303, false, 200, 2, 0),
                (303, true, 303, 1, 0),
                // 307/308 preserve method and body, which a streamed body cannot replay. The redirect
                // is surfaced to the container unchanged and the policy is never consulted — so the
                // cross-authority row is refused by the ABSENCE of a follow, not by the check.
                (307, false, 307, 1, 0),
                (307, true, 307, 1, 0),
                (308, false, 308, 1, 0),
                (308, true, 308, 1, 0),
            ],
            "the streamed-body redirect matrix moved"
        );
    }
}
