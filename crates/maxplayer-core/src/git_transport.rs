//! Shared in-process libgit2 transport for every maxplayer relay-git leg — seller push, seller
//! base-fetch, buyer verify-fetch, and ref-advertisement probes (ls-remote / boot preflight).
//!
//! No system `git` is used on any product path. A rustls-backed smart-HTTP
//! subtransport is registered for the `https` scheme; it injects a NIP-98 `Authorization`
//! header on every request so write/read auth rides the wire regardless of the local git
//! version (git ≤ 2.53 drops the header on the streamed POST retry). TLS is reqwest/rustls;
//! `git2` is built `default-features = false` so libgit2 never links openssl or its own HTTP.
//!
//! ## Security properties (these replace the system-git scrub machinery, and are stronger)
//! - **Transport allowlist / `ext::` RCE:** every entry point calls
//!   [`assert_allowed_repo_locator`] first, and only `https` is registered — `ext:`/`file:`/`ssh:`
//!   locators are refused before any remote is constructed. Belt-and-suspenders: the helpers
//!   re-assert the allowlist internally.
//! - **Ambient-config immunity:** at first use, [`ensure_registered`] empties libgit2's
//!   global/XDG/system config search path, so NO ambient git config is consulted on any in-process
//!   leg. libgit2 applies `url.*.insteadOf` from the repository's config when it creates a remote,
//!   and [`Repository::remote_anonymous`] does NOT prevent it — only clearing the search path does.
//!   So an ambient or poisoned `$HOME`/XDG/system config can never rewrite an allowlisted `https`
//!   URL after the allowlist check (#610). Only a repo-LOCAL config is ever read.
//! - **Destination binding (repo-local config, any source):** the repo-local config of a job
//!   workdir is job-written, and libgit2 can also reach a config through `.git/commondir`. So every
//!   leg is bound to the URL the caller named, independent of repository configuration:
//!   [`bound_remote`] refuses a remote whose resolved `url()`/`pushurl()` differs from the caller's
//!   URL (libgit2 stores the `insteadOf` result there at creation, before any connection), and
//!   [`NostrHttp::action`] compares the URL libgit2 hands over for each https leg against the same
//!   intended URL ([`same_destination`]) and builds NO request on a mismatch. The header can only
//!   ever travel to the intended destination. The push side additionally opens the workdir through
//!   the layout gate ([`crate::seller_git::open_plain_workdir_repo`]).
//! - **Object-sourced push, answered by the remote's own ACK (C6):** [`push_branch_with_header`]
//!   pushes the gated commit OBJECT (`<oid>:refs/heads/<branch>`), never a local ref name a
//!   surviving job process could move under the push, and requires the remote's status report to
//!   name exactly that ref with no error. That per-ref ACK is the whole answer: silence, a report
//!   for another ref, or a rejection all fail. No advertisement is re-read after the push. It never
//!   re-resolves the local branch.
//! - **Authorization minted per WIRE REQUEST:** an operation carries an [`AuthMinter`], not a
//!   finished header. [`HttpStream::send`] calls it immediately before each request is put on the
//!   wire, handing it the repo-root URL that request is about to reach — so the advertisement leg,
//!   the pack POST and any later attempt each present their own token, signed after whatever lock
//!   or wait preceded them, and a minter can refuse a destination it did not authorize. No token
//!   lifetime is extended to cover the gap. Automatic redirects are DISABLED on both clients
//!   ([`client_default`], [`client_short`]): a redirect is a destination the minter never saw and
//!   the binding checks never ran on, so a 3xx fails the leg instead of silently forwarding the
//!   token (and, on a 307/308, the pack) to wherever it points.
//! - **Key hygiene:** the seller/buyer secret is used ONLY in-process to sign the NIP-98 event.
//!   It is never placed on argv, never in child env, and never spawns a subprocess.
//!
//! A leaked branch-scoped token is bounded authority, not zero authority: it can replay a push to
//! that one ref of that one repository until it expires. The binding above keeps it from leaving.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream, Transport};
use git2::{
    AutotagOption, ConfigLevel, Direction, FetchOptions, Oid, PushOptions, Remote, RemoteCallbacks,
    Repository,
};

use crate::delivery_transport::{assert_allowed_repo_locator, TransportRefuse};

/// Failure of an in-process git transport operation. Callers map this into their own domain
/// error (`SellerGitError` / `DeliveryError` / `String`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// Locator failed the transport allowlist (`ext:`/`file:`/`ssh:` or malformed).
    Transport(String),
    /// Auth/permission signal (401/403/unauthorized) — fail-closed, no side effect.
    Auth(String),
    /// Remote rejected a pushed ref (non-fast-forward, hook refusal, …).
    Rejected(String),
    /// Any other transport/IO failure (connect, TLS, unexpected status, resolve).
    Io(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "transport refused: {m}"),
            Self::Auth(m) => write!(f, "auth failed: {m}"),
            Self::Rejected(m) => write!(f, "remote rejected ref: {m}"),
            Self::Io(m) => write!(f, "io error: {m}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<TransportRefuse> for TransportError {
    fn from(value: TransportRefuse) -> Self {
        Self::Transport(value.to_string())
    }
}

/// Mints a NIP-98 `Authorization` header for ONE wire request, at the moment that request is made.
///
/// The argument is the repo-root URL the request is about to reach — the same `u` the token is
/// signed over — so the minter both signs freshly and gets to REFUSE a destination it did not
/// authorize. Returning `Err` fails that leg; nothing is sent unauthorized.
///
/// Why a minter and not a header: the seller delivery push waits behind this seat's one delivery
/// lock, and libgit2 builds a fresh stream for every leg and every attempt. A header minted before
/// that wait is already aging when the relay checks its `created_at`; a minter called from
/// [`HttpStream::send`] signs after the wait, once per request, without widening the token's window.
pub type AuthMinter = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

/// An [`AuthMinter`] that returns one already-minted header for every leg.
///
/// This is the READ-leg shape (fetch, ls-remote, preflight): those operations are a single short
/// exchange with no lock in front of them, so their token is minted once by [`header_for`] exactly
/// as before and this adapter hands the same bytes to each leg. The delivery push does NOT use it.
pub fn static_auth(header: String) -> AuthMinter {
    Arc::new(move |_destination: &str| Ok(header.clone()))
}

/// Asked once more, at the last instruction before a request goes out: is this operation still the
/// one allowed to transmit?
///
/// Minting is not the same instant as sending. The delivery push's minter calls the signer actor and
/// can WAIT there — for a saturated queue, for a busy actor — and a bounded wait that returns a
/// token is a bound on the wait, not permission to put it on the wire. In between, the operation's
/// owner can have timed out, been cancelled, or been dropped, and the job can have lost the delivery
/// lock to somebody else. Checking only before signing answers the question at the wrong moment.
///
/// `Err` fails the leg with nothing sent. `None` is the honest default for operations that have no
/// owner to lose: the read legs, and any public remote pushing without a token at all.
pub type AuthorityCheck = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// What the https subtransport needs for the legs of ONE operation on ONE thread. Set by
/// [`with_context`] immediately before a push/fetch/connect and cleared right after; the registered
/// https factory snapshots it into [`NostrHttp`].
#[derive(Clone)]
struct LegContext {
    /// Mints the NIP-98 `Authorization` header for each leg, or `None` for a public/anonymous https
    /// remote (no header at all).
    mint: Option<AuthMinter>,
    /// Re-asked after the mint and immediately before each request is transmitted; `None` for
    /// operations with no owner to lose. See [`AuthorityCheck`].
    authority: Option<AuthorityCheck>,
    /// When true, use the SHORT-timeout HTTP client (the buyer money-path fetch: a hung fetch must
    /// fail CLOSED before authorize_pay burns budget).
    short: bool,
    /// The exact repo-root URL the caller named. Every leg of the operation must target this URL
    /// ([`same_destination`]); a leg to any other URL is refused before a request is built.
    intended_url: String,
}

thread_local! {
    /// The context of the operation running on THIS thread, if one is running.
    static CONTEXT: RefCell<Option<LegContext>> = const { RefCell::new(None) };
    /// Set by [`NostrHttp::action`] when libgit2 hands it a URL that is not the intended one.
    /// [`with_context`] reads it after the operation, so the caller sees the binding refusal and
    /// not a generic libgit2 error.
    static BINDING_VIOLATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Per-HTTP-leg cap for the buyer money-path fetch. git2 has no whole-operation timeout, but a
/// hung leg (info/refs GET or upload-pack POST) is bounded here so the fetch fails CLOSED well
/// under the MCP tool deadline (15s) and the Claude-Code client read-timeout (~60s). A smart-HTTP
/// fetch is at most two legs, so the worst-case wall time is ~2× this — still bounded, still no pay.
const BUYER_FETCH_LEG_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-HTTP-leg request timeout for the DEFAULT (long) client [`client_default`] — the seller
/// delivery push and base-fetch, where a legitimately large pack can make a single leg run long.
/// git2 has no whole-operation timeout, so on the push path this per-leg cap is the ONLY bound the
/// transport imposes on one leg (the info/refs advertisement or the receive-pack POST). The seller
/// delivery path's whole-operation ceiling (`DELIVERY_PUSH_TIMEOUT` = 150s in `seller_node::run`)
/// MUST stay strictly above this: a `const _` assert there binds the two clocks at COMPILE time
/// (#563), so raising this toward/past the whole-op bound fails the BUILD rather than silently
/// letting one slow-but-live push leg trip the whole-op `TimedOut` arm — which would false-strand a
/// maybe-accepted delivery and mask the real `Push` error (#562). This is the LONG client; the buyer
/// money-path fetch uses the short [`BUYER_FETCH_LEG_TIMEOUT`] instead.
pub(crate) const DEFAULT_HTTP_LEG_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether to skip TLS certificate verification. Honors `GIT_SSL_NO_VERIFY` — the SAME env var
/// system `git` obeys — so nothing changes for real deployments (the var is never set; TLS is
/// verified against the bundled webpki roots), and self-signed test fixtures work exactly as they
/// did under the old system-git path. Read once when the client is first built.
fn accept_invalid_certs() -> bool {
    std::env::var_os("GIT_SSL_NO_VERIFY").is_some()
}

/// Run a blocking git2 fetch/push off any ambient async runtime, on a dedicated OS thread.
///
/// The smart-HTTP subtransport ([`HttpStream`]) uses `reqwest::blocking`. In a DEBUG build reqwest
/// guards every request by building and immediately dropping a throwaway Tokio runtime — and
/// dropping a runtime inside another runtime's context panics ("Cannot drop a runtime in a context
/// where blocking is not allowed"); a release build makes that guard a no-op, which is why the bug
/// was masked (#152). The buyer verify-fetch runs synchronously inside `authorize_pay_async` (a
/// Tokio worker), so the git2 fetch that drives those requests must run on a plain thread where no
/// ambient runtime is present. Works under any caller runtime flavor (unlike `block_in_place`).
pub(crate) fn off_runtime<T, F>(work: F) -> T
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(work)
            .join()
            .unwrap_or_else(|_| panic!("git transport worker thread panicked"))
    })
}

/// Every request this module makes is a request some check already authorized: the allowlist ran on
/// the locator, [`bound_remote`] compared the resolved remote, [`NostrHttp::action`] compared the
/// URL libgit2 handed over, and the [`AuthMinter`] was shown that same destination. A redirect the
/// HTTP client follows on its own has passed NONE of them: reqwest keeps the `Authorization` header
/// across a same-origin hop (it strips it only when host or effective port changes), and a 307/308
/// replays the body — so an unfollowed 3xx is the difference between "the relay moved the route"
/// and "the token and the pack went somewhere nobody authorized". Both clients therefore follow
/// nothing: a 3xx is surfaced to [`HttpStream::send`] as a non-success status and fails the leg.
fn no_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::none()
}

/// The same argument one layer lower, for the replay reqwest performs BY ITSELF.
///
/// The workspace enables reqwest's `http2` feature (root `Cargo.toml`), so this client offers h2 by
/// ALPN, and reqwest installs a default retry policy that treats a remote `GOAWAY(NO_ERROR)` or
/// `RST_STREAM(REFUSED_STREAM)` as a protocol nack and REPLAYS the request — cloning method, URI,
/// body AND headers. That clone happens inside `Client::execute`, below [`HttpStream::send`]: the
/// minter is never called again, so the replayed attempt carries the token minted before the wait,
/// and the pack body goes out a second time under an authorization nobody re-checked.
///
/// A peer can delay and then refuse a stream, which makes the replayed token arbitrarily older than
/// the one leg it was signed for — the exact property the minter exists to prevent. So this client
/// never replays: every attempt on the wire is one libgit2 asked for, and libgit2 builds a fresh
/// stream (and takes a fresh mint) for each of its own retries. HTTP/2 itself is untouched, here and
/// everywhere else in the workspace; only the hidden reattempt is removed.
fn no_hidden_replay() -> reqwest::retry::Builder {
    reqwest::retry::never()
}

/// Long-running client for pushes and seller base fetches (large packs are legitimate).
fn client_default() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(DEFAULT_HTTP_LEG_TIMEOUT)
            .redirect(no_redirects())
            .retry(no_hidden_replay())
            .danger_accept_invalid_certs(accept_invalid_certs())
            .build()
            .expect("build reqwest blocking client")
    })
}

/// Short-timeout client for the buyer verify fetch — fail-closed money path.
fn client_short() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .connect_timeout(BUYER_FETCH_LEG_TIMEOUT)
            .timeout(BUYER_FETCH_LEG_TIMEOUT)
            .redirect(no_redirects())
            .retry(no_hidden_replay())
            .danger_accept_invalid_certs(accept_invalid_certs())
            .build()
            .expect("build reqwest blocking client (short)")
    })
}

/// One-time libgit2 process init for this module: isolate from ambient git config, then register the
/// `https` smart subtransport. Returns whether that init succeeded so every entry point surfaces a
/// failure loudly instead of proceeding into an opaque downstream error. Runs exactly once; the
/// stored outcome is returned on every later call.
fn ensure_registered() -> Result<(), TransportError> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        // SAFETY: `git2::opts` and `git2::transport::register` mutate libgit2 GLOBAL state and must be
        // externally synchronized with transport creation / config access. `OnceLock::get_or_init`
        // guarantees a single execution, every entry point calls this BEFORE building any remote, and
        // maxplayer-core drives git2 ONLY through this module — so nothing else races or is affected.
        unsafe {
            // Isolate from ambient git config so this module's documented insteadOf-immunity actually
            // holds. libgit2 consults the global/XDG/system config on EVERY remote op (anonymous
            // remotes included) and applies `url.*.insteadOf` at CONNECT time — `remote_anonymous`
            // does NOT prevent it. Emptying the search path for these levels means no such config —
            // hence no `insteadOf` — is ever read, so an ambient or poisoned config can't rewrite an
            // allowlisted `https` URL onto another host or a banned transport after the allowlist
            // check (#610). Only a repo-LOCAL config (in workdirs we create) remains, none rewrites.
            for level in [ConfigLevel::Global, ConfigLevel::XDG, ConfigLevel::System] {
                git2::opts::set_search_path(level, "").map_err(|error| {
                    format!("isolate ambient git config ({level:?}): {}", error.message())
                })?;
            }
            git2::transport::register("https", |remote| {
                let context = CONTEXT.with(|cell| cell.borrow().clone());
                let (mint, authority, short, intended_url) = match context {
                    Some(context) => (
                        context.mint,
                        context.authority,
                        context.short,
                        Some(context.intended_url),
                    ),
                    // No operation context: no destination is bound, so `action` refuses every
                    // leg. Fail closed rather than send a request nobody named.
                    None => (None, None, false, None),
                };
                Transport::smart(
                    remote,
                    true,
                    NostrHttp {
                        mint,
                        authority,
                        short,
                        intended_url,
                    },
                )
            })
            .map_err(|error| format!("register https subtransport: {}", error.message()))?;
        }
        Ok(())
    })
    .clone()
    .map_err(TransportError::Io)
}

/// Run `body` with `context` (header, timeout class, intended destination) bound to this thread,
/// clearing it afterward so no stray auth/timeout leaks into an unrelated later operation on the
/// same thread. A destination-binding refusal recorded by [`NostrHttp::action`] during `body` wins
/// over every other outcome, an apparent success included, and surfaces as
/// [`TransportError::Transport`].
fn with_context<T>(
    context: LegContext,
    body: impl FnOnce() -> Result<T, git2::Error>,
) -> Result<T, TransportError> {
    CONTEXT.with(|cell| *cell.borrow_mut() = Some(context));
    BINDING_VIOLATION.with(|cell| *cell.borrow_mut() = None);
    let result = body();
    let violation = BINDING_VIOLATION.with(|cell| cell.borrow_mut().take());
    CONTEXT.with(|cell| *cell.borrow_mut() = None);
    match violation {
        Some(message) => Err(TransportError::Transport(message)),
        None => result.map_err(map_git_error),
    }
}

/// Split a URL into `(scheme, authority, path)` for [`same_destination`]. The scheme and the
/// authority (host, port, userinfo) compare case-insensitively; one trailing slash on the path is
/// dropped. The path — query included — must otherwise match byte for byte.
fn destination_parts(url: &str) -> Option<(String, String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let path = path.strip_suffix('/').unwrap_or(path);
    Some((
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase(),
        path.to_owned(),
    ))
}

/// Whether `actual` names the destination the caller intended. Normalizes only the scheme case, the
/// host case, and one trailing slash; the path must match exactly. Unparseable input never matches.
///
/// `pub` so a caller that builds an [`AuthMinter`] applies the SAME comparison the transport
/// applies — one rule for "is this the destination we named", never two that can drift — and so an
/// out-of-process test can build a minter with that same rule rather than a lookalike.
pub fn same_destination(intended: &str, actual: &str) -> bool {
    match (destination_parts(intended), destination_parts(actual)) {
        (Some(intended), Some(actual)) => intended == actual,
        _ => false,
    }
}

/// Create the anonymous remote for `remote_url` and require that libgit2 kept the URL as given.
///
/// libgit2 applies `url.*.insteadOf` and `url.*.pushInsteadOf` from the repository's config when it
/// creates the remote (`remote.c`, `create_internal`), so a rewrite from ANY config libgit2 read —
/// `.git/config`, a config reached through `.git/commondir`, an include — shows up here as a changed
/// `url()` or a present `pushurl()`, before any connection exists and for every scheme, not only
/// `https`. This is the first destination check; [`NostrHttp::action`] repeats it on every https leg.
fn bound_remote<'repo>(
    repo: &'repo Repository,
    remote_url: &str,
) -> Result<Remote<'repo>, TransportError> {
    let remote = repo
        .remote_anonymous(remote_url)
        .map_err(|error| TransportError::Io(format!("anonymous remote: {error}")))?;
    if remote.url() != Some(remote_url) {
        return Err(TransportError::Transport(format!(
            "repository config rewrote the remote url {remote_url:?} to {:?}; refusing the operation",
            String::from_utf8_lossy(remote.url_bytes())
        )));
    }
    if let Some(pushurl) = remote.pushurl_bytes() {
        return Err(TransportError::Transport(format!(
            "repository config set a push url {:?} for {remote_url:?}; refusing the operation",
            String::from_utf8_lossy(pushurl)
        )));
    }
    Ok(remote)
}

/// Build the NIP-98 (`kind:27235`) `Authorization` header for `remote_url`.
///
/// Signs `u = <remote_url>` (the repo-root the relay verifies after stripping `/info/refs` or the
/// service suffix) with method `POST`. maxplayer-relay is method-agnostic on git routes and does not
/// dedup the event id, so this ONE header is valid for both the info/refs GET advertisement and the
/// service POST — the same token-reuse the git-credential-nostr helper relied on, delivered directly
/// instead of via git's credential protocol. The secret never appears in the returned string.
pub fn nip98_authorization_header(
    remote_url: &str,
    secret_key_hex: &str,
) -> Result<String, TransportError> {
    let keys = nostr_sdk::Keys::parse(secret_key_hex)
        .map_err(|error| TransportError::Auth(format!("invalid key: {error}")))?;
    nip98_authorization_header_with_keys(remote_url, &keys, None, None)
}

/// Build the NIP-98 `Authorization` header from an already-held [`Keys`](nostr_sdk::Keys) instead of
/// a raw secret hex. This is the custody-preserving entry point: a caller that keeps its secret
/// inside a signer actor signs the header THROUGH the actor (which owns the `Keys`) so the secret is
/// never re-read into a third site. Identical header to [`nip98_authorization_header`] when
/// `ref_scope` is `None`.
///
/// `ref_scope`: when `Some(refname)`, add one `["ref", "<refname>"]` tag. The relay (PR #929) reads
/// the first `ref` tag and refuses a push to any other ref, so a stolen token minted for the
/// delivery branch is bounded authority: it can replay a push to that one ref of that one
/// repository until it expires, and nothing else. `refname` must be fully qualified
/// (`refs/heads/…`); the relay rejects a bare branch name. `None` mints the unscoped header,
/// byte-identical to before.
///
/// `expiration_unix`: when `Some(ts)`, add a NIP-40 `["expiration", "<ts>"]` tag. This is the
/// long-lived-token seam (Task B8): the relay's Requirement B (see the relay brief) accepts a SCOPED
/// token up to its expiry, so ONE token can cover a full container job instead of the ±60 s default.
/// Only meaningful together with `ref_scope` — an unscoped token keeps the ±60 s window regardless.
/// `None` adds no expiration tag (today's behaviour). INERT until the relay honours it.
pub fn nip98_authorization_header_with_keys(
    remote_url: &str,
    keys: &nostr_sdk::Keys,
    ref_scope: Option<&str>,
    expiration_unix: Option<i64>,
) -> Result<String, TransportError> {
    use base64::Engine as _;
    use nostr_sdk::nips::nip98::{HttpData, HttpMethod};
    use nostr_sdk::prelude::{EventBuilder, Tag, Url};
    use nostr_sdk::JsonUtil;

    let url = Url::parse(remote_url)
        .map_err(|error| TransportError::Io(format!("invalid remote url: {error}")))?;
    let mut builder = EventBuilder::http_auth(HttpData::new(url, HttpMethod::POST));
    if let Some(refname) = ref_scope {
        let tag = Tag::parse(["ref", refname])
            .map_err(|error| TransportError::Auth(format!("invalid ref scope tag: {error}")))?;
        builder = builder.tag(tag);
    }
    if let Some(expiry) = expiration_unix {
        let tag = Tag::parse(["expiration", &expiry.to_string()])
            .map_err(|error| TransportError::Auth(format!("invalid expiration tag: {error}")))?;
        builder = builder.tag(tag);
    }
    let event = builder
        .sign_with_keys(keys)
        .map_err(|error| TransportError::Auth(format!("nip98 sign failed: {error}")))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(event.as_json());
    Ok(format!("Nostr {encoded}"))
}

/// The fully-qualified ref a delivery push writes for `branch`. Both the push refspec
/// ([`push_branch_with_header`]) and the branch-scoped token scope (the caller in `run.rs`) derive
/// the ref from THIS one function, so a future edit cannot split the token scope from the ref
/// actually pushed — the relay demands they match exactly (PR #929).
pub fn delivery_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

/// Resolve the NIP-98 header for a leg: `Some` header only when a key is supplied AND the remote is
/// relay-git (which auth-gates reads and writes); public/anonymous https gets `None` (no header).
fn header_for(remote_url: &str, auth: Option<&str>) -> Result<Option<String>, TransportError> {
    match auth {
        Some(secret) if crate::delivery_transport::is_relay_git_locator(remote_url) => {
            Ok(Some(nip98_authorization_header(remote_url, secret)?))
        }
        _ => Ok(None),
    }
}

/// Push the gated commit `gated_oid` to `refs/heads/<branch>` at `remote_url` in-process, returning
/// the attested oid (full hex, equal to `gated_oid`). `auth` is the seller secret hex (NIP-98 for
/// relay-git; `None`/public https pushes unauthenticated and fail closed at the remote).
pub fn push_branch(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    gated_oid: &str,
    auth: Option<&str>,
) -> Result<String, TransportError> {
    let header = header_for(remote_url, auth)?;
    push_branch_with_header(workdir, remote_url, branch, gated_oid, header)
}

/// Like [`push_branch`] but takes an already-resolved NIP-98 `Authorization` header instead of the
/// raw secret. A custody-preserving caller builds the header through its signer actor
/// ([`nip98_authorization_header_with_keys`]) and passes it here, so the secret never reaches this
/// layer. `None` = no auth (public/anonymous https). The header is bound to the repo-root URL and
/// reused for both the info/refs advertisement and the service POST, exactly as `push_branch` does.
///
/// `gated_oid` is the commit the gate produced. It is the push SOURCE: the refspec is
/// `<gated_oid>:refs/heads/<branch>`, so the bytes on the wire are that object's graph no matter
/// where the local branch points during the push (C6). After the push:
/// 1. the remote's status report must name exactly `refs/heads/<branch>` with no error message;
/// 2. that ACK is the whole answer — no advertisement is read back afterwards. A remote that
///    reports nothing for our ref has not said yes, and is refused on the spot.
///
/// The local branch is never re-resolved. The workdir is opened through the layout gate
/// ([`crate::seller_git::open_plain_workdir_repo`]) and every leg is bound to `remote_url`.
///
/// `header` is minted ONCE by the caller and presented on every leg. The delivery push wants the
/// opposite — see [`push_branch_with_minter`], which this delegates to.
pub fn push_branch_with_header(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    gated_oid: &str,
    header: Option<String>,
) -> Result<String, TransportError> {
    push_branch_with_minter(
        workdir,
        remote_url,
        branch,
        gated_oid,
        header.map(static_auth),
        None,
    )
}

/// [`push_branch_with_header`] with the authorization minted PER WIRE REQUEST instead of once up
/// front: `mint` is called from [`HttpStream::send`] immediately before each request, with the
/// repo-root URL that request is about to reach.
///
/// This is the delivery-push entry point. The push waits behind the seat's one delivery lock and
/// libgit2 opens a fresh stream per leg, so the advertisement and the pack POST each sign their own
/// token after that wait, and the minter sees — and may refuse — the destination each one names.
/// `None` is the public/anonymous https case: no header on any leg.
///
/// `authority` is asked again after that mint and before the request is transmitted (see
/// [`AuthorityCheck`]): minting can WAIT on a busy signer, and the caller that owned this push may
/// be gone by the time the token is in hand.
pub fn push_branch_with_minter(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    gated_oid: &str,
    mint: Option<AuthMinter>,
    authority: Option<AuthorityCheck>,
) -> Result<String, TransportError> {
    assert_allowed_repo_locator(remote_url)?;
    ensure_registered()?;
    let repo = open_delivery_repo(workdir)?;
    push_gated_object(&repo, remote_url, branch, gated_oid, mint, authority)
}

/// Open the committed workdir a delivery is pushed from, through the layout gate. A layout refusal
/// is a [`TransportError::Transport`]: fail closed, never retried, no network.
fn open_delivery_repo(workdir: &Path) -> Result<Repository, TransportError> {
    use crate::seller_git::SellerGitError;
    crate::seller_git::open_plain_workdir_repo(workdir).map_err(|error| match error {
        SellerGitError::Layout(message) => TransportError::Transport(message),
        other => TransportError::Io(format!("open workdir repo: {other}")),
    })
}

/// Parse a full 40-hex commit oid and require that the commit exists in `repo`.
fn gated_commit(repo: &Repository, gated_oid: &str) -> Result<Oid, TransportError> {
    let is_full_hex = gated_oid.len() == 40 && gated_oid.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_full_hex {
        return Err(TransportError::Io(format!(
            "gated oid {gated_oid:?} is not a full commit oid"
        )));
    }
    let oid = Oid::from_str(gated_oid)
        .map_err(|error| TransportError::Io(format!("gated oid {gated_oid:?}: {error}")))?;
    repo.find_commit(oid).map_err(|error| {
        TransportError::Io(format!(
            "gated commit {gated_oid} is not a commit in the workdir: {error}"
        ))
    })?;
    Ok(oid)
}

/// The push proper, on an already-opened repository: bind the remote, push the OBJECT `gated_oid`
/// to `refs/heads/<branch>`, and require the remote's own status report to ACK exactly that ref.
/// Split from [`push_branch_with_header`] so the object-sourced push can be exercised against a
/// local bare repository, which the transport allowlist refuses on the public entry point.
fn push_gated_object(
    repo: &Repository,
    remote_url: &str,
    branch: &str,
    gated_oid: &str,
    mint: Option<AuthMinter>,
    authority: Option<AuthorityCheck>,
) -> Result<String, TransportError> {
    let gated = gated_commit(repo, gated_oid)?.to_string();
    let target_ref = delivery_ref(branch);
    let mut remote = bound_remote(repo, remote_url)?;

    // The source is the object, not `refs/heads/<branch>`: libgit2 resolves a push source with
    // revparse (`push.c`, `check_lref` / `calculate_work`), and a full hex oid resolves to the object
    // itself before any ref is consulted (`revparse.c`, `maybe_sha`). A local ref a survivor moves
    // during the push changes nothing about what is sent.
    let refspec = format!("{gated}:{target_ref}");
    let reports: std::rc::Rc<RefCell<Vec<(String, Option<String>)>>> =
        std::rc::Rc::new(RefCell::new(Vec::new()));
    let mut callbacks = RemoteCallbacks::new();
    {
        let reports = reports.clone();
        callbacks.push_update_reference(move |refname, status| {
            reports
                .borrow_mut()
                .push((refname.to_owned(), status.map(str::to_owned)));
            Ok(())
        });
    }
    let mut options = PushOptions::new();
    options.remote_callbacks(callbacks);

    let context = LegContext {
        mint,
        authority,
        short: false,
        intended_url: remote_url.to_owned(),
    };
    with_context(context, || {
        remote.push(&[refspec.as_str()], Some(&mut options))
    })?;
    drop(options);
    // The remote's per-ref ACK is the whole answer. Reading the advertisement back afterwards added
    // no authority the ACK does not already carry — it is the same server answering the same
    // question a second time — while costing a second authorized connection to the delivery remote
    // after the bytes had already landed. What makes the ACK sufficient is that silence is refused:
    // `require_status_report` fails on no status, on a status for another ref, and on a rejection.
    require_status_report(&reports.borrow(), &target_ref)?;
    Ok(gated)
}

/// Require that the remote's status report names exactly `target_ref`, with no error message. The
/// smart protocol (`report-status`) and the local transport both report one status per refspec sent.
/// A message is a rejection (retryable: the relay's per-repo push lock surfaces this way). A report
/// for another ref, or no report at all, means the push did not do what was asked: refuse, no retry.
fn require_status_report(
    reports: &[(String, Option<String>)],
    target_ref: &str,
) -> Result<(), TransportError> {
    if let Some((refname, Some(message))) = reports.iter().find(|(_, status)| status.is_some()) {
        return Err(TransportError::Rejected(format!("{refname}: {message}")));
    }
    match reports {
        [(refname, None)] if refname == target_ref => Ok(()),
        [] => Err(TransportError::Transport(format!(
            "remote reported no status for {target_ref}; refusing to trust the push"
        ))),
        other => {
            let names: Vec<&str> = other.iter().map(|(name, _)| name.as_str()).collect();
            Err(TransportError::Transport(format!(
                "remote reported status for {names:?}; expected exactly {target_ref}"
            )))
        }
    }
}

/// Fetch `refspecs` from `remote_url` into `repo` in-process. `auth` supplies NIP-98 for relay-git
/// reads; `short_timeout` selects the fail-closed money-path client (buyer verify) vs the default
/// long client (seller base fetch). Tags are never downloaded (mirrors `--no-tags`).
///
/// The transport allowlist is NOT asserted here — fetch has legitimate LOCAL-path callers (the
/// buyer's store→working-clone merge, and test fixtures fetch from `file`/local bare repos). The
/// allowlist is enforced at the caller's seam (`PayPathDeliveryVerifier` for the money path;
/// `init_contribution_workdir` for the seller base). A local path routes through libgit2's built-in
/// local transport (no header); only allowlisted `https` reaches the NIP-98 subtransport.
pub fn fetch_refspecs(
    repo: &Repository,
    remote_url: &str,
    refspecs: &[&str],
    auth: Option<&str>,
    short_timeout: bool,
) -> Result<(), TransportError> {
    ensure_registered()?;
    let header = header_for(remote_url, auth)?;

    let mut remote = bound_remote(repo, remote_url)?;
    let mut options = FetchOptions::new();
    options.download_tags(AutotagOption::None);

    let context = LegContext {
        mint: header.map(static_auth),
        // A read leg owns nothing another job can take: no delivery lock, no push authority. There
        // is no owner to lose, so there is nothing to re-check.
        authority: None,
        short: short_timeout,
        intended_url: remote_url.to_owned(),
    };
    let result = with_context(context, || {
        remote.fetch(refspecs, Some(&mut options), None)
    });
    drop(options);
    result
}

/// Connect to `remote_url` in `direction` and return the advertised refs WITHOUT transferring a
/// pack. Used by the boot push-preflight (`Direction::Push` = receive-pack advertisement, the
/// auth-gated leg) and the relay-git seed probe (`Direction::Fetch` = upload-pack, ls-remote).
pub fn list_remote(
    remote_url: &str,
    auth: Option<&str>,
    direction: Direction,
) -> Result<Vec<(String, String)>, TransportError> {
    assert_allowed_repo_locator(remote_url)?;
    ensure_registered()?;
    let header = header_for(remote_url, auth)?;

    // A bare in-memory repo is enough to host an anonymous remote for a connect+list.
    let repo = Repository::open_from_env()
        .or_else(|_| {
            let tmp = std::env::temp_dir().join(format!(
                "maxplayer-lsremote-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            Repository::init_bare(tmp)
        })
        .map_err(|error| TransportError::Io(format!("scratch repo: {error}")))?;
    let mut remote = bound_remote(&repo, remote_url)?;

    let context = LegContext {
        mint: header.map(static_auth),
        authority: None,
        short: false,
        intended_url: remote_url.to_owned(),
    };
    let heads = with_context(context, || {
        remote.connect(direction)?;
        let list = remote
            .list()?
            .iter()
            .map(|h| (h.name().to_owned(), h.oid().to_string()))
            .collect::<Vec<_>>();
        let _ = remote.disconnect();
        Ok::<_, git2::Error>(list)
    })?;
    Ok(heads)
}

/// `ls-remote` over the upload-pack advertisement: list the remote's refs without transferring a
/// pack. Thin wrapper over [`list_remote`] in the fetch direction so callers outside this crate need
/// not name `git2::Direction`. Used by the seller's post-announce relay-git seed probe.
pub fn ls_remote(
    remote_url: &str,
    auth: Option<&str>,
) -> Result<Vec<(String, String)>, TransportError> {
    list_remote(remote_url, auth, Direction::Fetch)
}

/// Map a libgit2 error to a scrubbed [`TransportError`]. Auth/permission signals map to
/// `Auth` (fail-closed); everything else to `Io`. The secret is never in a git2 error.
fn map_git_error(error: git2::Error) -> TransportError {
    let lowered = error.message().to_ascii_lowercase();
    if lowered.contains("401")
        || lowered.contains("403")
        || lowered.contains("authentication")
        || lowered.contains("unauthorized")
        || lowered.contains("forbidden")
        || lowered.contains("permission")
        || lowered.contains("could not read username")
        || lowered.contains("repository not found")
        || lowered.contains("404")
    {
        TransportError::Auth(error.message().to_owned())
    } else {
        TransportError::Io(error.message().to_owned())
    }
}

/// rustls smart-HTTP subtransport that injects the NIP-98 header captured at construction time
/// and uses the short- or long-timeout client per the operation's timeout class. Every leg is bound
/// to `intended_url`: [`Self::action`] refuses any other URL before a request exists.
struct NostrHttp {
    mint: Option<AuthMinter>,
    authority: Option<AuthorityCheck>,
    short: bool,
    /// The repo-root URL the caller named, from the operation context. `None` when the transport was
    /// created outside any [`with_context`]; then every leg is refused.
    intended_url: Option<String>,
}

/// Map a smart-HTTP service to its `(service_name, is_post)` pair.
fn service_parts(service: Service) -> (&'static str, bool) {
    match service {
        Service::UploadPackLs => ("git-upload-pack", false),
        Service::UploadPack => ("git-upload-pack", true),
        Service::ReceivePackLs => ("git-receive-pack", false),
        Service::ReceivePack => ("git-receive-pack", true),
    }
}

/// Build the request URL for a service leg. POST legs hit `<base>/<service>`; the
/// ref-advertisement (LS) legs hit `<base>/info/refs?service=<service>` — matching libgit2's
/// built-in smart-HTTP transport (and what the relay strips back to the repo root).
fn service_url(base: &str, name: &str, is_post: bool) -> String {
    let base = base.trim_end_matches('/');
    if is_post {
        format!("{base}/{name}")
    } else {
        format!("{base}/info/refs?service={name}")
    }
}

impl SmartSubtransport for NostrHttp {
    /// One leg. `url` is the repo-root URL libgit2 resolved for the remote — after any rewrite the
    /// repository configuration applied. It must be the destination the caller named; otherwise no
    /// request is built and the refusal is recorded for [`with_context`]. The header never travels
    /// anywhere but the intended URL, whatever the configuration says.
    fn action(
        &self,
        url: &str,
        service: Service,
    ) -> Result<Box<dyn SmartSubtransportStream>, git2::Error> {
        let bound = self
            .intended_url
            .as_deref()
            .is_some_and(|intended| same_destination(intended, url));
        if !bound {
            let message = format!(
                "destination binding refused a {} leg to {url:?}: the operation is bound to {:?}",
                service_parts(service).0,
                self.intended_url.as_deref().unwrap_or("<no destination>")
            );
            BINDING_VIOLATION.with(|cell| *cell.borrow_mut() = Some(message.clone()));
            return Err(git2::Error::new(
                git2::ErrorCode::Auth,
                git2::ErrorClass::Net,
                message,
            ));
        }
        let (name, is_post) = service_parts(service);
        let full_url = service_url(url, name, is_post);
        Ok(Box::new(HttpStream {
            mint: self.mint.clone(),
            authority: self.authority.clone(),
            short: self.short,
            url: full_url,
            // The repo ROOT this leg belongs to, kept beside the service URL: it is what the token
            // is signed over (`u`) and what the minter is shown, so the minter judges the same
            // destination `action` just bound rather than a service path derived from it.
            destination: url.to_owned(),
            service: name,
            is_post,
            sent: false,
            request_body: Vec::new(),
            response: None,
        }))
    }

    fn close(&self) -> Result<(), git2::Error> {
        Ok(())
    }
}

/// One request/response leg of the smart-HTTP flow. libgit2 writes the request body (POST legs),
/// then reads the response; we buffer the writes and fire the HTTP request lazily on the first read
/// (the standard buffer-then-send pattern for stateless smart HTTP).
struct HttpStream {
    mint: Option<AuthMinter>,
    authority: Option<AuthorityCheck>,
    short: bool,
    url: String,
    destination: String,
    service: &'static str,
    is_post: bool,
    sent: bool,
    request_body: Vec<u8>,
    response: Option<reqwest::blocking::Response>,
}

impl HttpStream {
    fn send(&mut self) -> io::Result<()> {
        let client = if self.short {
            client_short()
        } else {
            client_default()
        };
        let mut request = if self.is_post {
            client
                .post(&self.url)
                .header(
                    "Content-Type",
                    format!("application/x-{}-request", self.service),
                )
                .header("Accept", format!("application/x-{}-result", self.service))
                .body(std::mem::take(&mut self.request_body))
        } else {
            client.get(&self.url).header("Accept", "*/*")
        };
        // identity encoding: never hand libgit2 a gzip stream it did not negotiate.
        request = request.header("Accept-Encoding", "identity");
        // Mint HERE, not when the operation was set up: this is the last instruction before the
        // request leaves, so whatever the operation waited on — the delivery lock, an earlier leg,
        // a previous attempt — is already behind us and the token's window starts now. libgit2
        // builds a fresh stream per leg and per attempt, so every request on the wire gets its own.
        // A minter that refuses this destination fails the leg with nothing sent.
        if let Some(mint) = &self.mint {
            let header = mint(&self.destination).map_err(|error| {
                io::Error::other(format!(
                    "authorize {} leg to {}: {error}",
                    self.service, self.destination
                ))
            })?;
            request = request.header("Authorization", header);
        }
        // Minting may have WAITED — the delivery push signs inside an actor with a queue. A token in
        // hand is not permission to transmit: while we were waiting, this operation's owner may have
        // timed out, been cancelled or been dropped, and the delivery lock may already belong to the
        // next job. Ask once more here, with nothing between this answer and the wire.
        if let Some(authority) = &self.authority {
            authority().map_err(|error| {
                io::Error::other(format!(
                    "refusing to send {} leg to {}: {error}",
                    self.service, self.destination
                ))
            })?;
        }
        let response = request
            .send()
            .map_err(|error| io::Error::other(format!("http request: {error}")))?;
        let status = response.status();
        if status.is_redirection() {
            // Unfollowed by construction (`no_redirects`). Name it plainly: the hop was never
            // checked by the allowlist, `bound_remote` or `action`, and the minter never saw it.
            return Err(io::Error::other(format!(
                "http status {} for {}: redirects are refused; the destination this operation \
                 authorized is {}",
                status.as_u16(),
                self.url,
                self.destination
            )));
        }
        if !status.is_success() {
            return Err(io::Error::other(format!(
                "http status {} for {}",
                status.as_u16(),
                self.url
            )));
        }
        self.response = Some(response);
        Ok(())
    }
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.sent {
            self.send()?;
            self.sent = true;
        }
        match self.response.as_mut() {
            Some(response) => response.read(buf),
            None => Ok(0),
        }
    }
}

impl Write for HttpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.request_body.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_legs_hit_info_refs_post_legs_hit_service() {
        let base = "https://relay.example/git/owner/repo.git";
        let (name, is_post) = service_parts(Service::ReceivePackLs);
        assert_eq!(name, "git-receive-pack");
        assert!(!is_post);
        assert_eq!(
            service_url(base, name, is_post),
            "https://relay.example/git/owner/repo.git/info/refs?service=git-receive-pack"
        );

        let (name, is_post) = service_parts(Service::ReceivePack);
        assert!(is_post);
        assert_eq!(
            service_url(base, name, is_post),
            "https://relay.example/git/owner/repo.git/git-receive-pack"
        );
    }

    #[test]
    fn upload_pack_ls_hits_info_refs() {
        let (name, is_post) = service_parts(Service::UploadPackLs);
        assert_eq!(name, "git-upload-pack");
        assert!(!is_post);
        assert_eq!(
            service_url("https://h/git/o/r", name, is_post),
            "https://h/git/o/r/info/refs?service=git-upload-pack"
        );
    }

    #[test]
    fn service_url_trims_one_trailing_slash_only() {
        assert_eq!(
            service_url("https://h/git/o/r/", "git-receive-pack", true),
            "https://h/git/o/r/git-receive-pack"
        );
    }

    #[test]
    fn header_none_for_public_https() {
        // No key ⇒ no header regardless of locator.
        assert_eq!(
            header_for("https://example.invalid/repo.git", None).unwrap(),
            None
        );
    }

    #[test]
    fn nip98_header_binds_repo_root_and_verifies() {
        use base64::Engine as _;
        use nostr_sdk::{Event, JsonUtil, Keys};

        let keys = Keys::generate();
        let secret = keys.secret_key().to_secret_hex();
        let remote = "https://relay.example/git/abcdef/repo.git";
        let header = nip98_authorization_header(remote, &secret).expect("build header");

        // Never leaks the secret; scheme is "Nostr <base64>".
        assert!(!header.contains(&secret), "secret leaked in header");
        let encoded = header.strip_prefix("Nostr ").expect("Nostr scheme");
        let json = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64");
        let event = Event::from_json(&json).expect("event json");
        event.verify().expect("valid signature");
        assert_eq!(event.kind.as_u16(), 27235, "NIP-98 kind");

        let u = event
            .tags
            .iter()
            .find(|t| t.kind() == nostr_sdk::TagKind::custom("u"))
            .and_then(|t| t.content().map(str::to_owned))
            .expect("u tag");
        assert_eq!(u, remote, "u tag binds the repo-root the relay verifies");
        let method = event
            .tags
            .iter()
            .find(|t| t.kind() == nostr_sdk::TagKind::custom("method"))
            .and_then(|t| t.content().map(str::to_owned))
            .expect("method tag");
        assert_eq!(method, "POST");
    }

    #[test]
    fn nip98_header_rejects_bad_key() {
        let err = nip98_authorization_header("https://relay.example/git/o/r.git", "not-a-key")
            .expect_err("must reject");
        assert!(matches!(err, TransportError::Auth(_)));
    }

    // Decode a "Nostr <base64>" header back to its NIP-98 event.
    fn decode_nip98(header: &str) -> nostr_sdk::Event {
        use base64::Engine as _;
        use nostr_sdk::{Event, JsonUtil};
        let encoded = header.strip_prefix("Nostr ").expect("Nostr scheme");
        let json = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64");
        Event::from_json(&json).expect("event json")
    }

    fn ref_tags(event: &nostr_sdk::Event) -> Vec<String> {
        event
            .tags
            .iter()
            .filter(|t| t.kind() == nostr_sdk::TagKind::custom("ref"))
            .filter_map(|t| t.content().map(str::to_owned))
            .collect()
    }

    #[test]
    fn nip98_header_scoped_carries_one_ref_tag_and_verifies() {
        use nostr_sdk::Keys;

        let keys = Keys::generate();
        let remote = "https://relay.example/git/abcdef/repo.git";
        let scope = "refs/heads/maxplayer/abc12345";
        let header = nip98_authorization_header_with_keys(remote, &keys, Some(scope), None)
            .expect("build header");

        let event = decode_nip98(&header);
        // Still a valid NIP-98 token: signature, kind, and the u/method binding are unchanged.
        event.verify().expect("valid signature");
        assert_eq!(event.kind.as_u16(), 27235, "NIP-98 kind");
        let u = event
            .tags
            .iter()
            .find(|t| t.kind() == nostr_sdk::TagKind::custom("u"))
            .and_then(|t| t.content().map(str::to_owned))
            .expect("u tag");
        assert_eq!(u, remote, "u tag still binds the repo-root");

        // Exactly one ref tag, carrying the exact scope. The relay reads the first ref tag; emitting
        // more than one would be ambiguous.
        assert_eq!(ref_tags(&event), vec![scope.to_owned()], "one exact ref tag");
    }

    #[test]
    fn nip98_header_unscoped_has_no_ref_tag() {
        use nostr_sdk::Keys;

        let keys = Keys::generate();
        let remote = "https://relay.example/git/abcdef/repo.git";
        let header =
            nip98_authorization_header_with_keys(remote, &keys, None, None).expect("build header");

        // Backward-compat guard: with no scope the token carries NO ref tag, so an old relay sees
        // exactly today's event.
        assert!(ref_tags(&decode_nip98(&header)).is_empty(), "no ref tag when unscoped");
    }

    #[test]
    fn delivery_ref_single_sources_scope_and_push() {
        use nostr_sdk::Keys;

        // The push refspec (push_branch_with_header) and the token scope (run.rs) BOTH call
        // delivery_ref on the same branch. This test pins delivery_ref's output and proves the
        // minted token carries exactly that value — so the two cannot drift apart.
        let branch = "maxplayer/abc12345";
        let push_ref = delivery_ref(branch);
        assert_eq!(push_ref, "refs/heads/maxplayer/abc12345", "fully-qualified ref");

        let keys = Keys::generate();
        let header =
            nip98_authorization_header_with_keys("https://relay.example/git/o/r.git", &keys, Some(&push_ref), None)
                .expect("build header");
        assert_eq!(
            ref_tags(&decode_nip98(&header)),
            vec![push_ref],
            "the token scope equals the ref the push uses"
        );
    }

    fn expiration_tags(event: &nostr_sdk::Event) -> Vec<String> {
        event
            .tags
            .iter()
            .filter(|t| t.kind() == nostr_sdk::TagKind::custom("expiration"))
            .filter_map(|t| t.content().map(str::to_owned))
            .collect()
    }

    #[test]
    fn nip98_header_carries_the_expiration_tag_only_when_set() {
        use nostr_sdk::Keys;
        let keys = Keys::generate();
        let remote = "https://relay.example/git/o/r.git";
        let scope = "refs/heads/maxplayer/abc12345";

        // The long-lived delivery-token shape (Task B8): scoped + a NIP-40 expiration.
        let long =
            nip98_authorization_header_with_keys(remote, &keys, Some(scope), Some(1_700_000_060))
                .expect("build header");
        let event = decode_nip98(&long);
        event.verify().expect("valid signature");
        assert_eq!(
            expiration_tags(&event),
            vec!["1700000060".to_owned()],
            "one expiration tag with the exact unix ts"
        );
        assert_eq!(ref_tags(&event), vec![scope.to_owned()], "scope tag unchanged");

        // None ⇒ no expiration tag (today's short-lived token). Backward-compat guard.
        let short = nip98_authorization_header_with_keys(remote, &keys, Some(scope), None)
            .expect("build header");
        assert!(
            expiration_tags(&decode_nip98(&short)).is_empty(),
            "no expiration tag when unset"
        );
    }

    #[test]
    fn allowlist_refused_before_any_network() {
        assert!(matches!(
            push_branch(
                std::path::Path::new("/nonexistent"),
                "ext::sh -c evil",
                "main",
                &"a".repeat(40),
                None
            ),
            Err(TransportError::Transport(_))
        ));
    }

    // #152 regression: a `reqwest::blocking` REQUEST executed on a Tokio worker hits reqwest's
    // debug-only guard, which builds and drops a throwaway runtime — a debug panic ("Cannot drop a
    // runtime in a context where blocking is not allowed"). The buyer verify-fetch runs inside
    // `authorize_pay_async`, so it must go through `off_runtime` (a plain thread). This drives a real
    // blocking request from within a Tokio runtime via `off_runtime` and asserts it returns (the
    // request fails — nothing listens on port 9 — but must NOT panic).
    //
    // Red-on-revert (strong form): call `.send()` DIRECTLY here (drop the `off_runtime` wrapper) and
    // this test panics in a debug build.
    #[tokio::test]
    async fn blocking_request_runs_off_the_async_runtime() {
        let result = off_runtime(|| {
            reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_millis(100))
                .build()
                .expect("client")
                .get("http://127.0.0.1:9/")
                .send()
        });
        assert!(result.is_err(), "the request should fail to connect, but must not panic");
    }

    // ── F1 layer (b): destination binding ────────────────────────────────────────────────────

    #[test]
    fn same_destination_normalizes_only_case_and_one_trailing_slash() {
        let intended = "https://relay.example/git/o/r.git";
        assert!(same_destination(intended, "https://relay.example/git/o/r.git"));
        assert!(same_destination(intended, "HTTPS://Relay.Example/git/o/r.git"));
        assert!(same_destination(intended, "https://relay.example/git/o/r.git/"));
        assert!(same_destination("https://relay.example/git/o/r.git/", intended));
        // The path is exact: case, a second slash, another repo, a query.
        assert!(!same_destination(intended, "https://relay.example/git/o/R.git"));
        assert!(!same_destination(intended, "https://relay.example/git/o/r.git//"));
        assert!(!same_destination(intended, "https://relay.example/git/o/other.git"));
        assert!(!same_destination(intended, "https://relay.example/git/o/r.git?x=1"));
        // Host, port, scheme and userinfo all count.
        assert!(!same_destination(intended, "https://evil.example/git/o/r.git"));
        assert!(!same_destination(intended, "https://relay.example:8443/git/o/r.git"));
        assert!(!same_destination(intended, "http://relay.example/git/o/r.git"));
        assert!(!same_destination(intended, "https://relay.example@evil.example/git/o/r.git"));
        // Unparseable input never matches, not even itself.
        assert!(!same_destination(intended, "relay.example/git/o/r.git"));
        assert!(!same_destination("", ""));
        assert!(!same_destination("https://", "https://"));
    }

    // The https subtransport builds NO request for a leg whose URL is not the bound destination.
    // Red-on-revert: remove the check at the top of `NostrHttp::action` and the leg to the other
    // host is accepted.
    #[test]
    fn action_refuses_a_leg_to_any_other_destination() {
        let intended = "https://relay.example/git/o/r.git";
        let transport = NostrHttp {
            mint: Some(static_auth("Nostr token".to_owned())),
            authority: None,
            short: false,
            intended_url: Some(intended.to_owned()),
        };
        assert!(
            transport.action(intended, Service::ReceivePackLs).is_ok(),
            "the intended URL is accepted"
        );
        assert!(
            transport
                .action("https://relay.example/git/o/r.git/", Service::ReceivePack)
                .is_ok(),
            "one trailing slash is the same destination"
        );
        let err = match transport.action("https://127.0.0.1:1/git/o/r.git", Service::ReceivePackLs) {
            Err(err) => err,
            Ok(_) => panic!("another host must be refused"),
        };
        assert!(
            err.message().contains("destination binding refused"),
            "{}",
            err.message()
        );
        assert!(
            transport
                .action("https://relay.example/git/o/other.git", Service::ReceivePack)
                .is_err(),
            "another path on the same host is refused"
        );
        // A transport created outside any operation context has no destination: nothing passes.
        let unbound = NostrHttp {
            mint: Some(static_auth("Nostr token".to_owned())),
            authority: None,
            short: false,
            intended_url: None,
        };
        assert!(unbound.action(intended, Service::ReceivePackLs).is_err());
        // The refusals above recorded a violation on this thread; clear it.
        BINDING_VIOLATION.with(|cell| *cell.borrow_mut() = None);
    }

    // ── Fixtures for the object-sourced push and the layout gate ─────────────────────────────

    fn temp_root(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "maxplayer-git-transport-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir root");
        root
    }

    // Commit `content` at `name` on top of `parent`; returns the commit oid. No ref is updated.
    fn commit_file(repo: &Repository, name: &str, content: &str, parent: Option<Oid>) -> Oid {
        let workdir = repo.workdir().expect("workdir");
        std::fs::write(workdir.join(name), content).expect("write file");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new(name)).expect("add");
        index.write().expect("write index");
        let tree = repo
            .find_tree(index.write_tree().expect("tree"))
            .expect("find tree");
        let sig = git2::Signature::new(
            "t",
            "t@example.invalid",
            &git2::Time::new(1_700_000_000, 0),
        )
        .expect("sig");
        let parents: Vec<git2::Commit<'_>> = parent
            .map(|p| repo.find_commit(p).expect("parent"))
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        repo.commit(None, &sig, &sig, name, &tree, &parent_refs)
            .expect("commit")
    }

    // A workdir with commits A (the gated one, a root) and B (a child of A). `refs/heads/job`
    // points at B: a survivor moved the branch off the gated commit.
    fn workdir_with_moved_branch(root: &Path) -> (std::path::PathBuf, Oid, Oid) {
        let workdir = root.join("workdir");
        let repo = Repository::init(&workdir).expect("init");
        let a = commit_file(&repo, "a.txt", "gated\n", None);
        let b = commit_file(&repo, "b.txt", "moved\n", Some(a));
        repo.reference("refs/heads/job", b, true, "branch at B")
            .expect("branch");
        (workdir, a, b)
    }

    // ── C6: the push sends the gated OBJECT ──────────────────────────────────────────────────

    // The local branch points at B; the push is told to deliver A. The remote must end at A and the
    // returned oid must be A. This also verifies empirically that libgit2 accepts a raw hex oid as
    // the push source (`push.c` resolves it with revparse).
    // Red-on-revert: build the refspec from `refs/heads/job` instead of the oid and the remote gets B.
    #[test]
    fn push_sends_the_gated_object_not_the_branch_tip() {
        let root = temp_root("c6-object");
        let (workdir, a, b) = workdir_with_moved_branch(&root);
        let bare = root.join("remote.git");
        Repository::init_bare(&bare).expect("bare remote");
        let remote_url = bare.to_str().expect("utf8").to_owned();

        let repo = crate::seller_git::open_plain_workdir_repo(&workdir).expect("open workdir");
        let pushed = push_gated_object(&repo, &remote_url, "job", &a.to_string(), None, None)
            .expect("push the gated object");
        assert_eq!(pushed, a.to_string(), "the returned oid is the gated one");

        let remote_repo = Repository::open_bare(&bare).expect("open bare");
        let remote_tip = remote_repo
            .refname_to_id("refs/heads/job")
            .expect("remote ref");
        assert_eq!(remote_tip, a, "the remote got the gated object A, not the branch tip B");
        assert_ne!(remote_tip, b);
        // The local branch was neither consulted nor touched.
        assert_eq!(repo.refname_to_id("refs/heads/job").expect("local ref"), b);

        // A repeat push of the same object (the resume path) is accepted and ACKed again.
        let again = push_gated_object(&repo, &remote_url, "job", &a.to_string(), None, None)
            .expect("re-push the gated object");
        assert_eq!(again, a.to_string());
        let _ = std::fs::remove_dir_all(&root);
    }

    // The push refuses a gated oid that is not a full hex oid or not a commit in the workdir, before
    // it names a remote.
    #[test]
    fn push_refuses_a_gated_oid_that_is_not_a_local_commit() {
        let root = temp_root("c6-bad-oid");
        let (workdir, a, _b) = workdir_with_moved_branch(&root);
        let repo = crate::seller_git::open_plain_workdir_repo(&workdir).expect("open workdir");
        let bare = root.join("remote.git");
        Repository::init_bare(&bare).expect("bare remote");
        let remote_url = bare.to_str().expect("utf8").to_owned();
        for bad in ["", "abc", &a.to_string()[..39], &"f".repeat(40)] {
            let err = push_gated_object(&repo, &remote_url, "job", bad, None, None)
                .expect_err("refused");
            assert!(matches!(err, TransportError::Io(_)), "{bad:?}: {err}");
        }
        assert!(
            Repository::open_bare(&bare)
                .expect("bare")
                .refname_to_id("refs/heads/job")
                .is_err(),
            "nothing was pushed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // With the read-back gone, the remote's per-ref ACK is the only thing standing between "the
    // push returned" and "the delivery landed", so the parser that reads it is load-bearing. These
    // are the shapes a remote can answer with; every one that is not an explicit success for OUR
    // ref must fail.
    // Red-on-revert: relax `require_status_report` to accept an empty report and this goes red.
    #[test]
    fn the_per_ref_ack_is_the_only_accepted_answer() {
        let target = "refs/heads/job";

        // Silence is not consent: a remote that reports nothing for our ref has not accepted it.
        assert!(
            matches!(
                require_status_report(&[], target),
                Err(TransportError::Transport(_))
            ),
            "an empty status report must fail"
        );

        // An ACK for somebody else's ref is not an ACK for ours — and the error names what we did
        // get, so an operator is not left guessing.
        let other = require_status_report(&[("refs/heads/other".to_owned(), None)], target)
            .expect_err("a report for another ref must fail");
        assert!(other.to_string().contains("refs/heads/other"), "{other}");

        // A rejection is a rejection even when our ref is named.
        let rejected = require_status_report(
            &[(
                target.to_owned(),
                Some("pre-receive hook declined".to_owned()),
            )],
            target,
        )
        .expect_err("a rejection must fail");
        assert!(
            matches!(&rejected, TransportError::Rejected(m) if m.contains("pre-receive hook declined")),
            "{rejected}"
        );

        // A remote that ACKs our ref AND rejects it in the same report is not an acceptance.
        assert!(
            require_status_report(
                &[
                    (target.to_owned(), None),
                    (target.to_owned(), Some("denied".to_owned())),
                ],
                target,
            )
            .is_err(),
            "a contradictory report must fail"
        );

        // We push exactly one refspec, so exactly one ACK for exactly our ref is the only shape
        // that means "the delivery landed". A remote that also claims to have updated refs we never
        // pushed is not answering our question, and is refused too.
        let extra = require_status_report(
            &[
                ("refs/heads/other".to_owned(), None),
                (target.to_owned(), None),
            ],
            target,
        )
        .expect_err("a report naming refs we did not push must fail");
        assert!(extra.to_string().contains("refs/heads/other"), "{extra}");

        // The one accepted shape.
        require_status_report(&[(target.to_owned(), None)], target).expect("exact report");
    }

    #[test]
    fn status_report_must_name_exactly_the_delivery_ref() {
        let target = "refs/heads/job";
        require_status_report(&[(target.to_owned(), None)], target).expect("exact report");
        assert!(matches!(
            require_status_report(&[], target),
            Err(TransportError::Transport(_))
        ));
        assert!(matches!(
            require_status_report(&[("refs/heads/other".to_owned(), None)], target),
            Err(TransportError::Transport(_))
        ));
        assert!(matches!(
            require_status_report(
                &[(target.to_owned(), None), ("refs/heads/other".to_owned(), None)],
                target
            ),
            Err(TransportError::Transport(_))
        ));
        // A message is the remote's rejection: retryable, as before.
        assert!(matches!(
            require_status_report(
                &[(target.to_owned(), Some("pre-receive hook declined".to_owned()))],
                target
            ),
            Err(TransportError::Rejected(_))
        ));
    }

    // ── F1 layer (a): the layout gate runs before the push names a remote ────────────────────

    // A `.git/commondir` the job left behind is refused BEFORE any remote exists. A loopback
    // listener stands in for the relay named in the URL: it must see no connection at all.
    // Red-on-revert: open the workdir with `Repository::open` instead of the gated open and the
    // push proceeds to the transport.
    #[test]
    fn push_refuses_a_commondir_layout_before_any_network() {
        let root = temp_root("layout-commondir");
        let (workdir, a, _b) = workdir_with_moved_branch(&root);
        // A second, valid git dir the job prepared, and a commondir pointer at it.
        let other = root.join("other.git");
        Repository::init_bare(&other).expect("other git dir");
        std::fs::write(
            workdir.join(".git").join("commondir"),
            format!("{}\n", other.display()),
        )
        .expect("plant commondir");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("addr").port();
        let url = format!("https://127.0.0.1:{port}/git/o/r.git");

        let err = push_branch_with_header(
            &workdir,
            &url,
            "job",
            &a.to_string(),
            Some("Nostr token".to_owned()),
        )
        .expect_err("the layout is refused");
        assert!(
            matches!(&err, TransportError::Transport(m) if m.contains("commondir")),
            "{err}"
        );
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "no connection reached the relay"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
