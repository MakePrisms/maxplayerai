//! Test-only git-over-HTTPS fixture with a NIP-98-style auth gate.
//!
//! Serves a local repository over the git smart-HTTP protocol on 127.0.0.1, speaking
//! HTTPS through an in-memory self-signed cert (rustls + rcgen — the seller transport
//! allowlist refuses plain `http://`, so a faithful fixture must be `https://`). Every
//! smart-transport request (`info/refs`, `git-upload-pack`, `git-receive-pack`) requires
//! an `Authorization` header — mirroring mobee relay-git, which gates reads AND writes
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

/// Auth-gated smart-HTTP git server bound to `127.0.0.1:<ephemeral>`.
pub struct GitHttpAuthServer {
    addr: SocketAddr,
    mount: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl GitHttpAuthServer {
    /// Serve `repo` (a worktree or bare repo — anything `git upload-pack` accepts) at
    /// `https://127.0.0.1:<port><mount>`; `mount` is the URL path of the repo, e.g.
    /// `/git/<owner>/base.git` for the relay-git shape.
    pub fn spawn(repo: &Path, mount: &str) -> Self {
        let tls_config = Arc::new(self_signed_tls_config());
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fixture listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("listener addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let repo: PathBuf = repo.to_path_buf();
        let mount_bg = mount.to_owned();
        let requests_bg = Arc::clone(&requests);
        let shutdown_bg = Arc::clone(&shutdown);
        let accept_thread = std::thread::spawn(move || {
            while !shutdown_bg.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        let tls_config = Arc::clone(&tls_config);
                        let repo = repo.clone();
                        let mount = mount_bg.clone();
                        let requests = Arc::clone(&requests_bg);
                        std::thread::spawn(move || {
                            let _ = handle_connection(stream, tls_config, &repo, &mount, &requests);
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
}

impl Drop for GitHttpAuthServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

fn self_signed_tls_config() -> ServerConfig {
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

    requests.lock().expect("requests lock").push(RecordedRequest {
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
            &["WWW-Authenticate: Basic realm=\"mobee-relay-git-fixture\""],
            "text/plain",
            b"authorization required\n",
        );
    }

    if expects_continue {
        tls.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        tls.flush()?;
    }
    let body = read_body(&mut tls, &headers, &buf[head_end + 4..])?;

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
        return respond(tls, "500 Internal Server Error", &[], "text/plain", b"backend failed\n");
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
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(body)?; // drop closes the pipe → backend sees EOF
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return respond(tls, "500 Internal Server Error", &[], "text/plain", b"backend failed\n");
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
