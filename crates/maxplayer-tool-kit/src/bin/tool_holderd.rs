//! `tool-holderd` — the seller-level tool holder.
//!
//! Lifecycle, stated plainly because this is the correction that shaped the daemon:
//!
//! * The holder enrols the tool **once**, at startup, if it is not already enrolled.
//! * The tool is then available for as long as this process runs.
//! * No award, payment, job start or job completion opens, closes, renews or revokes anything.
//!   Finishing a job does not log the tool out; the next job of the same seller finds the same
//!   session already live.
//! * Stopping the daemon takes the tool away. That is the only thing that does.
//!
//! Jobs are *addressed*, not entitled. Attaching a job creates a per-job socket and records
//! that job's directory, so the holder knows which directory a connection's paths resolve in.
//! It mints nothing, checks no eligibility, meters nothing and expires nothing.
//!
//! The credential never enters a job container: `vendor-cli` runs as a child of **this**
//! process, with a cleared environment pointing at the holder's private home. A job container
//! is given its own socket and its own directory, and nothing else.

use maxplayer_tool_kit::config::{ParamKind, SellerToolConfig};
use maxplayer_tool_kit::proto::{self, RpcRequest, RpcResponse};
use maxplayer_tool_kit::safeio;
use maxplayer_tool_kit::validate::{validate_call, CallArg};
use maxplayer_tool_kit::Health;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

struct JobSlot {
    root: PathBuf,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
}

struct Holder {
    cfg: SellerToolConfig,
    vendor_home: PathBuf,
    vendor_cli: PathBuf,
    vendor_base_url: String,
    credential_file: Option<PathBuf>,
    runtime: PathBuf,
    health: Mutex<Health>,
    /// True when startup found an existing session and skipped login. This is the fact the
    /// persistence evidence turns on, so the daemon reports it rather than inferring it later.
    resumed_existing_session: bool,
    enrollments_this_process: AtomicU64,
    calls_served: AtomicU64,
    /// Monotonic sequence for per-call staging directory names, so two concurrent calls of one
    /// job never collide on a staging path.
    staging_seq: AtomicU64,
    started_at: SystemTime,
    jobs: Mutex<BTreeMap<String, JobSlot>>,
}

/// The seller's tool never reads or writes a path a job can influence. Instead the holder copies
/// each input into this holder-private directory, runs the tool against it, and publishes outputs
/// from it. The directory is 0700, lives under the holder runtime (never mounted into a job
/// container), and is removed when this guard drops — on success, on rejection, or on error.
struct Staging {
    dir: PathBuf,
}

impl Staging {
    fn create(runtime: &Path, job_id: &str, seq: u64) -> std::io::Result<Self> {
        let parent = runtime.join("staging");
        std::fs::create_dir_all(&parent)?;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
        let dir = parent.join(format!("{job_id}-{seq}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(Staging { dir })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Largest input the holder will stage for one call. It matches the fake vendor's HTTP body cap,
/// so a larger input would be refused downstream anyway; refusing it here keeps the holder from
/// copying an unbounded file first.
const MAX_INPUT_BYTES: u64 = 1 << 20;

/// Copy an opened input descriptor into a staged file, refusing anything over the ingest bound.
/// The source is the descriptor `safeio::open_input` returned, so these are exactly the bytes that
/// were opened no-follow — not a re-read of a name that could have changed.
fn stage_input(src: &mut std::fs::File, dest: &Path, max: u64) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dest)
        .map_err(|e| format!("cannot stage input: {e}"))?;
    // Read one byte past the ceiling so an at-the-limit file is accepted and an over-limit one is
    // caught without reading it all.
    let copied = std::io::copy(&mut src.take(max + 1), &mut out).map_err(|e| format!("cannot stage input: {e}"))?;
    if copied > max {
        return Err(format!("input exceeds the {max}-byte ingest bound"));
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cfg_path = req(&args, "--config");
    let state_dir = PathBuf::from(req(&args, "--state"));
    let runtime = PathBuf::from(req(&args, "--runtime"));
    let vendor_cli = PathBuf::from(flag(&args, "--vendor-cli").unwrap_or_else(|| "vendor-cli".into()));
    let credential_file = flag(&args, "--credential-file").map(PathBuf::from);

    let cfg = SellerToolConfig::load(Path::new(&cfg_path)).unwrap_or_else(|e| {
        eprintln!("tool-holderd: {e}");
        std::process::exit(2);
    });
    let vendor_base_url = flag(&args, "--vendor-base-url").unwrap_or_else(|| cfg.vendor_base_url.clone());

    // Holder-private state, 0700. The vendor home lives inside it and is never mounted into a
    // job container.
    private_dir(&state_dir).unwrap_or_else(|e| fatal(&format!("state dir {}: {e}", state_dir.display())));
    private_dir(&runtime).unwrap_or_else(|e| fatal(&format!("runtime dir {}: {e}", runtime.display())));
    let jobs_sock_dir = runtime.join("jobs");
    private_dir(&jobs_sock_dir).unwrap_or_else(|e| fatal(&format!("jobs socket dir: {e}")));
    let vendor_home = state_dir.join("vendor-home");
    private_dir(&vendor_home).unwrap_or_else(|e| fatal(&format!("vendor home: {e}")));

    // Enrolment: once. An existing session is reused, which is exactly what makes the tool
    // survive a job ending and the daemon restarting.
    let already = vendor_home.join("auth.json").exists();
    let mut enrolments = 0u64;
    if !already {
        let Some(cred) = credential_file.clone() else {
            fatal("not enrolled and no --credential-file given");
        };
        let out = Command::new(&vendor_cli)
            .arg("login")
            .arg("--credential-file")
            .arg(&cred)
            .env_clear()
            .env("VENDOR_CLI_HOME", &vendor_home)
            .env("VENDOR_CLI_BASE_URL", &vendor_base_url)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| fatal(&format!("cannot run {}: {e}", vendor_cli.display())));
        if !out.status.success() {
            fatal(&format!(
                "enrolment failed ({}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        enrolments = 1;
        println!("tool-holderd: enrolled {} (first start)", cfg.seller_id);
    } else {
        println!("tool-holderd: existing session found; not logging in again");
    }

    let holder = Arc::new(Holder {
        cfg,
        vendor_home,
        vendor_cli,
        vendor_base_url,
        credential_file,
        runtime: runtime.clone(),
        health: Mutex::new(Health::Unhealthy("not probed yet".into())),
        resumed_existing_session: already,
        enrollments_this_process: AtomicU64::new(enrolments),
        calls_served: AtomicU64::new(0),
        staging_seq: AtomicU64::new(0),
        started_at: SystemTime::now(),
        jobs: Mutex::new(BTreeMap::new()),
    });

    // Probe once at startup so `status` is meaningful before any job runs.
    holder.probe_health();

    let control_path = runtime.join("holder.sock");
    let control = bind_private(&control_path).unwrap_or_else(|e| fatal(&e));
    println!("tool-holderd: control endpoint {}", control_path.display());
    println!(
        "tool-holderd: offering {:?} with {} operation(s), available while this process runs",
        holder.cfg.offering,
        holder.cfg.operations.len()
    );
    let _ = std::io::stdout().flush();

    for conn in control.incoming() {
        let Ok(conn) = conn else { continue };
        let holder = Arc::clone(&holder);
        // Control connections are handled inline: they are the seller's own, few, and ordered.
        if let Err(e) = holder.serve_conn(conn, None) {
            eprintln!("tool-holderd: control connection: {e}");
        }
    }
}

impl Holder {
    /// Serve one connection. `job` is `None` for the seller's control socket, or the job id when
    /// the connection arrived on a per-job socket — the job's identity comes from the listener
    /// it reached, never from the request body.
    fn serve_conn(self: &Arc<Self>, conn: UnixStream, job: Option<String>) -> std::io::Result<()> {
        let mut writer = conn.try_clone()?;
        let reader = BufReader::new(conn);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<RpcRequest>(&line) {
                Ok(req) => self.dispatch(req, job.as_deref()),
                Err(e) => RpcResponse::err(None, proto::CODE_INVALID_PARAMS, format!("malformed request: {e}")),
            };
            writer.write_all(resp.to_line().as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        Ok(())
    }

    fn dispatch(self: &Arc<Self>, req: RpcRequest, job: Option<&str>) -> RpcResponse {
        let id = req.id.clone();
        match req.method.as_str() {
            proto::METHOD_INITIALIZE => RpcResponse::ok(
                id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "maxplayer-tool-kit-holder", "version": env!("CARGO_PKG_VERSION")},
                }),
            ),

            // The same list for every job of this seller. It is seller configuration.
            proto::METHOD_TOOLS_LIST => RpcResponse::ok(id, json!({"tools": self.tool_descriptors()})),

            proto::METHOD_TOOLS_CALL => match job {
                Some(job_id) => self.tools_call(id, req.params, job_id),
                None => RpcResponse::err(
                    id,
                    proto::CODE_INVALID_PARAMS,
                    "tools/call must arrive on a job endpoint, not the control endpoint",
                ),
            },

            proto::METHOD_HEALTH => {
                let health = self.probe_health();
                RpcResponse::ok(id, json!({"health": health, "healthy": health.is_healthy()}))
            }

            proto::METHOD_STATUS => {
                let health = self.health.lock().map(|h| h.clone()).unwrap_or(Health::Unhealthy("state poisoned".into()));
                let jobs: Vec<Value> = self
                    .jobs
                    .lock()
                    .map(|j| {
                        j.iter()
                            .map(|(id, slot)| json!({"job_id": id, "root": slot.root, "socket": slot.socket}))
                            .collect()
                    })
                    .unwrap_or_default();
                RpcResponse::ok(
                    id,
                    json!({
                        "seller_id": self.cfg.seller_id,
                        "offering": self.cfg.offering,
                        "operations": self.cfg.operations.iter().map(|o| &o.name).collect::<Vec<_>>(),
                        "health": health,
                        "healthy": health.is_healthy(),
                        "resumed_existing_session": self.resumed_existing_session,
                        "enrollments_this_process": self.enrollments_this_process.load(Ordering::SeqCst),
                        "calls_served": self.calls_served.load(Ordering::SeqCst),
                        "uptime_secs": self.started_at.elapsed().map(|d| d.as_secs()).unwrap_or(0),
                        "attached_jobs": jobs,
                    }),
                )
            }

            // Seller-side wiring only. Creates a socket and records a directory; mints nothing.
            "holder/attach_job" if job.is_none() => self.attach_job(id, req.params),
            "holder/detach_job" if job.is_none() => self.detach_job(id, req.params),

            "holder/reenroll" if job.is_none() => self.reenroll(id),

            proto::METHOD_SHUTDOWN if job.is_none() => {
                self.cleanup();
                println!("tool-holderd: stopping; tool is no longer available");
                let _ = std::io::stdout().flush();
                // Answer before exiting so the caller sees a clean stop.
                let out = RpcResponse::ok(id, json!({"stopping": true}));
                let line = out.to_line();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(120));
                    std::process::exit(0);
                });
                let _ = line;
                out
            }

            other => RpcResponse::err(id, proto::CODE_METHOD_NOT_FOUND, format!("unknown method {other:?}")),
        }
    }

    fn tool_descriptors(&self) -> Vec<Value> {
        self.cfg
            .operations
            .iter()
            .map(|op| {
                let mut props = Map::new();
                let mut required = Vec::new();
                for p in &op.params {
                    let schema = match &p.kind {
                        ParamKind::Text { max_len } => {
                            json!({"type": "string", "maxLength": max_len, "description": "literal text"})
                        }
                        ParamKind::Choice { choices } => json!({"type": "string", "enum": choices}),
                        ParamKind::JobInputFile => json!({
                            "type": "string",
                            "description": "path relative to this job's own directory",
                        }),
                        ParamKind::JobOutputFile => json!({
                            "type": "string",
                            "description": "output path relative to this job's own directory",
                        }),
                    };
                    props.insert(p.name.clone(), schema);
                    required.push(p.name.clone());
                }
                json!({
                    "name": op.name,
                    "description": op.description,
                    "inputSchema": {"type": "object", "properties": props, "required": required, "additionalProperties": false},
                })
            })
            .collect()
    }

    fn tools_call(self: &Arc<Self>, id: Option<Value>, params: Value, job_id: &str) -> RpcResponse {
        let Some(name) = params["name"].as_str() else {
            return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "params.name is required");
        };
        let args = match &params["arguments"] {
            Value::Object(m) => m.clone(),
            Value::Null => Map::new(),
            _ => return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "params.arguments must be an object"),
        };

        // A job may not nominate its own directory. Refuse loudly rather than ignoring it, so a
        // caller never believes it chose one.
        for reserved in ["job_id", "job_root", "cwd", "home"] {
            if args.contains_key(reserved) {
                return RpcResponse::err(
                    id,
                    proto::CODE_REJECTED,
                    format!("{reserved:?} is not a parameter; the job's directory is fixed by the seller"),
                );
            }
        }

        let root = match self.jobs.lock() {
            Ok(j) => match j.get(job_id) {
                Some(slot) => slot.root.clone(),
                None => return RpcResponse::err(id, proto::CODE_INTERNAL, "job is no longer attached"),
            },
            Err(_) => return RpcResponse::err(id, proto::CODE_INTERNAL, "state poisoned"),
        };

        let arg_map: BTreeMap<String, Value> = args.into_iter().collect();
        let call = match validate_call(&self.cfg, name, &arg_map, &root) {
            Ok(c) => c,
            Err(reject) => return RpcResponse::err(id, proto::CODE_REJECTED, reject.to_string()),
        };

        // Consume the call race-safely. The whole point of F2: the CLI never touches a path the
        // job can influence. The holder opens each input itself, following no symlink, copies it
        // into a holder-private staging directory the job cannot reach, and gives the CLI only
        // staged paths. Output is produced in staging and published back with an equally
        // no-follow create. A `Staging` guard removes the directory on every exit path.
        let seq = self.staging_seq.fetch_add(1, Ordering::SeqCst);
        let staging = match Staging::create(&self.runtime, job_id, seq) {
            Ok(s) => s,
            Err(e) => return RpcResponse::err(id, proto::CODE_INTERNAL, format!("staging: {e}")),
        };

        let mut argv: Vec<String> = Vec::new();
        // (staged output path, job-relative destination) pairs, published only after the CLI
        // succeeds and only through a no-follow create.
        let mut pending_outputs: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut out_idx = 0usize;
        let mut in_idx = 0usize;

        for arg in &call.args {
            match arg {
                CallArg::Literal { flag, value } => {
                    argv.push(flag.clone());
                    argv.push(value.clone());
                }
                CallArg::Input { flag, rel } => {
                    // Open the real input no-follow, then stage its bytes. A symlink or a
                    // swapped parent is refused here, on the descriptor that is actually read.
                    let mut src = match safeio::open_input(&root, rel, "input") {
                        Ok(f) => f,
                        Err(reject) => return RpcResponse::err(id, proto::CODE_REJECTED, reject.to_string()),
                    };
                    let staged = staging.path(&format!("in-{in_idx}"));
                    in_idx += 1;
                    if let Err(e) = stage_input(&mut src, &staged, MAX_INPUT_BYTES) {
                        return RpcResponse::err(id, proto::CODE_REJECTED, e);
                    }
                    argv.push(flag.clone());
                    argv.push(staged.to_string_lossy().into_owned());
                }
                CallArg::Output { flag, rel } => {
                    let staged = staging.path(&format!("out-{out_idx}"));
                    out_idx += 1;
                    argv.push(flag.clone());
                    argv.push(staged.to_string_lossy().into_owned());
                    pending_outputs.push((staged, rel.clone()));
                }
            }
        }

        // Fixed program, fixed subcommand, staged operands, cleared environment, cwd pinned to the
        // holder-private staging directory. No shell anywhere on this path, and no job-writable
        // path in the child's argv or cwd.
        let out = Command::new(&self.vendor_cli)
            .arg(&call.subcommand)
            .args(&argv)
            .env_clear()
            .env("VENDOR_CLI_HOME", &self.vendor_home)
            .env("VENDOR_CLI_BASE_URL", &self.vendor_base_url)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .current_dir(&staging.dir)
            .stdin(Stdio::null())
            .output();

        let out = match out {
            Ok(o) => o,
            Err(e) => return RpcResponse::err(id, proto::CODE_INTERNAL, format!("cannot run tool: {e}")),
        };

        // Exit code 3 is the tool's "vendor rejected the stored session". That is a holder
        // health fact, not a bad request, and it must be visible to the seller.
        if out.status.code() == Some(3) {
            self.set_health(Health::Unhealthy("vendor rejected the stored session".into()));
            return RpcResponse::err(id, proto::CODE_UNHEALTHY, "tool is not authenticated; holder is unhealthy");
        }
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let msg = if msg.len() > 400 { format!("{}…", &msg[..400]) } else { msg };
            return RpcResponse::err(id, proto::CODE_TOOL_FAILED, format!("tool failed: {msg}"));
        }

        // Publish each staged output back into the job directory. The ceiling is enforced on the
        // staged file, before anything is written into the job's directory, so an oversized
        // result never lands there at all. The destination is created no-follow, so a symlink a
        // job planted at the output name is refused rather than written through.
        let mut outputs: Vec<Value> = Vec::new();
        for (staged, rel) in &pending_outputs {
            let bytes = std::fs::metadata(staged).map(|m| m.len()).unwrap_or(0);
            if bytes as usize > call.max_output_bytes {
                return RpcResponse::err(
                    id,
                    proto::CODE_TOOL_FAILED,
                    format!("output exceeded the configured ceiling of {} bytes; not published", call.max_output_bytes),
                );
            }
            let data = match std::fs::read(staged) {
                Ok(d) => d,
                Err(e) => return RpcResponse::err(id, proto::CODE_INTERNAL, format!("read staged output: {e}")),
            };
            let mut dest = match safeio::create_output(&root, rel, "output") {
                Ok(f) => f,
                Err(reject) => return RpcResponse::err(id, proto::CODE_REJECTED, reject.to_string()),
            };
            if let Err(e) = dest.write_all(&data) {
                return RpcResponse::err(id, proto::CODE_INTERNAL, format!("write output: {e}"));
            }
            // Report the path relative to the job's own root: the job has no business learning the
            // holder's filesystem layout.
            outputs.push(json!({"path": rel, "bytes": bytes}));
        }

        self.calls_served.fetch_add(1, Ordering::SeqCst);
        self.set_health(Health::Healthy);
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();

        RpcResponse::ok(
            id,
            json!({
                "content": [{"type": "text", "text": stdout}],
                "isError": false,
                "operation": call.operation,
                "outputs": outputs,
            }),
        )
    }

    fn attach_job(self: &Arc<Self>, id: Option<Value>, params: Value) -> RpcResponse {
        let Some(job_id) = params["job_id"].as_str() else {
            return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "job_id is required");
        };
        if !maxplayer_tool_kit::config::is_plain_ident(job_id) {
            return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "job_id must be [a-z0-9_-]");
        }
        let Some(root) = params["job_root"].as_str() else {
            return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "job_root is required");
        };
        let root = match Path::new(root).canonicalize() {
            Ok(r) if r.is_dir() => r,
            _ => return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "job_root must be an existing directory"),
        };
        // The holder's own state must never be reachable as a job directory.
        if self.vendor_home.starts_with(&root) || root.starts_with(&self.vendor_home) {
            return RpcResponse::err(id, proto::CODE_REJECTED, "job_root may not contain or equal the holder's private state");
        }

        // Each job's socket gets its own directory, so exactly one endpoint can be handed to a
        // job container without handing over the directory that holds every other job's. A flat
        // `jobs/<id>.sock` layout would make per-job isolation unexpressible as a mount.
        let dir = self.runtime.join("jobs").join(job_id);
        if let Err(e) = private_dir(&dir) {
            return RpcResponse::err(id, proto::CODE_INTERNAL, format!("job socket dir: {e}"));
        }
        let sock = dir.join("job.sock");
        let listener = match bind_private(&sock) {
            Ok(l) => l,
            Err(e) => return RpcResponse::err(id, proto::CODE_INTERNAL, e),
        };

        let stop = Arc::new(AtomicBool::new(false));
        {
            let mut jobs = match self.jobs.lock() {
                Ok(j) => j,
                Err(_) => return RpcResponse::err(id, proto::CODE_INTERNAL, "state poisoned"),
            };
            jobs.insert(
                job_id.to_string(),
                JobSlot { root: root.clone(), socket: sock.clone(), stop: Arc::clone(&stop) },
            );
        }

        let holder = Arc::clone(self);
        let job_owned = job_id.to_string();
        let sock_owned = sock.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let holder2 = Arc::clone(&holder);
                let job2 = job_owned.clone();
                std::thread::spawn(move || {
                    if let Err(e) = holder2.serve_conn(conn, Some(job2)) {
                        eprintln!("tool-holderd: job connection: {e}");
                    }
                });
            }
            let _ = std::fs::remove_file(&sock_owned);
        });

        RpcResponse::ok(
            id,
            json!({
                "job_id": job_id,
                "socket": sock,
                "job_root": root,
                "note": "addressing and isolation only; no grant, no entitlement, no expiry",
            }),
        )
    }

    fn detach_job(self: &Arc<Self>, id: Option<Value>, params: Value) -> RpcResponse {
        let Some(job_id) = params["job_id"].as_str() else {
            return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "job_id is required");
        };
        let slot = match self.jobs.lock() {
            Ok(mut j) => j.remove(job_id),
            Err(_) => return RpcResponse::err(id, proto::CODE_INTERNAL, "state poisoned"),
        };
        let Some(slot) = slot else {
            return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "no such attached job");
        };
        slot.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop so the thread notices the flag and removes its socket.
        let _ = UnixStream::connect(&slot.socket);
        let _ = std::fs::remove_file(&slot.socket);

        // The tool is untouched: still enrolled, still healthy, still serving other jobs. This
        // is the assertion the correction turns on, so it is stated in the reply.
        let health = self.health.lock().map(|h| h.clone()).unwrap_or(Health::Healthy);
        RpcResponse::ok(
            id,
            json!({
                "job_id": job_id,
                "detached": true,
                "tool_still_enrolled": true,
                "health": health,
                "note": "job ended; the tool was not logged out and no session was closed",
            }),
        )
    }

    fn reenroll(self: &Arc<Self>, id: Option<Value>) -> RpcResponse {
        let Some(cred) = self.credential_file.clone() else {
            return RpcResponse::err(id, proto::CODE_INVALID_PARAMS, "no credential file configured");
        };
        let out = Command::new(&self.vendor_cli)
            .arg("login")
            .arg("--credential-file")
            .arg(&cred)
            .env_clear()
            .env("VENDOR_CLI_HOME", &self.vendor_home)
            .env("VENDOR_CLI_BASE_URL", &self.vendor_base_url)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => {
                self.enrollments_this_process.fetch_add(1, Ordering::SeqCst);
                let health = self.probe_health();
                RpcResponse::ok(id, json!({"reenrolled": true, "health": health}))
            }
            Ok(o) => RpcResponse::err(
                id,
                proto::CODE_UNHEALTHY,
                format!("re-enrolment failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
            ),
            Err(e) => RpcResponse::err(id, proto::CODE_INTERNAL, format!("cannot run tool: {e}")),
        }
    }

    /// Ask the tool, not ourselves. Health is a fact about the vendor session.
    fn probe_health(&self) -> Health {
        let out = Command::new(&self.vendor_cli)
            .arg("health")
            .env_clear()
            .env("VENDOR_CLI_HOME", &self.vendor_home)
            .env("VENDOR_CLI_BASE_URL", &self.vendor_base_url)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .output();
        let health = match out {
            Ok(o) if o.status.success() => Health::Healthy,
            Ok(o) if o.status.code() == Some(3) => {
                Health::Unhealthy("vendor rejected the stored session".into())
            }
            Ok(o) => Health::Unhealthy(format!(
                "tool health check failed ({})",
                o.status.code().unwrap_or(-1)
            )),
            Err(e) => Health::Unhealthy(format!("cannot run tool: {e}")),
        };
        self.set_health(health.clone());
        health
    }

    fn set_health(&self, health: Health) {
        if let Ok(mut h) = self.health.lock() {
            *h = health;
        }
    }

    fn cleanup(&self) {
        if let Ok(jobs) = self.jobs.lock() {
            for slot in jobs.values() {
                slot.stop.store(true, Ordering::SeqCst);
                let _ = UnixStream::connect(&slot.socket);
                let _ = std::fs::remove_file(&slot.socket);
            }
        }
        let _ = std::fs::remove_file(self.runtime.join("holder.sock"));
    }
}

/// 0700 directory, created restrictively.
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Bind a Unix socket reachable only by the seller's own uid.
///
/// The parent directory is already 0700, which is what actually closes the window between
/// `bind` and `set_permissions`.
fn bind_private(path: &Path) -> Result<UnixListener, String> {
    if path.exists() {
        // A live socket means a second daemon; a dead one is just litter from a hard stop.
        if UnixStream::connect(path).is_ok() {
            return Err(format!("{} is already served by a running daemon", path.display()));
        }
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path).map_err(|e| format!("bind {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(listener)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn req(args: &[String], name: &str) -> String {
    flag(args, name).unwrap_or_else(|| fatal(&format!("{name} <value> is required")))
}

fn fatal(msg: &str) -> ! {
    eprintln!("tool-holderd: {msg}");
    std::process::exit(2)
}
