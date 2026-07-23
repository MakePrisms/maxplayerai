//! Shared in-process libgit2 transport for every mobee relay-git leg — seller push, seller
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
//! - **`insteadOf` immunity:** remotes are built with [`Repository::remote_anonymous`], which
//!   uses the literal URL and does NOT apply `url.*.insteadOf` config rewrites — so an
//!   agent-planted `.git/config` (or a poisoned `$HOME/.gitconfig`) can never rewrite an
//!   allowlisted `https` URL onto a banned transport. No global/XDG/system config is consulted.
//! - **Key hygiene:** the seller/buyer secret is used ONLY in-process to sign the NIP-98 event.
//!   It is never placed on argv, never in child env, and never spawns a subprocess.

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Once, OnceLock};
use std::time::Duration;

use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream, Transport};
use git2::{AutotagOption, Direction, FetchOptions, PushOptions, RemoteCallbacks, Repository};

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

static REGISTER: Once = Once::new();

/// Per-HTTP-leg cap for the buyer money-path fetch. git2 has no whole-operation timeout, but a
/// hung leg (info/refs GET or upload-pack POST) is bounded here so the fetch fails CLOSED well
/// under the MCP tool deadline (15s) and the Claude-Code client read-timeout (~60s). A smart-HTTP
/// fetch is at most two legs, so the worst-case wall time is ~2× this — still bounded, still no pay.
const BUYER_FETCH_LEG_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether to skip TLS certificate verification. Honors `GIT_SSL_NO_VERIFY` — the SAME env var
/// system `git` obeys — so nothing changes for real deployments (the var is never set; TLS is
/// verified against the bundled webpki roots), and self-signed test fixtures work exactly as they
/// did under the old system-git path. Read once when the client is first built.
fn accept_invalid_certs() -> bool {
    std::env::var_os("GIT_SSL_NO_VERIFY").is_some()
}

/// Long-running client for pushes and seller base fetches (large packs are legitimate).
fn client_default() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
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

/// Register the https smart subtransport exactly once for this process.
fn ensure_registered() {
    REGISTER.call_once(|| {
        // SAFETY: libgit2 requires transport registration be externally synchronized with other
        // transport creation. `Once` guarantees a single registration, and mobee-core drives git2
        // ONLY through this module, so overriding the `https` scheme affects no other code path.
        unsafe {
            let _ = git2::transport::register("https", |remote| {
                let header = AUTH_HEADER.with(|cell| cell.borrow().clone());
                let short = SHORT_TIMEOUT.with(|cell| *cell.borrow());
                Transport::smart(remote, true, NostrHttp { header, short })
            });
        }
    });
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
/// service suffix) with method `POST`. mobee-relay is method-agnostic on git routes and does not
/// dedup the event id, so this ONE header is valid for both the info/refs GET advertisement and the
/// service POST — the same token-reuse the git-credential-nostr helper relied on, delivered directly
/// instead of via git's credential protocol. The secret never appears in the returned string.
pub fn nip98_authorization_header(
    remote_url: &str,
    secret_key_hex: &str,
) -> Result<String, TransportError> {
    use base64::Engine as _;
    use nostr_sdk::nips::nip98::{HttpData, HttpMethod};
    use nostr_sdk::prelude::{EventBuilder, Url};
    use nostr_sdk::{JsonUtil, Keys};

    let keys = Keys::parse(secret_key_hex)
        .map_err(|error| TransportError::Auth(format!("invalid key: {error}")))?;
    let url = Url::parse(remote_url)
        .map_err(|error| TransportError::Io(format!("invalid remote url: {error}")))?;
    let event = EventBuilder::http_auth(HttpData::new(url, HttpMethod::POST))
        .sign_with_keys(&keys)
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

/// Push `refs/heads/<branch>:refs/heads/<branch>` to `remote_url` in-process, returning the pushed
/// commit OID (full hex). `auth` is the seller secret hex (NIP-98 for relay-git; `None`/public https
/// pushes unauthenticated and fail closed at the remote).
pub fn push_branch(
    workdir: &Path,
    remote_url: &str,
    branch: &str,
    auth: Option<&str>,
) -> Result<String, TransportError> {
    assert_allowed_repo_locator(remote_url)?;
    ensure_registered();
    let header = header_for(remote_url, auth)?;

    let repo = Repository::open(workdir)
        .map_err(|error| TransportError::Io(format!("open workdir repo: {error}")))?;
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
    ensure_registered();
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
    ensure_registered();
    let header = header_for(remote_url, auth)?;

    // A bare in-memory repo is enough to host an anonymous remote for a connect+list.
    let repo = Repository::open_from_env()
        .or_else(|_| {
            let tmp = std::env::temp_dir().join(format!(
                "mobee-lsremote-{}-{}",
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
}
