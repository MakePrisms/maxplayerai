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
//! - **Object-sourced push with remote read-back (C6):** [`push_branch_with_header`] pushes the
//!   gated commit OBJECT (`<oid>:refs/heads/<branch>`), never a local ref name a surviving job
//!   process could move under the push, requires the remote's status report to name exactly that
//!   ref, and then reads the remote's advertisement over a new connection and requires the ref to
//!   point at the gated oid. It never re-resolves the local branch.
//! - **Key hygiene:** the seller/buyer secret is used ONLY in-process to sign the NIP-98 event.
//!   It is never placed on argv, never in child env, and never spawns a subprocess.
//!
//! A leaked branch-scoped token is bounded authority, not zero authority: it can replay a push to
//! that one ref of that one repository until it expires. The binding above keeps it from leaving.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::OnceLock;
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

/// What the https subtransport needs for the legs of ONE operation on ONE thread. Set by
/// [`with_context`] immediately before a push/fetch/connect and cleared right after; the registered
/// https factory snapshots it into [`NostrHttp`].
#[derive(Clone, Debug)]
struct LegContext {
    /// NIP-98 `Authorization` header, or `None` for a public/anonymous https remote.
    header: Option<String>,
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

/// Long-running client for pushes and seller base fetches (large packs are legitimate).
fn client_default() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(DEFAULT_HTTP_LEG_TIMEOUT)
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
                let (header, short, intended_url) = match context {
                    Some(context) => (context.header, context.short, Some(context.intended_url)),
                    // No operation context: no destination is bound, so `action` refuses every
                    // leg. Fail closed rather than send a request nobody named.
                    None => (None, false, None),
                };
                Transport::smart(
                    remote,
                    true,
                    NostrHttp {
                        header,
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
fn same_destination(intended: &str, actual: &str) -> bool {
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
/// 2. the remote's advertisement is read over a new connection (same header context) and
///    `refs/heads/<branch>` must point at `gated_oid`.
///
/// The local branch is never re-resolved. The workdir is opened through the layout gate
/// ([`crate::seller_git::open_plain_workdir_repo`]) and every leg is bound to `remote_url`.
pub fn push_branch_with_header(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    gated_oid: &str,
    header: Option<String>,
) -> Result<String, TransportError> {
    let uploaded = upload_gated_branch(workdir, remote_url, branch, gated_oid, header.clone())?;
    attest_pushed_branch(&uploaded, header)
}

/// A delivery whose pack the remote accepted and reported status for, and which is NOT yet attested.
/// Deliberately not a `String`: the only way from here to a delivered commit oid is
/// [`attest_pushed_branch`], so "uploaded" cannot be read as "verified" by a caller that drops the
/// second leg.
#[derive(Debug, Clone)]
#[must_use = "an uploaded delivery is not delivered until attest_pushed_branch verifies the remote ref"]
pub struct UploadedDelivery {
    remote_url: String,
    target_ref: String,
    gated_oid: String,
}

impl UploadedDelivery {
    /// Re-hydrate an upload the seller node JOURNALED durably, so a resume after a crash, a kill, or
    /// a 401 on the read-back can run the SAME attestation the interrupted push owed — under a
    /// freshly minted token — instead of re-pushing a pack the remote may already hold.
    ///
    /// This constructor is the resume path's only entry, and it grants nothing the read-back does not
    /// re-check on the wire: [`attest_pushed_branch`] still asserts the outbound allowlist, still
    /// opens a detached remote bound to `remote_url`, and still refuses unless `target_ref` is
    /// advertised at exactly `gated_oid`. An `UploadedDelivery` built here is a QUESTION for the
    /// remote, never an answer — a caller cannot turn it into a delivered oid without that check
    /// passing.
    pub fn from_journaled_upload(remote_url: String, target_ref: String, gated_oid: String) -> Self {
        Self {
            remote_url,
            target_ref,
            gated_oid,
        }
    }

    /// The repo-root URL every leg of this delivery is bound to.
    pub fn remote_url(&self) -> &str {
        &self.remote_url
    }

    /// The fully-qualified ref the pack was pushed to (`refs/heads/<branch>`).
    pub fn target_ref(&self) -> &str {
        &self.target_ref
    }

    /// The gated commit that was pushed — the ONLY oid the attestation accepts.
    pub fn gated_oid(&self) -> &str {
        &self.gated_oid
    }
}

/// Leg 1 of a delivery push: send the gated OBJECT to `refs/heads/<branch>` at `remote_url` and
/// require the remote's status report to name exactly that ref with no error message. Nothing is
/// attested here — the returned [`UploadedDelivery`] must be handed to [`attest_pushed_branch`].
///
/// `gated_oid` is the push SOURCE (refspec `<gated_oid>:refs/heads/<branch>`), so the bytes on the
/// wire are that object's graph no matter where the local branch points during the push (C6). The
/// workdir is opened through the layout gate ([`crate::seller_git::open_plain_workdir_repo`]) and
/// every leg is bound to `remote_url`.
pub fn upload_gated_branch(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    gated_oid: &str,
    header: Option<String>,
) -> Result<UploadedDelivery, TransportError> {
    assert_allowed_repo_locator(remote_url)?;
    ensure_registered()?;
    let repo = open_delivery_repo(workdir)?;
    upload_gated_object(&repo, remote_url, branch, gated_oid, header)
}

/// Leg 2 of a delivery push: open a NEW connection to the SAME repo-root under `header`, read the
/// remote's ref advertisement, and require the uploaded ref to point at the uploaded gated oid.
/// Returns that attested oid — the delivered commit.
///
/// `header` is minted by the caller AFTER [`upload_gated_branch`] returns, which is the point of the
/// split: the relay's default NIP-98 rule is a ±60 s window on `created_at`, so a token minted
/// before a transfer that outlives that window is already refused when the read-back runs — a 401 on
/// the verification of a push that succeeded. A fresh token is bounded exactly as the upload's was
/// (signer-owned, repo-root-bound `u` tag, same ref scope): nothing is relaxed, and the oid compared
/// is still the exact gated oid.
///
/// The connection is a DETACHED remote: it carries no repository config, so no `insteadOf` can
/// rewrite it, and the transport still refuses any leg whose destination is not
/// [`UploadedDelivery::remote_url`].
pub fn attest_pushed_branch(
    uploaded: &UploadedDelivery,
    header: Option<String>,
) -> Result<String, TransportError> {
    assert_allowed_repo_locator(&uploaded.remote_url)?;
    ensure_registered()?;
    attest_uploaded_delivery(uploaded, header)
}

/// [`attest_pushed_branch`] without the outbound allowlist assertion, so the split legs can be
/// exercised against a local bare repository — which the allowlist refuses on the public entry
/// point — exactly as [`upload_gated_object`] is.
fn attest_uploaded_delivery(
    uploaded: &UploadedDelivery,
    header: Option<String>,
) -> Result<String, TransportError> {
    let mut remote = Remote::create_detached(uploaded.remote_url.as_str())
        .map_err(|error| TransportError::Io(format!("detached remote for attestation: {error}")))?;
    let context = LegContext {
        header,
        short: false,
        intended_url: uploaded.remote_url.clone(),
    };
    attest_remote_branch(
        &mut remote,
        context,
        &uploaded.target_ref,
        &uploaded.gated_oid,
    )?;
    Ok(uploaded.gated_oid.clone())
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

/// The upload proper, on an already-opened repository: bind the remote, push the OBJECT `gated_oid`
/// to `refs/heads/<branch>` and check the status report. The remote read-back is NOT done here —
/// it is [`attest_uploaded_delivery`], run under its own freshly minted token. Split from
/// [`upload_gated_branch`] so the object-sourced push can be exercised against a local bare
/// repository, which the transport allowlist refuses on the public entry point.
fn upload_gated_object(
    repo: &Repository,
    remote_url: &str,
    branch: &str,
    gated_oid: &str,
    header: Option<String>,
) -> Result<UploadedDelivery, TransportError> {
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
        header,
        short: false,
        intended_url: remote_url.to_owned(),
    };
    with_context(context, || {
        remote.push(&[refspec.as_str()], Some(&mut options))
    })?;
    drop(options);
    require_status_report(&reports.borrow(), &target_ref)?;
    Ok(UploadedDelivery {
        remote_url: remote_url.to_owned(),
        target_ref,
        gated_oid: gated,
    })
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

/// Remote attestation: open a NEW connection to the remote under `context`, read its current ref
/// advertisement, and require `target_ref` to point at `gated_oid`. The header in `context` is the
/// read-back's OWN token (minted after the upload settled), not the upload's; the relay is
/// method-agnostic and does not dedup the event id, so either would be accepted while fresh — only
/// this one is still fresh after a slow transfer. The local branch is not consulted.
fn attest_remote_branch(
    remote: &mut Remote<'_>,
    context: LegContext,
    target_ref: &str,
    gated_oid: &str,
) -> Result<(), TransportError> {
    let heads = with_context(context, || {
        let connection = remote.connect_auth(Direction::Push, None, None)?;
        let heads = connection
            .list()?
            .iter()
            .map(|head| (head.name().to_owned(), head.oid().to_string()))
            .collect::<Vec<_>>();
        Ok(heads)
    })?;
    check_remote_attestation(&heads, target_ref, gated_oid)
}

/// The attestation comparison on an advertisement: `target_ref` present and equal to `gated_oid`.
fn check_remote_attestation(
    heads: &[(String, String)],
    target_ref: &str,
    gated_oid: &str,
) -> Result<(), TransportError> {
    match heads.iter().find(|(name, _)| name == target_ref) {
        Some((_, oid)) if oid == gated_oid => Ok(()),
        Some((_, oid)) => Err(TransportError::Rejected(format!(
            "remote attestation failed: {target_ref} is at {oid}, not at the pushed {gated_oid}"
        ))),
        None => Err(TransportError::Rejected(format!(
            "remote attestation failed: {target_ref} is absent from the remote after the push"
        ))),
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
        header,
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
        header,
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
    header: Option<String>,
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
            header: self.header.clone(),
            short: self.short,
            url: full_url,
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
    header: Option<String>,
    short: bool,
    url: String,
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
        if let Some(header) = &self.header {
            request = request.header("Authorization", header);
        }
        let response = request
            .send()
            .map_err(|error| io::Error::other(format!("http request: {error}")))?;
        let status = response.status();
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
            header: Some("Nostr token".to_owned()),
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
            header: Some("Nostr token".to_owned()),
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
        let uploaded = upload_gated_object(&repo, &remote_url, "job", &a.to_string(), None)
            .expect("upload the gated object");
        let pushed = attest_uploaded_delivery(&uploaded, None).expect("attest the gated object");
        assert_eq!(pushed, a.to_string(), "the returned oid is the gated one");

        let remote_repo = Repository::open_bare(&bare).expect("open bare");
        let remote_tip = remote_repo
            .refname_to_id("refs/heads/job")
            .expect("remote ref");
        assert_eq!(remote_tip, a, "the remote got the gated object A, not the branch tip B");
        assert_ne!(remote_tip, b);
        // The local branch was neither consulted nor touched.
        assert_eq!(repo.refname_to_id("refs/heads/job").expect("local ref"), b);

        // A repeat push of the same object (the resume path) is accepted and attested again.
        let re_uploaded = upload_gated_object(&repo, &remote_url, "job", &a.to_string(), None)
            .expect("re-push the gated object");
        let again = attest_uploaded_delivery(&re_uploaded, None).expect("re-attest");
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
            let err = upload_gated_object(&repo, &remote_url, "job", bad, None)
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

    // The remote read-back refuses when the remote's ref disagrees with what was pushed: here a
    // second client moves the remote ref to B after our push of A completed.
    // Red-on-revert: make `check_remote_attestation` return Ok and this passes vacuously.
    #[test]
    fn attestation_refuses_a_remote_whose_ref_moved_after_the_push() {
        let root = temp_root("c6-attest");
        let (workdir, a, b) = workdir_with_moved_branch(&root);
        let bare = root.join("remote.git");
        Repository::init_bare(&bare).expect("bare remote");
        let remote_url = bare.to_str().expect("utf8").to_owned();
        let repo = crate::seller_git::open_plain_workdir_repo(&workdir).expect("open workdir");
        let uploaded_a = upload_gated_object(&repo, &remote_url, "job", &a.to_string(), None)
            .expect("push A");
        attest_uploaded_delivery(&uploaded_a, None).expect("attest A before it moves");

        // A second client moves the remote ref to B.
        {
            let other = Repository::open(&workdir).expect("second client");
            let mut other_remote = other.remote_anonymous(&remote_url).expect("remote");
            other_remote
                .push(&["+refs/heads/job:refs/heads/job"], None)
                .expect("move the remote ref to B");
        }
        assert_eq!(
            Repository::open_bare(&bare)
                .expect("bare")
                .refname_to_id("refs/heads/job")
                .expect("remote ref"),
            b
        );

        // The read-back that follows a push of A must refuse: the remote disagrees with the push.
        let mut remote = bound_remote(&repo, &remote_url).expect("bound remote");
        let context = LegContext {
            header: None,
            short: false,
            intended_url: remote_url.clone(),
        };
        let err = attest_remote_branch(&mut remote, context, "refs/heads/job", &a.to_string())
            .expect_err("the remote disagrees");
        assert!(
            matches!(&err, TransportError::Rejected(m) if m.contains("remote attestation failed")),
            "{err}"
        );
        assert!(
            err.to_string().contains(&b.to_string()),
            "names the oid the remote holds: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // RESUME, with real git: an upload whose read-back never happened is re-verified from the
    // JOURNALED facts alone (remote url + ref + oid — what the store holds), and the exact-oid gate
    // still decides. Nothing is re-pushed here: the second call touches no workdir.
    #[test]
    fn a_journaled_upload_re_verifies_and_still_refuses_a_different_oid() {
        let root = temp_root("resume-attest");
        let (workdir, a, b) = workdir_with_moved_branch(&root);
        let bare = root.join("remote.git");
        Repository::init_bare(&bare).expect("bare remote");
        let remote_url = bare.to_str().expect("utf8").to_owned();
        let repo = crate::seller_git::open_plain_workdir_repo(&workdir).expect("open workdir");
        // The pre-crash run: the pack lands on the remote, and its `UploadedDelivery` is DROPPED on
        // purpose — that value did not survive the crash, only the journaled strings below did.
        drop(upload_gated_object(&repo, &remote_url, "job", &a.to_string(), None).expect("upload A"));

        // What the store journaled — no `UploadedDelivery` survived the crash, only these strings.
        let journaled = UploadedDelivery::from_journaled_upload(
            remote_url.clone(),
            "refs/heads/job".to_owned(),
            a.to_string(),
        );
        assert_eq!(
            attest_uploaded_delivery(&journaled, None).expect("the pack is on the remote"),
            a.to_string(),
            "a resume re-verifies the journaled upload and returns the delivered oid"
        );

        // FAIL CLOSED on a wrong oid: same remote, same ref, an oid the remote does not hold.
        let wrong = UploadedDelivery::from_journaled_upload(
            remote_url,
            "refs/heads/job".to_owned(),
            b.to_string(),
        );
        let err = attest_uploaded_delivery(&wrong, None).expect_err("the remote disagrees");
        assert!(
            matches!(&err, TransportError::Rejected(m) if m.contains("remote attestation failed")),
            "a journaled oid the remote does not hold is REJECTED, never delivered: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn attestation_comparison_requires_the_ref_at_the_gated_oid() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let agrees = vec![
            ("HEAD".to_owned(), a.clone()),
            ("refs/heads/job".to_owned(), a.clone()),
        ];
        check_remote_attestation(&agrees, "refs/heads/job", &a).expect("agrees");
        let moved = vec![("refs/heads/job".to_owned(), b.clone())];
        assert!(matches!(
            check_remote_attestation(&moved, "refs/heads/job", &a),
            Err(TransportError::Rejected(_))
        ));
        let absent = vec![("refs/heads/other".to_owned(), a.clone())];
        assert!(matches!(
            check_remote_attestation(&absent, "refs/heads/job", &a),
            Err(TransportError::Rejected(_))
        ));
        assert!(matches!(
            check_remote_attestation(&[], "refs/heads/job", &a),
            Err(TransportError::Rejected(_))
        ));
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
