//! Test harness: a real fake vendor on an ephemeral port, a real holder daemon, real Unix
//! sockets, real child processes. Nothing here is mocked in-process — the properties under test
//! are process, filesystem and socket properties, and an in-process double would prove none of
//! them.

#![allow(dead_code)]

use maxplayer_tool_kit::{client, http};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const VENDOR_SERVICE: &str = env!("CARGO_BIN_EXE_vendor-service");
pub const VENDOR_CLI: &str = env!("CARGO_BIN_EXE_vendor-cli");
pub const TOOL_HOLDERD: &str = env!("CARGO_BIN_EXE_tool-holderd");
pub const HOLDERCTL: &str = env!("CARGO_BIN_EXE_holderctl");
pub const MCP_BRIDGE: &str = env!("CARGO_BIN_EXE_tool-mcp-bridge");

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The synthetic credential. Generated per run, written only into a temp dir, never committed
/// and never passed on a command line.
const SYNTHETIC_CLIENT_ID: &str = "synthetic-seller-client";
const SYNTHETIC_CLIENT_SECRET: &str = "synthetic-fixture-secret-not-real";

pub struct Fixture {
    pub root: PathBuf,
    pub state: PathBuf,
    pub runtime: PathBuf,
    pub jobs_dir: PathBuf,
    pub config: PathBuf,
    pub credential: PathBuf,
    pub vendor_base_url: String,
    /// The program the holder runs as the vendor CLI. The real fake by default; a test can hand
    /// in a wrapper, for example one that makes the tool slow.
    pub vendor_cli: PathBuf,
    vendor: Option<Child>,
    holder: Option<Child>,
}

impl Fixture {
    pub fn start() -> Self {
        Self::start_configured(|_| {})
    }

    /// Start with the committed seller config, optionally patched. Used by the ceiling test,
    /// which needs a limit small enough to actually cross.
    pub fn start_configured(patch: impl FnOnce(&mut Value)) -> Self {
        Self::start_with(patch, Path::new(VENDOR_CLI))
    }

    /// Start with a patched config AND a different vendor CLI program. `vendor_cli` must be an
    /// executable the holder can run with a cleared environment.
    pub fn start_with(patch: impl FnOnce(&mut Value), vendor_cli: &Path) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        // Deliberately NOT `std::env::temp_dir()`. A Unix socket path is capped by `SUN_LEN`
        // (104 bytes on macOS, 108 on Linux), and macOS hands out temp directories like
        // `/var/folders/mc/_gm6z2qx19qf.../T/` that eat most of it — the control socket fits and
        // the per-job sockets do not, which is a confusing way to discover the limit. The
        // holder's runtime directory must therefore be short by construction, in tests and in
        // deployment alike.
        let root = PathBuf::from("/tmp/mtk").join(format!("{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create fixture root");

        let state = root.join("holder-state");
        let runtime = root.join("holder-run");
        let jobs_dir = root.join("jobs");
        std::fs::create_dir_all(&jobs_dir).expect("create jobs dir");

        // Credential file, generated now, 0600.
        let credential = root.join("synthetic-credential.json");
        write_private(
            &credential,
            json!({"client_id": SYNTHETIC_CLIENT_ID, "client_secret": SYNTHETIC_CLIENT_SECRET})
                .to_string()
                .as_bytes(),
        )
        .expect("write credential");

        // Vendor on port 0; learn the real port from its first stdout line.
        let mut vendor = Command::new(VENDOR_SERVICE)
            .arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--credential-file")
            .arg(&credential)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn vendor-service");
        let stdout = vendor.stdout.take().expect("vendor stdout");
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).expect("vendor banner");
        let addr = line
            .trim()
            .rsplit_once(' ')
            .map(|(_, a)| a.to_string())
            .expect("vendor banner carries an address");
        let vendor_base_url = format!("http://{addr}");

        // Seller config, taken from the committed fixture so tests exercise the real one.
        let config = root.join("seller-tool-config.json");
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/seller-tool-config.json");
        let raw = std::fs::read_to_string(&src).expect("read seller config fixture");
        let mut parsed: Value = serde_json::from_str(&raw).expect("seller config fixture is json");
        patch(&mut parsed);
        std::fs::write(&config, serde_json::to_vec_pretty(&parsed).expect("serialize config"))
            .expect("write seller config");

        let mut fx = Fixture {
            root,
            state,
            runtime,
            jobs_dir,
            config,
            credential,
            vendor_base_url,
            vendor_cli: vendor_cli.to_path_buf(),
            vendor: Some(vendor),
            holder: None,
        };
        fx.start_holder();
        fx
    }

    /// Start (or restart) the daemon against the same state directory. Restarting is how the
    /// suite shows a persisted session being reused rather than re-established.
    pub fn start_holder(&mut self) {
        assert!(self.holder.is_none(), "holder already running");
        let child = Command::new(TOOL_HOLDERD)
            .arg("--config")
            .arg(&self.config)
            .arg("--state")
            .arg(&self.state)
            .arg("--runtime")
            .arg(&self.runtime)
            .arg("--credential-file")
            .arg(&self.credential)
            .arg("--vendor-cli")
            .arg(&self.vendor_cli)
            .arg("--vendor-base-url")
            .arg(&self.vendor_base_url)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn tool-holderd");
        self.holder = Some(child);

        let sock = self.control_socket();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if sock.exists() && client::call(&sock, "holder/status", json!({})).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("holder control socket never became ready at {}", sock.display());
    }

    pub fn control_socket(&self) -> PathBuf {
        self.runtime.join("holder.sock")
    }

    /// One directory per job, holding that job's single socket. The demo mounts exactly this
    /// directory into the matching job container.
    pub fn job_socket(&self, job_id: &str) -> PathBuf {
        self.runtime.join("jobs").join(job_id).join("job.sock")
    }

    pub fn ctl(&self, method: &str, params: Value) -> Result<Value, String> {
        client::call(&self.control_socket(), method, params)
    }

    pub fn job_call(&self, job_id: &str, method: &str, params: Value) -> Result<Value, String> {
        client::call(&self.job_socket(job_id), method, params)
    }

    /// Create a job directory and attach it. Returns the job's own root.
    pub fn make_job(&self, job_id: &str) -> PathBuf {
        let root = self.jobs_dir.join(job_id);
        std::fs::create_dir_all(&root).expect("create job root");
        let res = self
            .ctl("holder/attach_job", json!({"job_id": job_id, "job_root": root}))
            .expect("attach job");
        assert_eq!(res["job_id"], json!(job_id));
        root
    }

    pub fn detach_job(&self, job_id: &str) -> Value {
        self.ctl("holder/detach_job", json!({"job_id": job_id})).expect("detach job")
    }

    /// Ask the *vendor* what happened. The independent observer for anything about calls.
    pub fn vendor_stats(&self) -> Value {
        let resp = http::request(&self.vendor_base_url, "GET", "/admin/stats", None, None)
            .expect("vendor stats");
        assert_eq!(resp.status, 200, "vendor stats should answer 200");
        serde_json::from_slice(&resp.body).expect("vendor stats json")
    }

    /// Revoke every live session, server side. Drives the auth-failure path without touching
    /// the holder, so the holder's reaction is a real observation.
    pub fn revoke_vendor_sessions(&self) -> u64 {
        let resp = http::request(&self.vendor_base_url, "POST", "/admin/revoke", None, Some(b"{}"))
            .expect("vendor revoke");
        assert_eq!(resp.status, 200);
        let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        body["revoked"].as_u64().unwrap_or(0)
    }

    pub fn holderctl(&self, args: &[&str]) -> (bool, String, String) {
        let out = Command::new(HOLDERCTL)
            .args(args)
            .arg("--socket")
            .arg(self.control_socket())
            .output()
            .expect("run holderctl");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    /// Drive the MCP stdio bridge exactly as an agent in a job container would: spawn it with
    /// only that job's socket in its environment, then speak newline-delimited JSON-RPC.
    pub fn mcp_session(&self, job_id: &str) -> McpSession {
        let mut child = Command::new(MCP_BRIDGE)
            .env_clear()
            .env("HOLDER_JOB_SOCKET", self.job_socket(job_id))
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn tool-mcp-bridge");
        let stdin = child.stdin.take().expect("bridge stdin");
        let stdout = BufReader::new(child.stdout.take().expect("bridge stdout"));
        McpSession { child, stdin, stdout, next_id: 1 }
    }

    pub fn stop_holder(&mut self) {
        if let Some(mut child) = self.holder.take() {
            let _ = self.ctl("holder/shutdown", json!({}));
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
    }

    /// Path to the holder's private session file. Tests assert about its mode and location;
    /// nothing reads its contents.
    pub fn auth_file(&self) -> PathBuf {
        self.state.join("vendor-home/auth.json")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop_holder();
        if let Some(mut vendor) = self.vendor.take() {
            let _ = http::request(&self.vendor_base_url, "POST", "/admin/shutdown", None, Some(b"{}"));
            std::thread::sleep(Duration::from_millis(80));
            let _ = vendor.kill();
            let _ = vendor.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub struct McpSession {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpSession {
    /// Send a request and read its response. Returns the whole JSON-RPC envelope so tests can
    /// assert on `error.code` as well as on `result`.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        writeln!(self.stdin, "{line}").expect("write mcp request");
        self.stdin.flush().expect("flush mcp request");
        let mut resp = String::new();
        self.stdout.read_line(&mut resp).expect("read mcp response");
        assert!(!resp.trim().is_empty(), "bridge returned an empty line for {method}");
        let parsed: Value = serde_json::from_str(resp.trim()).expect("mcp response json");
        assert_eq!(parsed["id"], json!(id), "response id must match the request");
        parsed
    }

    /// A notification must draw no reply at all. Verified by sending one, then a request, and
    /// checking the request's own id comes back — a stray reply would desynchronize this.
    pub fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        writeln!(self.stdin, "{line}").expect("write mcp notification");
        self.stdin.flush().expect("flush mcp notification");
    }

    pub fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        )
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

/// The synthetic secret, for the one test that greps a job directory for it.
pub fn synthetic_secret() -> &'static str {
    SYNTHETIC_CLIENT_SECRET
}

pub fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}
