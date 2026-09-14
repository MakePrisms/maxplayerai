//! Test-only git-over-HTTPS fixture with a NIP-98-style auth gate.
//!
//! Serves a local repository over the git smart-HTTP protocol on 127.0.0.1, speaking
//! HTTPS through an in-memory self-signed cert (rustls + rcgen — the seller transport
//! allowlist refuses plain `http://`, so a faithful fixture must be `https://`). Every
//! smart-transport request (`info/refs`, `git-upload-pack`, `git-receive-pack`) requires
//! an `Authorization` header — mirroring maxplayer relay-git, which gates reads AND writes
//! behind NIP-98 — and is answered `401` + `WWW-Authenticate` challenge otherwise, so a
//! stock git client only moves refs/packs after its credential helper produces a
//! credential. Every request is recorded (method, target, Authorization) so tests can
//! assert on the wire what the seller code path actually presented.
//!
//! Reached through the REAL seller path (`init_contribution_workdir` → scrubbed
//! `git fetch`): the scrub strips SSH/insteadOf/config vectors but keeps ambient
//! `GIT_SSL_NO_VERIFY`, which is how tests accept the self-signed cert without touching
//! production code.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

type TlsStream = StreamOwned<ServerConnection, TcpStream>;

/// One HTTP request the fixture saw. Recorded BEFORE the auth gate runs, so refused
/// probes show up too (`authorization: None`).
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    /// Path + query exactly as sent by the client.
    pub target: String,
    pub authorization: Option<String>,
}

/// Optional behaviours a test can ask the fixture for. Default = the plain auth-gated server every
/// existing test uses; each knob is additive.
#[derive(Clone, Default)]
pub struct FixtureOptions {
    /// Sleep this long before answering the FIRST request this server sees, so a test can put real
    /// elapsed time between one leg and the next (a token minted once, up front, then visibly
    /// predates the later legs).
    pub first_response_delay: Option<Duration>,
    /// Answer every smart-HTTP request with `302` + this `Location` instead of serving git — for
    /// asserting what a client does with a redirect it was never authorized to follow.
    pub redirect_to: Option<String>,
    /// Hold the FIRST request here until the test releases it, instead of for a guessed duration.
    ///
    /// A delay is a bet that the other party got scheduled in time; this is an appointment. The
    /// fixture parks the first request and tells the test it has parked ([`RequestGate::wait_held`]);
    /// the test does whatever it needed the pause for and then opens the gate
    /// ([`RequestGate::release`]). Nothing in between depends on a clock.
    pub hold_first_request: Option<Arc<RequestGate>>,
}

/// A one-shot appointment between the fixture and the test: the fixture parks a request and waits;
/// the test observes that it is parked and decides when it continues.
#[derive(Default)]
pub struct RequestGate {
    /// `(request is parked, gate is open)` under one mutex, so both transitions are observed in the
    /// same critical section.
    state: Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

impl RequestGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Held side: announce that something is parked here, then block until the test opens the
    /// gate. Bounded so a mistake in a test fails as a test rather than hanging the suite.
    ///
    /// Used by the fixture to hold a request, and by tests that need to hold anything else at a
    /// chosen instruction — a signer that takes its time, for one — without a sleep.
    pub fn park(&self) {
        let mut state = self.state.lock().expect("gate");
        state.0 = true;
        self.changed.notify_all();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !state.1 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "a held request was never released");
            let (next, _) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("gate wait");
            state = next;
        }
    }

    /// Test side: block until a request is actually parked in the fixture. Returning means the
    /// server has read that request's head — its `Authorization` is already recorded — and is not
    /// going to answer it until [`Self::release`].
    pub fn wait_held(&self) {
        let mut state = self.state.lock().expect("gate");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !state.0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "no request ever reached the gate");
            let (next, _) = self
                .changed
                .wait_timeout(state, remaining)
                .expect("gate wait");
            state = next;
        }
    }

    /// Test side: let the held request — and every request after it — proceed.
    pub fn release(&self) {
        let mut state = self.state.lock().expect("gate");
        state.1 = true;
        self.changed.notify_all();
    }
}

/// Auth-gated smart-HTTP git server bound to `127.0.0.1:<ephemeral>`.
pub struct GitHttpAuthServer {
    addr: SocketAddr,
    mount: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    concurrency: Arc<Concurrency>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

/// How many authorized requests this server was serving AT ONCE, at the high-water mark.
///
/// The wire's own answer to "did two deliveries upload at the same time?". A lock held only until
/// the caller stops waiting leaves an abandoned upload running, and the next delivery's
/// `git-receive-pack` overlaps it — which is visible right here, at the server, as a peak above one.
#[derive(Default)]
struct Concurrency {
    live: Mutex<(usize, usize)>,
}

impl Concurrency {
    fn enter(&self) {
        let mut live = self.live.lock().expect("concurrency");
        live.0 += 1;
        live.1 = live.1.max(live.0);
    }

    fn leave(&self) {
        let mut live = self.live.lock().expect("concurrency");
        live.0 -= 1;
    }

    fn peak(&self) -> usize {
        self.live.lock().expect("concurrency").1
    }
}

/// Counts one authorized request as in-flight for as long as this server is working on it — through
/// every early return in [`handle_connection`], including a held one.
struct InFlight<'a>(&'a Concurrency);

impl<'a> InFlight<'a> {
    fn enter(concurrency: &'a Concurrency) -> Self {
        concurrency.enter();
        Self(concurrency)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.leave();
    }
}

impl GitHttpAuthServer {
    /// Serve `repo` (a worktree or bare repo — anything `git upload-pack` accepts) at
    /// `https://127.0.0.1:<port><mount>`; `mount` is the URL path of the repo, e.g.
    /// `/git/<owner>/base.git` for the relay-git shape.
    pub fn spawn(repo: &Path, mount: &str) -> Self {
        Self::spawn_with(repo, mount, FixtureOptions::default())
    }

    /// [`GitHttpAuthServer::spawn`] with [`FixtureOptions`].
    pub fn spawn_with(repo: &Path, mount: &str, options: FixtureOptions) -> Self {
        let tls_config = Arc::new(self_signed_tls_config());
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fixture listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("listener addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let concurrency = Arc::new(Concurrency::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let repo: PathBuf = repo.to_path_buf();
        let mount_bg = mount.to_owned();
        let requests_bg = Arc::clone(&requests);
        let concurrency_bg = Arc::clone(&concurrency);
        let shutdown_bg = Arc::clone(&shutdown);
        let options = Arc::new(options);
        // The delay is a one-shot: it exists to separate the first leg from the rest in time, not to
        // slow every leg of the test.
        let delay_spent = Arc::new(AtomicBool::new(false));
        // The gate is a one-shot for the same reason: it parks the FIRST request, not every one.
        let gate_spent = Arc::new(AtomicBool::new(false));
        let accept_thread = std::thread::spawn(move || {
            while !shutdown_bg.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        let tls_config = Arc::clone(&tls_config);
                        let repo = repo.clone();
                        let mount = mount_bg.clone();
                        let requests = Arc::clone(&requests_bg);
                        let concurrency = Arc::clone(&concurrency_bg);
                        let options = Arc::clone(&options);
                        let delay_spent = Arc::clone(&delay_spent);
                        let gate_spent = Arc::clone(&gate_spent);
                        std::thread::spawn(move || {
                            let _ = handle_connection(
                                stream,
                                tls_config,
                                &repo,
                                &mount,
                                &requests,
                                &concurrency,
                                &options,
                                &delay_spent,
                                &gate_spent,
                            );
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            mount: mount.to_owned(),
            requests,
            concurrency,
            shutdown,
            accept_thread: Some(accept_thread),
        }
    }

    /// Clone/fetch URL of the served repo (allowlist-shaped: https, credential-free).
    pub fn repo_url(&self) -> String {
        format!("https://127.0.0.1:{}{}", self.addr.port(), self.mount)
    }

    /// Snapshot of every request seen so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("requests lock").clone()
    }

    /// The most requests this server was serving simultaneously. See [`Concurrency`].
    pub fn peak_concurrent_requests(&self) -> usize {
        self.concurrency.peak()
    }
}

impl Drop for GitHttpAuthServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

/// `pub` so the h2 fixture (`tests/h2_no_hidden_replay.rs`) serves the SAME certificate this one
/// does: the client's trust decision is then identical, and the only thing that differs between the
/// two fixtures is the protocol they negotiate.
pub fn self_signed_tls_config() -> ServerConfig {
    // SAN content is irrelevant to the tests (clients connect with GIT_SSL_NO_VERIFY),
    // but keep it honest for 127.0.0.1 anyway.
    let certified =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
            .expect("generate self-signed cert");
    let cert: CertificateDer<'static> = certified.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
    ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("server cert")
}

fn handle_connection(
    stream: TcpStream,
    tls_config: Arc<ServerConfig>,
    repo: &Path,
    mount: &str,
    requests: &Mutex<Vec<RecordedRequest>>,
    concurrency: &Concurrency,
    options: &FixtureOptions,
    delay_spent: &AtomicBool,
    gate_spent: &AtomicBool,
) -> std::io::Result<()> {
    // macOS: sockets accepted from a nonblocking listener inherit O_NONBLOCK — undo it.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let conn = ServerConnection::new(tls_config).map_err(std::io::Error::other)?;
    let mut tls = StreamOwned::new(conn, stream);

    // Request head: request line + headers, up to the blank line.
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        let mut tmp = [0u8; 4096];
        let n = tls.read(&mut tmp)?;
        if n == 0 {
            return Ok(()); // client went away before completing a request
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let authorization = headers
        .get("authorization")
        .filter(|value| !value.trim().is_empty())
        .cloned();

    requests
        .lock()
        .expect("requests lock")
        .push(RecordedRequest {
            method: method.clone(),
            target: target.clone(),
            authorization: authorization.clone(),
        });

    let expects_continue = headers
        .get("expect")
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"));

    // NIP-98-style gate: EVERY smart endpoint needs Authorization; challenge otherwise.
    // With Expect: 100-continue no body is in flight yet, so refuse immediately; else
    // drain the body first so the client can read the 401 without a connection reset.
    if authorization.is_none() {
        if !expects_continue {
            let _ = read_body(&mut tls, &headers, &buf[head_end + 4..]);
        }
        return respond(
            &mut tls,
            "401 Unauthorized",
            &["WWW-Authenticate: Basic realm=\"maxplayer-relay-git-fixture\""],
            "text/plain",
            b"authorization required\n",
        );
    }

    // From here on this server is WORKING on an authorized request; the count stands until the
    // response is finished, however this function leaves.
    let _in_flight = InFlight::enter(concurrency);

    if expects_continue {
        tls.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        tls.flush()?;
    }
    let body = read_body(&mut tls, &headers, &buf[head_end + 4..])?;

    // Held AFTER the request (and its Authorization) was recorded, so the recording timestamps the
    // token as minted, and whatever the client sends next is genuinely later.
    if let Some(delay) = options.first_response_delay {
        if !delay_spent.swap(true, Ordering::SeqCst) {
            std::thread::sleep(delay);
        }
    }

    // Same position, but held until the test says so rather than for a guessed duration.
    if let Some(gate) = &options.hold_first_request {
        if !gate_spent.swap(true, Ordering::SeqCst) {
            gate.park();
        }
    }

    if let Some(location) = &options.redirect_to {
        let header = format!("Location: {location}");
        return respond(
            &mut tls,
            "302 Found",
            &[header.as_str()],
            "text/plain",
            b"moved\n",
        );
    }

    // Smart-HTTP v0/v2: pass the client's Git-Protocol offer through to the backend.
    let git_protocol = headers.get("git-protocol").cloned();
    route(
        &mut tls,
        repo,
        mount,
        &method,
        &target,
        git_protocol.as_deref(),
        &body,
    )
}

fn route(
    tls: &mut TlsStream,
    repo: &Path,
    mount: &str,
    method: &str,
    target: &str,
    git_protocol: Option<&str>,
    body: &[u8],
) -> std::io::Result<()> {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let Some(rest) = path.strip_prefix(mount) else {
        return respond(tls, "404 Not Found", &[], "text/plain", b"unknown repo\n");
    };
    match (method, rest) {
        ("GET", "/info/refs") => {
            match query.and_then(|q| q.strip_prefix("service=")) {
                Some(service @ ("git-upload-pack" | "git-receive-pack")) => {
                    advertise(tls, repo, service, git_protocol)
                }
                // Dumb-protocol fallback is not served: smart + authed only.
                _ => respond(
                    tls,
                    "403 Forbidden",
                    &[],
                    "text/plain",
                    b"smart service required\n",
                ),
            }
        }
        ("POST", "/git-upload-pack") => rpc(tls, repo, "git-upload-pack", git_protocol, body),
        ("POST", "/git-receive-pack") => rpc(tls, repo, "git-receive-pack", git_protocol, body),
        _ => respond(tls, "404 Not Found", &[], "text/plain", b"unrouted\n"),
    }
}

/// `GET …/info/refs?service=<service>` → pkt-line service prelude + ref advertisement
/// from the real backend (`git upload-pack|receive-pack --stateless-rpc --advertise-refs`).
fn advertise(
    tls: &mut TlsStream,
    repo: &Path,
    service: &str,
    git_protocol: Option<&str>,
) -> std::io::Result<()> {
    let subcommand = service.strip_prefix("git-").expect("git- prefixed service");
    let mut cmd = Command::new("git");
    cmd.arg(subcommand)
        .arg("--stateless-rpc")
        .arg("--advertise-refs")
        .arg(repo)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(protocol) = git_protocol {
        cmd.env("GIT_PROTOCOL", protocol);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return respond(
            tls,
            "500 Internal Server Error",
            &[],
            "text/plain",
            b"backend failed\n",
        );
    }
    let mut payload = pkt_line(&format!("# service={service}\n"));
    payload.extend_from_slice(b"0000");
    payload.extend_from_slice(&out.stdout);
    respond(
        tls,
        "200 OK",
        &[],
        &format!("application/x-{service}-advertisement"),
        &payload,
    )
}

/// `POST …/<service>` → body piped into the stateless-rpc backend, stdout streamed back.
fn rpc(
    tls: &mut TlsStream,
    repo: &Path,
    service: &str,
    git_protocol: Option<&str>,
    body: &[u8],
) -> std::io::Result<()> {
    let subcommand = service.strip_prefix("git-").expect("git- prefixed service");
    let mut cmd = Command::new("git");
    cmd.arg(subcommand)
        .arg("--stateless-rpc")
        .arg(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(protocol) = git_protocol {
        cmd.env("GIT_PROTOCOL", protocol);
    }
    let mut child = cmd.spawn()?;
    child.stdin.take().expect("piped stdin").write_all(body)?; // drop closes the pipe → backend sees EOF
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return respond(
            tls,
            "500 Internal Server Error",
            &[],
            "text/plain",
            b"backend failed\n",
        );
    }
    respond(
        tls,
        "200 OK",
        &[],
        &format!("application/x-{service}-result"),
        &out.stdout,
    )
}

fn respond(
    tls: &mut TlsStream,
    status: &str,
    extra_headers: &[&str],
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n",
        body.len()
    );
    for header in extra_headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    tls.write_all(head.as_bytes())?;
    tls.write_all(body)?;
    tls.conn.send_close_notify();
    tls.flush()?;
    Ok(())
}

/// Read the request body per Content-Length or chunked transfer coding; `pending` is
/// whatever body prefix arrived with the header read.
fn read_body(
    tls: &mut TlsStream,
    headers: &HashMap<String, String>,
    pending: &[u8],
) -> std::io::Result<Vec<u8>> {
    let mut body = pending.to_vec();
    if let Some(len) = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
    {
        while body.len() < len {
            let mut tmp = [0u8; 8192];
            let n = tls.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(len);
        Ok(body)
    } else if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        dechunk(tls, body)
    } else {
        Ok(Vec::new()) // GET / bodiless request
    }
}

fn dechunk(tls: &mut TlsStream, mut pending: Vec<u8>) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let size_line = read_crlf_line(tls, &mut pending)?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| std::io::Error::other("bad chunk size"))?;
        if size == 0 {
            let _ = read_crlf_line(tls, &mut pending); // trailer section terminator
            return Ok(out);
        }
        while pending.len() < size + 2 {
            let mut tmp = [0u8; 8192];
            let n = tls.read(&mut tmp)?;
            if n == 0 {
                return Err(std::io::Error::other("eof inside chunk"));
            }
            pending.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&pending[..size]);
        pending.drain(..size + 2); // chunk data + CRLF
    }
}

fn read_crlf_line(tls: &mut TlsStream, pending: &mut Vec<u8>) -> std::io::Result<String> {
    loop {
        if let Some(pos) = find(pending, b"\r\n") {
            let line = String::from_utf8_lossy(&pending[..pos]).into_owned();
            pending.drain(..pos + 2);
            return Ok(line);
        }
        let mut tmp = [0u8; 1024];
        let n = tls.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::other("eof inside chunked body"));
        }
        pending.extend_from_slice(&tmp[..n]);
    }
}

fn pkt_line(content: &str) -> Vec<u8> {
    format!("{:04x}{content}", content.len() + 4).into_bytes()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
