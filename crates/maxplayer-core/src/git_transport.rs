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
//! - **`insteadOf` / ambient-config immunity:** at first use, [`ensure_registered`] empties
//!   libgit2's global/XDG/system config search path, so NO ambient git config is consulted on any
//!   in-process leg. This is load-bearing: libgit2 applies `url.*.insteadOf` at CONNECT time and
//!   [`Repository::remote_anonymous`] does NOT prevent it — only clearing the search path does. So an
//!   ambient or agent-planted/poisoned `$HOME`/XDG/system config can never rewrite an allowlisted
//!   `https` URL onto another host or a banned transport after the allowlist check (#610). Only a
//!   repo-LOCAL config (in a workdir we create) is ever read, and none rewrites.
//! - **Key hygiene:** the seller/buyer secret is used ONLY in-process to sign the NIP-98 event.
//!   It is never placed on argv, never in child env, and never spawns a subprocess.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream, Transport};
use git2::{
    AutotagOption, ConfigLevel, Direction, FetchOptions, PushOptions, RemoteCallbacks, Repository,
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

thread_local! {
    /// NIP-98 `Authorization` header for the operation running on THIS thread. Set immediately
    /// before a push/fetch/connect and cleared right after; the registered https factory reads it.
    static AUTH_HEADER: RefCell<Option<String>> = const { RefCell::new(None) };
    /// When true, the operation on this thread uses the SHORT-timeout HTTP client (the buyer
    /// money-path fetch: a hung fetch must fail CLOSED before authorize_pay burns budget).
    static SHORT_TIMEOUT: RefCell<bool> = const { RefCell::new(false) };
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
                let header = AUTH_HEADER.with(|cell| cell.borrow().clone());
                let short = SHORT_TIMEOUT.with(|cell| *cell.borrow());
                Transport::smart(remote, true, NostrHttp { header, short })
            })
            .map_err(|error| format!("register https subtransport: {}", error.message()))?;
        }
        Ok(())
    })
    .clone()
    .map_err(TransportError::Io)
}

/// Run `body` with the NIP-98 header and timeout-class bound to this thread, clearing both
/// afterward so no stray auth/timeout leaks into an unrelated later operation on the same thread.
fn with_context<T>(header: Option<String>, short: bool, body: impl FnOnce() -> T) -> T {
    AUTH_HEADER.with(|cell| *cell.borrow_mut() = header);
    SHORT_TIMEOUT.with(|cell| *cell.borrow_mut() = short);
    let result = body();
    AUTH_HEADER.with(|cell| *cell.borrow_mut() = None);
    SHORT_TIMEOUT.with(|cell| *cell.borrow_mut() = false);
    result
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
    nip98_authorization_header_with_keys(remote_url, &keys)
}

/// Build the NIP-98 `Authorization` header from an already-held [`Keys`](nostr_sdk::Keys) instead of
/// a raw secret hex. This is the custody-preserving entry point: a caller that keeps its secret
/// inside a signer actor signs the header THROUGH the actor (which owns the `Keys`) so the secret is
/// never re-read into a third site. Identical header to [`nip98_authorization_header`].
pub fn nip98_authorization_header_with_keys(
    remote_url: &str,
    keys: &nostr_sdk::Keys,
) -> Result<String, TransportError> {
    use base64::Engine as _;
    use nostr_sdk::nips::nip98::{HttpData, HttpMethod};
    use nostr_sdk::prelude::{EventBuilder, Url};
    use nostr_sdk::JsonUtil;

    let url = Url::parse(remote_url)
        .map_err(|error| TransportError::Io(format!("invalid remote url: {error}")))?;
    let event = EventBuilder::http_auth(HttpData::new(url, HttpMethod::POST))
        .sign_with_keys(keys)
        .map_err(|error| TransportError::Auth(format!("nip98 sign failed: {error}")))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(event.as_json());
    Ok(format!("Nostr {encoded}"))
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

/// Where hook lookup is pinned on a job workdir's git legs. `/dev/null` is not a directory, so a
/// `git` CLI resolving a hook beneath it finds nothing and runs nothing.
const NEUTRALISED_HOOKS_PATH: &str = "/dev/null";

/// What a job left in its workdir's `.git/`, found by [`neutralise_workdir_hooks`] while pinning hook
/// lookup off. Reported, never fatal — see that function for why refusing would be the worse trade.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookFindings {
    /// Live hook file names under `hooks/`. `*.sample` is excluded: git writes those itself and they
    /// are inert, so counting them would flag every normal workdir as planted.
    pub hooks: Vec<String>,
    /// A repo-LOCAL `core.hooksPath` that was already set to something other than our own pin —
    /// i.e. the job pointed hook lookup at a directory it controls.
    pub hooks_path: Option<String>,
}

impl HookFindings {
    /// Nothing planted: no live hook file and no `core.hooksPath` of the job's own.
    pub fn is_clean(&self) -> bool {
        self.hooks.is_empty() && self.hooks_path.is_none()
    }
}

/// Pin `core.hooksPath` to [`NEUTRALISED_HOOKS_PATH`] in a job workdir's LOCAL config, reporting any
/// hook the job planted (#826).
///
/// The job agent owns this workdir by design — it is bind-mounted read-write into the container at
/// `/work`, so `.git/` is reachable from inside the sandbox and the agent can leave a `pre-push` or a
/// `core.hooksPath` behind. The delivery legs then run on the HOST, and `git push` executes hooks.
/// That would be sandbox → host code execution on the delivery path, in the daemon's environment,
/// beside the wallet and keys the sandbox exists to protect.
///
/// **Why a config write and not the `-c core.hooksPath=/dev/null` / `--no-verify` flags #826 asks
/// for: there is no argv to put them on.** Every leg here is in-process libgit2, which executes NO
/// hooks at all — `seller_git::tests::snapshot_bypasses_base_repo_hooks` proves it by planting a
/// failing `pre-commit` and asserting delivery still succeeds. So no planted hook runs today and a
/// CLI flag has nothing to attach to. Writing the pin into the repo's own config puts it where the
/// `git` CLI *would* honour it, so the property holds for a future call site that shells out — the
/// only way this vector can actually open. `tests/no_system_git.rs` is what keeps such a call site
/// from arriving unnoticed, and it runs only in the `git-delivery` money-path job.
///
/// **Why report and not refuse**, which is the one place this departs from the issue's suggestion:
/// the planted hook is inert, so refusing would trade nothing for a real availability loss — the
/// untrusted agent could fail its own seller's delivery, and an unpushed delivery is an unpaid job.
/// Refusing would also invert the deliberate property that base-repo hooks cannot block a delivery.
///
/// Ordering is load-bearing: this runs on the delivery legs, after the container is gone. Pinning at
/// workdir init instead would leave the agent free to overwrite it.
pub(crate) fn neutralise_workdir_hooks(repo: &Repository) -> Result<HookFindings, TransportError> {
    let mut findings = HookFindings::default();

    let mut config = repo
        .config()
        .and_then(|config| config.open_level(ConfigLevel::Local))
        .map_err(|error| TransportError::Io(format!("open local config: {error}")))?;
    if let Ok(existing) = config.get_string("core.hooksPath") {
        if existing != NEUTRALISED_HOOKS_PATH {
            findings.hooks_path = Some(existing);
        }
    }

    if let Ok(entries) = std::fs::read_dir(repo.path().join("hooks")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".sample") {
                findings.hooks.push(name);
            }
        }
        findings.hooks.sort();
    }

    config
        .set_str("core.hooksPath", NEUTRALISED_HOOKS_PATH)
        .map_err(|error| TransportError::Io(format!("pin core.hooksPath: {error}")))?;

    Ok(findings)
}

/// Pin hooks off for `repo` and surface anything the job planted on stderr, beside the push/snapshot
/// lines the delivery legs already emit. A job writing into `.git/` is a signal worth seeing even
/// though it cannot execute.
pub(crate) fn neutralise_and_report_hooks(repo: &Repository, leg: &str) -> Result<(), TransportError> {
    let findings = neutralise_workdir_hooks(repo)?;
    if !findings.is_clean() {
        eprintln!(
            "seller {leg} hooks-neutralised planted={:?} prior-hooksPath={:?}",
            findings.hooks, findings.hooks_path
        );
    }
    Ok(())
}

/// Push `refs/heads/<branch>:refs/heads/<branch>` to `remote_url` in-process, returning the pushed
/// commit OID (full hex). `auth` is the seller secret hex (NIP-98 for relay-git; `None`/public https
/// pushes unauthenticated and fail closed at the remote).
pub fn push_branch(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    auth: Option<&str>,
) -> Result<String, TransportError> {
    let header = header_for(remote_url, auth)?;
    push_branch_with_header(workdir, remote_url, branch, header)
}

/// Like [`push_branch`] but takes an already-resolved NIP-98 `Authorization` header instead of the
/// raw secret. A custody-preserving caller builds the header through its signer actor
/// ([`nip98_authorization_header_with_keys`]) and passes it here, so the secret never reaches this
/// layer. `None` = no auth (public/anonymous https). The header is bound to the repo-root URL and
/// reused for both the info/refs advertisement and the service POST, exactly as `push_branch` does.
pub fn push_branch_with_header(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    header: Option<String>,
) -> Result<String, TransportError> {
    assert_allowed_repo_locator(remote_url)?;
    ensure_registered()?;

    let repo = Repository::open(workdir)
        .map_err(|error| TransportError::Io(format!("open workdir repo: {error}")))?;
    // #826: the workdir was writable by the sandboxed job, and this leg runs on the HOST. Pin hook
    // lookup off before any git write, and surface anything the job planted.
    neutralise_and_report_hooks(&repo, "push")?;
    let mut remote = repo
        .remote_anonymous(remote_url)
        .map_err(|error| TransportError::Io(format!("anonymous remote: {error}")))?;

    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let rejection: std::rc::Rc<RefCell<Option<String>>> = std::rc::Rc::new(RefCell::new(None));
    let mut callbacks = RemoteCallbacks::new();
    {
        let rejection = rejection.clone();
        callbacks.push_update_reference(move |refname, status| {
            if let Some(message) = status {
                *rejection.borrow_mut() = Some(format!("{refname}: {message}"));
            }
            Ok(())
        });
    }
    let mut options = PushOptions::new();
    options.remote_callbacks(callbacks);

    let push_result = with_context(header, false, || {
        remote.push(&[refspec.as_str()], Some(&mut options))
    });
    drop(options);
    push_result.map_err(map_git_error)?;

    if let Some(message) = rejection.borrow().clone() {
        return Err(TransportError::Rejected(message));
    }

    let oid = repo
        .revparse_single(&format!("refs/heads/{branch}"))
        .and_then(|object| object.peel_to_commit())
        .map(|commit| commit.id().to_string())
        .map_err(|error| TransportError::Io(format!("resolve pushed oid: {error}")))?;
    if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(TransportError::Io(format!("unexpected commit oid {oid:?}")));
    }
    Ok(oid)
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

    let mut remote = repo
        .remote_anonymous(remote_url)
        .map_err(|error| TransportError::Io(format!("anonymous remote: {error}")))?;
    let mut options = FetchOptions::new();
    options.download_tags(AutotagOption::None);

    let result = with_context(header, short_timeout, || {
        remote.fetch(refspecs, Some(&mut options), None)
    });
    drop(options);
    result.map_err(map_git_error)
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
    let mut remote = repo
        .remote_anonymous(remote_url)
        .map_err(|error| TransportError::Io(format!("anonymous remote: {error}")))?;

    let heads = with_context(header, false, || {
        remote.connect(direction)?;
        let list = remote
            .list()?
            .iter()
            .map(|h| (h.name().to_owned(), h.oid().to_string()))
            .collect::<Vec<_>>();
        let _ = remote.disconnect();
        Ok::<_, git2::Error>(list)
    })
    .map_err(map_git_error)?;
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
/// and uses the short- or long-timeout client per the operation's timeout class.
struct NostrHttp {
    header: Option<String>,
    short: bool,
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
    fn action(
        &self,
        url: &str,
        service: Service,
    ) -> Result<Box<dyn SmartSubtransportStream>, git2::Error> {
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

    #[test]
    fn allowlist_refused_before_any_network() {
        assert!(matches!(
            push_branch(
                std::path::Path::new("/nonexistent"),
                "ext::sh -c evil",
                "main",
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

    // ── #826: hook neutralisation on the delivery legs ──────────────────────────────────────────
    fn hooks_repo(label: &str) -> (std::path::PathBuf, Repository) {
        let dir = std::env::temp_dir().join(format!(
            "maxplayer-hooks-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let repo = Repository::init(&dir).expect("init repo");
        (dir, repo)
    }

    fn planted_hook(repo: &Repository, name: &str, body: &str) {
        let hooks = repo.path().join("hooks");
        std::fs::create_dir_all(&hooks).expect("hooks dir");
        std::fs::write(hooks.join(name), body).expect("write hook");
    }

    fn local_hooks_path(repo: &Repository) -> Option<String> {
        repo.config()
            .and_then(|config| config.open_level(ConfigLevel::Local))
            .and_then(|config| config.get_string("core.hooksPath"))
            .ok()
    }

    // POSITIVE CONTROL for the `*.sample` trap. `git init` populates `hooks/` with sample files, so a
    // naive "non-empty `.git/hooks`" check would flag EVERY workdir — a guard that fires on everything
    // is not a guard. A normal workdir must read CLEAN. The sample is planted explicitly rather than
    // trusting whatever libgit2's `init` happens to write, so the exclusion is red-on-revert: drop the
    // `.sample` filter and this test fails.
    #[test]
    fn a_normal_workdir_and_its_samples_read_clean() {
        let (dir, repo) = hooks_repo("clean");
        planted_hook(&repo, "pre-push.sample", "#!/bin/sh\nexit 1\n");

        let findings = neutralise_workdir_hooks(&repo).expect("neutralise");

        assert!(
            findings.is_clean(),
            "a normal workdir must still deliver; samples are inert, got {findings:?}"
        );
        assert_eq!(local_hooks_path(&repo).as_deref(), Some(NEUTRALISED_HOOKS_PATH));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A hook the job planted is REPORTED — the write into `.git/` is the signal worth surfacing — and
    // hook lookup is pinned off, so a `git` CLI call site would find nothing to run. Delivery is NOT
    // refused: refusing would let the untrusted agent fail its own seller's push.
    #[test]
    fn a_planted_hook_is_reported_and_lookup_is_pinned_off() {
        let (dir, repo) = hooks_repo("planted");
        planted_hook(&repo, "pre-push", "#!/bin/sh\nid > /tmp/escaped\n");

        let findings = neutralise_workdir_hooks(&repo).expect("neutralise");

        assert_eq!(findings.hooks, vec!["pre-push".to_string()]);
        assert!(!findings.is_clean());
        assert_eq!(local_hooks_path(&repo).as_deref(), Some(NEUTRALISED_HOOKS_PATH));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The other half of the vector, and the half a hooks-directory scan alone would miss: no hook
    // FILE, but `core.hooksPath` aimed at a directory the job controls.
    #[test]
    fn a_planted_hooks_path_is_reported_and_overwritten() {
        let (dir, repo) = hooks_repo("hookspath");
        repo.config()
            .and_then(|config| config.open_level(ConfigLevel::Local))
            .expect("local config")
            .set_str("core.hooksPath", "/work/.evil-hooks")
            .expect("plant hooksPath");

        let findings = neutralise_workdir_hooks(&repo).expect("neutralise");

        assert_eq!(findings.hooks_path.as_deref(), Some("/work/.evil-hooks"));
        assert_eq!(local_hooks_path(&repo).as_deref(), Some(NEUTRALISED_HOOKS_PATH));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Both post-agent legs run the pin (snapshot, then push), so the second one reads the pin the
    // first one wrote. It must not report OUR OWN pin as a planted `hooksPath` — that false positive
    // would fire on every normal contribution delivery, which is the shape that makes a warning
    // worthless.
    #[test]
    fn our_own_pin_is_not_reported_as_planted() {
        let (dir, repo) = hooks_repo("idempotent");
        neutralise_workdir_hooks(&repo).expect("first leg");

        let findings = neutralise_workdir_hooks(&repo).expect("second leg");

        assert!(
            findings.is_clean(),
            "the pin we wrote ourselves is not a planted hook, got {findings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
