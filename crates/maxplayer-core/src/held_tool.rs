//! The Holder route, daemon side: one vendor CLI held logged in, in a persistent holder container
//! the seller daemon supervises, and offered to every job over a per-job Unix socket.
//!
//! The model this implements, and the correction that shaped it (Petar, 2026-09-09): "the seller is
//! defined by its offering, there is no offering per job, it is per seller, tool should at all times
//! be active together with the seller daemon". So:
//!
//! * ONE holder container per seat, started at daemon boot, stopped at daemon stop. It runs
//!   `tool-holderd` from `crates/maxplayer-tool-kit` beside the vendor's own CLI, in an image the
//!   seller builds. It enrols once, or resumes the login it persisted in its state volume.
//! * Per job, the daemon ATTACHES: the holder creates `jobs/<job>/job.sock` in its runtime volume,
//!   and the job container mounts exactly that directory at `/run/holder/<server name>`. The agent
//!   reaches it through `tool-mcp-bridge --socket …`, baked into the sandbox image, as a stdio MCP
//!   server. Job end DETACHES the socket; the tool stays enrolled.
//! * Several held tools are several holders: one container, two volumes and one socket per tool,
//!   all named by the tool's `server_name`. A job gets one mount and one server entry per tool.
//! * The credential file and the holder's state never enter a job container. A job gets a socket and
//!   the seller-declared operations, and nothing else.
//!
//! Every docker interaction here is a blocking `docker` CLI call on the blocking pool, because the
//! seller node runs every awarded job as a `spawn_local` task on ONE thread. The daemon talks to
//! the holder through `docker exec … holderctl`, never through a host-side socket: a Unix socket on
//! a bind mount does not cross the Docker Desktop VM boundary, and the demo already chose this path.
//!
//! The pure argv builders are separate from the calls that run them, so the shape of every command
//! — what is mounted, as whom, with which flags — is unit-tested without a docker daemon.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::driver::{McpServer, McpServerStdio};
use crate::home::HeldToolConfig;
use crate::seller_exec::{ExtraMount, JobAttachments};

/// Where the seller-tool config JSON is mounted inside the holder (read-only).
pub const HOLDER_CONFIG_PATH: &str = "/etc/maxplayer/seller-tool-config.json";
/// Where the vendor credential is mounted inside the holder (read-only). Never mounted into a job.
pub const HOLDER_CREDENTIAL_PATH: &str = "/run/secrets/cred.json";
/// The holder's private state (the vendor login) — its state volume.
///
/// Deliberately a path NO holder image ships (the kit image creates `/var/lib/holder`, not this).
/// Measured on Docker Desktop 29: a named volume first mounted over a directory that EXISTS in the
/// image is initialized from that directory, and a `chown` of the volume root in that same first
/// container reports success and is then lost — the holder, running as the job uid, could never own
/// its volumes. A path the image lacks is created root-owned and empty, and the one-shot `chown` in
/// [`volume_init_argv`] holds.
pub const HOLDER_STATE_DIR: &str = "/var/lib/maxplayer-holder";
/// The holder's runtime (control socket, per-job socket directories, staging) — its runtime volume.
/// Same rule as [`HOLDER_STATE_DIR`]: a path no image ships.
pub const HOLDER_RUNTIME_DIR: &str = "/run/maxplayer-holder";
/// The holder's control socket, reached with `docker exec … holderctl --socket`.
pub const HOLDER_CONTROL_SOCKET: &str = "/run/maxplayer-holder/holder.sock";
/// Where the seat's job workdirs (`<home>/seller-jobs`) are mounted inside the holder, so the
/// holder can stage a job's inputs and publish its outputs under that job's own directory.
pub const HOLDER_JOBS_DIR: &str = "/srv/jobs";
/// Under which a job container mounts its socket directories, one per held tool:
/// `/run/holder/<server name>`.
pub const JOB_SOCKET_MOUNT_ROOT: &str = "/run/holder";
/// The socket bridge inside the sandbox image (`docker/maxplayer-sandbox/Dockerfile`).
pub const CONTAINER_TOOL_BRIDGE_BIN: &str = "/usr/local/bin/tool-mcp-bridge";

/// Where a job container mounts the socket directory of the tool named `server_name`.
pub fn job_socket_mount(server_name: &str) -> String {
    format!("{JOB_SOCKET_MOUNT_ROOT}/{server_name}")
}

/// The socket path inside the job container for the tool named `server_name` — what the bridge is
/// told with `--socket`.
pub fn job_socket_path(server_name: &str) -> String {
    format!("{}/job.sock", job_socket_mount(server_name))
}
/// The label every holder container carries, valued with the seat's pubkey hex, so a stale holder
/// can be attributed to the seat that leaked it.
pub const HOLDER_LABEL: &str = "maxplayer.held-tool.seat";

/// The file in the seat's home that says "this seat started holders". Written when holders start,
/// removed when a boot with no held tools reconciles and finds nothing left to own. It is what lets
/// a boot that left docker mode, or dropped `[sandbox]`, still remove the holders of its previous
/// configuration, without a `docker` call at the boot of every seat that never held a tool.
pub const HELD_TOOLS_MARKER: &str = "held-tools-started";

/// The marker's path under the seat's home directory.
pub fn marker_path(home_root: &Path) -> std::path::PathBuf {
    home_root.join(HELD_TOOLS_MARKER)
}

/// Record that `seat` starts holders for `server_names` now. Informational content; the file's
/// presence is the fact.
pub fn write_marker(home_root: &Path, seat: &str, server_names: &[String]) -> std::io::Result<()> {
    std::fs::write(
        marker_path(home_root),
        format!("seat {seat}\nheld tools: {}\n", server_names.join(", ")),
    )
}

/// How long boot waits for the holder to answer `status` — enrolment against a vendor is inside it.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const START_POLL: Duration = Duration::from_millis(500);

/// The docker names one held tool uses: the container, and its two volumes. Derived from the seat
/// and the tool's server name, never random, for the reason `sandbox_netns::holder_name` gives: a
/// stale one can be attributed, and a second daemon on the same seat collides loudly instead of
/// leaking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderNames {
    pub container: String,
    pub state_volume: String,
    pub runtime_volume: String,
}

/// The names for the tool `server_name` of `seat` (the seller pubkey hex; its first 16 characters
/// are the seat's part of the suffix).
pub fn holder_names(seat: &str, server_name: &str) -> HolderNames {
    let seat: String = seat.chars().take(16).collect();
    let suffix = format!("{seat}-{server_name}");
    HolderNames {
        container: format!("maxplayer-held-tool-{suffix}"),
        state_volume: format!("maxplayer-held-tool-state-{suffix}"),
        runtime_volume: format!("maxplayer-held-tool-runtime-{suffix}"),
    }
}

/// `docker run -d …` for the holder.
///
/// What it mounts, and what it does not: the config and the credential read-only at fixed paths; the
/// state and runtime volumes; the seat's `seller-jobs` directory at [`HOLDER_JOBS_DIR`], so a job's
/// directory is `/srv/jobs/<job>` inside. Nothing from `$MAXPLAYER_HOME` beyond the jobs directory.
/// It runs as the JOB uid, so the outputs it publishes into a job's directory are owned by the same
/// uid the job container runs as — a root holder would publish files the job cannot read.
///
/// Hardened like a job container (`--cap-drop ALL`, `no-new-privileges`, `--init`), but NOT egress
/// contained: it has to reach the vendor. Never `--rm`: the daemon removes it by name, so a crashed
/// daemon leaves a stale, attributable container rather than nothing.
pub fn holder_run_argv(
    cfg: &HeldToolConfig,
    names: &HolderNames,
    seat: &str,
    jobs_root: &Path,
    uid: u32,
    gid: u32,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "docker".into(),
        "run".into(),
        "-d".into(),
        "--name".into(),
        names.container.clone(),
        "--label".into(),
        format!("{HOLDER_LABEL}={seat}"),
        "--init".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--user".into(),
        format!("{uid}:{gid}"),
    ];
    if let Some(network) = cfg.network.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        argv.push("--network".into());
        argv.push(network.to_owned());
    }
    argv.extend([
        "-v".into(),
        format!("{}:{HOLDER_CONFIG_PATH}:ro", cfg.config.display()),
        "-v".into(),
        format!("{}:{HOLDER_CREDENTIAL_PATH}:ro", cfg.credential_file.display()),
        "-v".into(),
        format!("{}:{HOLDER_STATE_DIR}", names.state_volume),
        "-v".into(),
        format!("{}:{HOLDER_RUNTIME_DIR}", names.runtime_volume),
        "-v".into(),
        format!("{}:{HOLDER_JOBS_DIR}", jobs_root.display()),
        cfg.image.clone(),
        "tool-holderd".into(),
        "--config".into(),
        HOLDER_CONFIG_PATH.into(),
        "--state".into(),
        HOLDER_STATE_DIR.into(),
        "--runtime".into(),
        HOLDER_RUNTIME_DIR.into(),
        "--credential-file".into(),
        HOLDER_CREDENTIAL_PATH.into(),
    ]);
    if let Some(cli) = cfg.vendor_cli.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        argv.push("--vendor-cli".into());
        argv.push(cli.to_owned());
    }
    if let Some(url) = cfg.vendor_base_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        argv.push("--vendor-base-url".into());
        argv.push(url.to_owned());
    }
    argv
}

/// `docker exec <container> holderctl <command…> --socket <control socket>`.
pub fn holderctl_argv(container: &str, command: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = vec!["docker".into(), "exec".into(), container.into(), "holderctl".into()];
    argv.extend(command.iter().map(|part| (*part).to_owned()));
    argv.push("--socket".into());
    argv.push(HOLDER_CONTROL_SOCKET.into());
    argv
}

/// A one-shot container that makes the two fresh volumes writable by the holder's uid.
///
/// A named volume takes its root's ownership from the image directory it is first mounted over,
/// which is root's. `tool-holderd` insists on 0700 directories it owns, so a non-root holder would
/// refuse to start on volumes root owns. This runs as root, in the holder's own image, and does
/// nothing else. The shell form, measured: `--entrypoint chown` with two paths left both roots
/// untouched on Docker Desktop 29 while reporting success; `sh -c 'chown …'` changes them.
pub fn volume_init_argv(image: &str, names: &HolderNames, uid: u32, gid: u32) -> Vec<String> {
    vec![
        "docker".into(),
        "run".into(),
        "--rm".into(),
        "--user".into(),
        "0:0".into(),
        "--entrypoint".into(),
        "sh".into(),
        "-v".into(),
        format!("{}:{HOLDER_STATE_DIR}", names.state_volume),
        "-v".into(),
        format!("{}:{HOLDER_RUNTIME_DIR}", names.runtime_volume),
        image.into(),
        "-c".into(),
        format!("chown {uid}:{gid} {HOLDER_STATE_DIR} {HOLDER_RUNTIME_DIR}"),
    ]
}

/// A one-shot container that proves this daemon can mount a volume SUBPATH — the mechanism every
/// job's socket mount depends on. Run once at boot, after the holder created `jobs/`, so a daemon
/// too old for it (Docker Engine before 26) fails the seat's boot line instead of every awarded job.
pub fn subpath_probe_argv(image: &str, names: &HolderNames) -> Vec<String> {
    vec![
        "docker".into(),
        "run".into(),
        "--rm".into(),
        "--entrypoint".into(),
        "true".into(),
        "--mount".into(),
        format!("type=volume,src={},dst=/probe,volume-subpath=jobs", names.runtime_volume),
        image.into(),
    ]
}

/// What `holderctl status` reports, as the daemon reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderStatus {
    pub healthy: bool,
    pub resumed_existing_session: bool,
    pub enrollments_this_process: u64,
    /// The health text when unhealthy, for the boot line. Empty when healthy.
    pub health_detail: String,
}

/// Parse the JSON `holderctl status` prints. `holderctl` exits 1 when unhealthy but still prints
/// the document, so the caller parses stdout whatever the exit code.
pub fn parse_status(stdout: &str) -> Result<HolderStatus, String> {
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("holder status is not JSON: {error}"))?;
    let healthy = value["healthy"].as_bool().ok_or("holder status has no `healthy` field")?;
    let health_detail = match &value["health"] {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .iter()
            .map(|(key, detail)| format!("{key}: {detail}"))
            .collect::<Vec<_>>()
            .join("; "),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    Ok(HolderStatus {
        healthy,
        resumed_existing_session: value["resumed_existing_session"].as_bool().unwrap_or(false),
        enrollments_this_process: value["enrollments_this_process"].as_u64().unwrap_or(0),
        health_detail: if healthy { String::new() } else { health_detail },
    })
}

/// Deadlines for the `docker` CLI calls. A `docker` that hangs (a wedged daemon, a registry client
/// that never answers, an `exec` that never returns) must not hold a boot or a job open. Each call
/// is killed at its deadline and reported as a failure its caller handles.
///
/// Queries: `inspect`, `ps`, `volume create`, `volume rm`, `logs`.
const DOCKER_QUERY_TIMEOUT: Duration = Duration::from_secs(20);
/// `holderctl` through `docker exec`: `status`, `attach`, `detach`, `shutdown`.
const DOCKER_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// `docker run` for the holder and for the two one-shot containers.
const DOCKER_RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// `docker rm --force`.
const DOCKER_REMOVE_TIMEOUT: Duration = Duration::from_secs(60);

/// One `docker` CLI run on the blocking pool, bounded by `deadline`: exit code, stdout, stderr. At
/// the deadline the child is killed and reaped, and the call is an `Err` that says so.
async fn docker(argv: Vec<String>, deadline: Duration) -> Result<(i32, String, String), String> {
    tokio::task::spawn_blocking(move || run_bounded(&argv, deadline))
        .await
        .map_err(|error| format!("docker task panicked: {error}"))?
}

/// [`docker`], succeeding only on exit 0; the error carries the command's own words.
async fn docker_ok(argv: Vec<String>, deadline: Duration) -> Result<String, String> {
    let shown = argv.join(" ");
    let (code, stdout, stderr) = docker(argv, deadline).await?;
    if code == 0 {
        Ok(stdout)
    } else {
        Err(format!(
            "`{shown}` exited {code}: {}",
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }
}

/// The words a `docker` call fails with when the program cannot be started at all: no docker CLI on
/// this host. A boot with nothing to reconcile reads it as "nothing to do", not as a fault.
pub const DOCKER_NOT_RUNNABLE: &str = "could not run `docker`";

/// The least time the collection of a child's output gets after the child exits, when the deadline
/// is nearly spent. A child that exits at the deadline's edge has flushed its pipes; a quarter
/// second is enough to read them and is the only way a call can outlast its deadline.
const COLLECT_FLOOR: Duration = Duration::from_millis(250);

/// The blocking half of [`docker`]: spawn the child in its own process group, drain both pipes on
/// their own threads, poll for its exit, and kill the group at the deadline. Every `Drop` fallback
/// in this module runs through it too, so a fallback cannot hang either.
///
/// ONE deadline covers the run and the collection of its output. The pipes close only when every
/// process that holds them has exited. A descendant the CLI left behind (a credential helper, a
/// plugin) that keeps a pipe open past the deadline is killed with the group, and the call is an
/// `Err`: never a success with empty output, which a caller would read as "nothing printed".
fn run_bounded(argv: &[String], deadline: Duration) -> Result<(i32, String, String), String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let (program, args) = argv.split_first().ok_or("an empty docker argv")?;
    let shown = argv.join(" ");
    let started = std::time::Instant::now();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("could not run `{program}`: {error}"))?;
    let group = child.id() as libc::pid_t;
    let stdout = drain_on_thread(child.stdout.take());
    let stderr = drain_on_thread(child.stderr.take());
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= deadline => {
                kill_group(group);
                let _ = child.wait();
                return Err(format!(
                    "`{shown}` did not finish within {}s and was killed",
                    deadline.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                kill_group(group);
                let _ = child.wait();
                return Err(format!("could not wait for `{program}`: {error}"));
            }
        }
    };
    let code = status.code().unwrap_or(-1);
    let remaining = || deadline.saturating_sub(started.elapsed()).max(COLLECT_FLOOR);
    let out = stdout.recv_timeout(remaining());
    let err = stderr.recv_timeout(remaining());
    match (out, err) {
        (Ok(out), Ok(err)) => Ok((
            code,
            String::from_utf8_lossy(&out).trim().to_owned(),
            String::from_utf8_lossy(&err).trim().to_owned(),
        )),
        _ => {
            kill_group(group);
            Err(format!(
                "`{shown}` exited {code}, but its output pipes stayed open past the {}s deadline: a \
                 descendant of the command held them and was killed; nothing it printed was collected",
                deadline.as_secs()
            ))
        }
    }
}

/// Kill every process in the group `run_bounded` created for one call. The child is still held
/// unreaped by the caller, so its id cannot have been reused.
fn kill_group(group: libc::pid_t) {
    // SAFETY: a plain signal to a process group this process created and still holds.
    unsafe {
        libc::killpg(group, libc::SIGKILL);
    }
}

fn drain_on_thread<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        let _ = tx.send(buffer);
    });
    rx
}

/// Whether the container `name` exists, in any state. Unknown is an `Err`, never "absent".
async fn container_exists(name: &str) -> Result<bool, String> {
    let (code, _, stderr) = docker(
        vec!["docker".into(), "inspect".into(), "--format".into(), "{{.Id}}".into(), name.to_owned()],
        DOCKER_QUERY_TIMEOUT,
    )
    .await?;
    inspect_outcome(name, code, &stderr)
}

/// The meaning of one `docker inspect <name>`: exit 0 is "exists"; the daemon's own "no such"
/// words are "absent"; anything else (a daemon that is down, a client that failed) is unknown.
/// Unknown is an error. A shutdown that read unknown as absent would mark a holder stopped that
/// still runs, and disarm the fallback that would have removed it.
fn inspect_outcome(name: &str, code: i32, stderr: &str) -> Result<bool, String> {
    if code == 0 {
        return Ok(true);
    }
    let words = stderr.to_ascii_lowercase();
    if words.contains("no such object") || words.contains("no such container") {
        return Ok(false);
    }
    Err(format!(
        "`docker inspect {name}` exited {code} without saying whether the container exists: {stderr}"
    ))
}

/// Remove the container `name`. `Ok` only when it is confirmed gone: removed now, or absent already.
async fn remove_container(name: &str) -> Result<(), String> {
    let (code, stdout, stderr) =
        docker(vec!["docker".into(), "rm".into(), "--force".into(), name.to_owned()], DOCKER_REMOVE_TIMEOUT).await?;
    if code == 0 || stderr.contains("No such container") {
        return Ok(());
    }
    // `rm` can report a failure for a container that is gone anyway; the fact that matters is
    // whether it exists.
    if !container_exists(name).await? {
        return Ok(());
    }
    Err(format!(
        "`docker rm --force {name}` exited {code}: {}",
        if stderr.is_empty() { stdout } else { stderr }
    ))
}

/// Remove the volume `name`. `Ok` when it is gone: removed now, or absent already.
async fn remove_volume(name: &str) -> Result<(), String> {
    let (code, stdout, stderr) =
        docker(vec!["docker".into(), "volume".into(), "rm".into(), name.to_owned()], DOCKER_QUERY_TIMEOUT).await?;
    if code == 0 || stderr.to_ascii_lowercase().contains("no such volume") {
        return Ok(());
    }
    Err(format!(
        "`docker volume rm {name}` exited {code}: {}",
        if stderr.is_empty() { stdout } else { stderr }
    ))
}

/// Remove a holder's container and runtime volume, and keep its state volume. The async path of
/// a failed start and of [`reconcile_stale_holders`].
async fn remove_holder_resources(names: &HolderNames) -> Result<(), String> {
    remove_container(&names.container).await?;
    remove_volume(&names.runtime_volume).await
}

/// The blocking twin of [`remove_holder_resources`], for the `Drop` fallbacks: `Drop` cannot await,
/// and a task spawned from `Drop` is discarded when the runtime shuts down, which is exactly the
/// path an aborted daemon takes. Bounded, so an aborted daemon cannot hang on it either.
fn remove_holder_blocking(names: &HolderNames) {
    let rm = vec!["docker".into(), "rm".into(), "--force".into(), names.container.clone()];
    match run_bounded(&rm, DOCKER_REMOVE_TIMEOUT) {
        Ok((0, _, _)) => {}
        Ok((_, _, stderr)) if stderr.contains("No such container") => {}
        Ok((code, stdout, stderr)) => eprintln!(
            "seller node: [sandbox] held_tool: fallback removal of {} exited {code}: {}",
            names.container,
            if stderr.is_empty() { stdout } else { stderr }
        ),
        Err(error) => eprintln!(
            "seller node: [sandbox] held_tool: fallback removal of {} failed: {error}",
            names.container
        ),
    }
    let rm_volume = vec!["docker".into(), "volume".into(), "rm".into(), names.runtime_volume.clone()];
    let _ = run_bounded(&rm_volume, DOCKER_QUERY_TIMEOUT);
}

/// Who cleans up after a `docker` call whose future was dropped.
///
/// `spawn_blocking` cannot be cancelled. A future dropped at its `.await` leaves the child running,
/// and the effect the child produces afterwards (a container created, a job attached) has no owner.
/// This type gives it one. The blocking task records when the call ended; the future's owner
/// records the abandonment; whichever of the two comes SECOND runs `cleanup`, so it runs exactly
/// once and only after the effect exists. A call that completes and is disarmed cleans up nothing.
///
/// A start owns its container this way (`cleanup` removes the container and the runtime volume);
/// an attach owns its attachment (`cleanup` detaches the job again).
struct OwnedCall {
    pending: Arc<Mutex<Pending>>,
    cleanup: Arc<dyn Fn() + Send + Sync>,
    armed: bool,
}

#[derive(Default)]
struct Pending {
    in_flight: bool,
    abandoned: bool,
}

impl OwnedCall {
    fn new(cleanup: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            pending: Arc::new(Mutex::new(Pending::default())),
            cleanup: Arc::new(cleanup),
            armed: true,
        }
    }

    /// [`docker`], with the call's effect owned across a dropped future.
    async fn docker(&self, argv: Vec<String>, deadline: Duration) -> Result<(i32, String, String), String> {
        lock(&self.pending).in_flight = true;
        let pending = Arc::clone(&self.pending);
        let cleanup = Arc::clone(&self.cleanup);
        tokio::task::spawn_blocking(move || {
            let outcome = run_bounded(&argv, deadline);
            let abandoned = {
                let mut pending = lock(&pending);
                pending.in_flight = false;
                pending.abandoned
            };
            if abandoned {
                cleanup();
            }
            outcome
        })
        .await
        .map_err(|error| format!("docker task panicked: {error}"))?
    }

    /// [`docker_ok`], with the call's effect owned across a dropped future.
    async fn docker_ok(&self, argv: Vec<String>, deadline: Duration) -> Result<String, String> {
        let shown = argv.join(" ");
        let (code, stdout, stderr) = self.docker(argv, deadline).await?;
        if code == 0 {
            Ok(stdout)
        } else {
            Err(format!(
                "`{shown}` exited {code}: {}",
                if stderr.is_empty() { stdout } else { stderr }
            ))
        }
    }

    /// The effect now has its owner (the value that was built from it): no cleanup on drop.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OwnedCall {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let in_flight = {
            let mut pending = lock(&self.pending);
            pending.abandoned = true;
            pending.in_flight
        };
        // A call still in flight cleans up itself when its child ends. One that ended, or never
        // started, is cleaned up here and now.
        if !in_flight {
            (self.cleanup)();
        }
    }
}

fn lock(pending: &Mutex<Pending>) -> std::sync::MutexGuard<'_, Pending> {
    pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The holders of `seat` that a boot must remove: every container that carries the seat's label
/// and is not the holder of one of the `configured` tools. Pure: `listed` is what
/// `docker ps -a --filter label=… --format {{.Names}}` printed, one name per line.
pub fn stale_holders(seat: &str, listed: &str, configured: &[String]) -> Vec<HolderNames> {
    let wanted: std::collections::HashSet<String> =
        configured.iter().map(|name| holder_names(seat, name).container).collect();
    let seat16: String = seat.chars().take(16).collect();
    let own_prefix = format!("maxplayer-held-tool-{seat16}-");
    listed
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !wanted.contains(*name))
        .filter_map(|container| {
            // The label already says the holder is this seat's; the name check is a second belt,
            // so a container that merely carries the label is never removed by a name it lacks.
            let suffix = container.strip_prefix("maxplayer-held-tool-")?;
            container.strip_prefix(&own_prefix)?;
            Some(HolderNames {
                container: container.to_owned(),
                state_volume: format!("maxplayer-held-tool-state-{suffix}"),
                runtime_volume: format!("maxplayer-held-tool-runtime-{suffix}"),
            })
        })
        .collect()
}

/// Remove the holders of `seat` that this boot's configuration no longer names: a tool that was
/// removed or renamed while its holder survived a daemon that was killed. Their runtime volumes go
/// with them; their state volumes stay, so a tool that is named again resumes its login. Returns
/// the containers removed. Runs at boot, before the configured holders start.
pub async fn reconcile_stale_holders(seat: &str, configured: &[String]) -> Result<Vec<String>, String> {
    let listed = docker_ok(
        vec![
            "docker".into(),
            "ps".into(),
            "-a".into(),
            "--filter".into(),
            format!("label={HOLDER_LABEL}={seat}"),
            "--format".into(),
            "{{.Names}}".into(),
        ],
        DOCKER_QUERY_TIMEOUT,
    )
    .await?;
    let mut removed = Vec::new();
    for names in stale_holders(seat, &listed, configured) {
        remove_holder_resources(&names).await?;
        removed.push(names.container);
    }
    Ok(removed)
}

/// What one `holderctl attach` call established.
#[derive(Debug, PartialEq, Eq)]
enum AttachAnswer {
    /// The holder attached the job; `stdout` is its reply document.
    Attached(String),
    /// The holder answered and refused (a duplicate id, a bad root). Nothing was attached by this
    /// call, and an attachment the refusal names is someone else's: nothing to take back.
    Refused(String),
    /// The call did not complete: killed at its deadline, or `docker` itself failed. Whether the
    /// holder attached the job is unknown, so the caller takes it back, best effort.
    Unknown(String),
}

/// Classify the outcome of the `docker exec … holderctl attach` call.
fn attach_answer(outcome: Result<(i32, String, String), String>) -> AttachAnswer {
    match outcome {
        Ok((0, stdout, _)) => AttachAnswer::Attached(stdout),
        Ok((code, stdout, stderr)) => AttachAnswer::Refused(format!(
            "holderctl exited {code}: {}",
            if stderr.is_empty() { stdout } else { stderr }
        )),
        Err(why) => AttachAnswer::Unknown(why),
    }
}

/// The seat's held tool: a running holder container the daemon owns for its whole life.
///
/// Dropping it removes the container (blocking, bounded, like `NetnsHolder`), unless
/// [`Self::shutdown`] already confirmed the removal. The state volume is never removed here: it
/// holds the vendor login the next boot resumes, which is the whole point of the enroll-once model.
pub struct HeldTool {
    seat: String,
    names: HolderNames,
    image: String,
    server_name: String,
    required: bool,
    status: HolderStatus,
    stopped: AtomicBool,
}

impl HeldTool {
    /// Start the holder for one held tool and wait until it answers, enrolled or resumed.
    ///
    /// `jobs_root` is the seat's `seller-jobs` directory on the host; `uid`/`gid` the identity job
    /// containers run as ([`crate::seller_exec::job_identity`]). Fails — and the caller decides
    /// whether that refuses the boot — when a host path is missing, the image cannot run, the holder
    /// exits before answering (an enrolment failure exits it), a `docker` call passes its deadline,
    /// or this daemon cannot mount a volume subpath. A holder that runs but reports UNHEALTHY starts
    /// successfully; the status says so.
    ///
    /// A start that fails after `docker run`, or is cancelled there, removes the container and the
    /// runtime volume it created ([`OwnedCall`]). The state volume stays.
    pub async fn start(
        cfg: &HeldToolConfig,
        seat: &str,
        jobs_root: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<Self, String> {
        for (label, path) in [("config", &cfg.config), ("credential_file", &cfg.credential_file)] {
            if !path.is_absolute() {
                return Err(format!("[sandbox] held_tool: {label} must be absolute, got {}", path.display()));
            }
            // A missing bind-mount SOURCE would be created by docker as an empty root-owned
            // DIRECTORY, and the holder would then read a directory where it expects a file.
            if !path.is_file() {
                return Err(format!(
                    "[sandbox] held_tool: {label} {} is not a readable file on this host",
                    path.display()
                ));
            }
        }
        if cfg.image.trim().is_empty() {
            return Err("[sandbox] held_tools: image must not be empty".into());
        }
        let server_name = cfg.server_name.trim();
        if server_name.is_empty() || !server_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(format!("[sandbox] held_tools: server_name {:?} is not a plain name", cfg.server_name));
        }
        std::fs::create_dir_all(jobs_root)
            .map_err(|error| format!("[sandbox] held_tool: cannot create {}: {error}", jobs_root.display()))?;

        let names = holder_names(seat, server_name);
        let owned_names = names.clone();
        let mut owner = OwnedCall::new(move || remove_holder_blocking(&owned_names));
        match Self::start_owned(cfg, seat, jobs_root, uid, gid, &names, server_name, &owner).await {
            Ok(tool) => {
                owner.disarm();
                Ok(tool)
            }
            Err(error) => {
                // The explicit path: remove what this start created, and report both facts. The
                // owner stays armed until then, for a cancellation during this cleanup.
                let cleanup = remove_holder_resources(&names).await;
                owner.disarm();
                Err(match cleanup {
                    Ok(()) => error,
                    Err(more) => format!("{error}; the cleanup of the failed start also failed: {more}"),
                })
            }
        }
    }

    /// [`Self::start`] from the first `docker` call on. Every call goes through `owner`, so a start
    /// cancelled while `docker run` is still creating the holder removes it once it exists; the
    /// caller owns the cleanup of a failure.
    #[allow(clippy::too_many_arguments)]
    async fn start_owned(
        cfg: &HeldToolConfig,
        seat: &str,
        jobs_root: &Path,
        uid: u32,
        gid: u32,
        names: &HolderNames,
        server_name: &str,
        owner: &OwnedCall,
    ) -> Result<Self, String> {
        // A stale holder from a daemon that died without its shutdown path: remove it by name, so
        // this boot's container is the one the name addresses.
        remove_container(&names.container).await?;
        for volume in [&names.state_volume, &names.runtime_volume] {
            owner
                .docker_ok(
                    vec!["docker".into(), "volume".into(), "create".into(), volume.clone()],
                    DOCKER_QUERY_TIMEOUT,
                )
                .await?;
        }
        owner.docker_ok(volume_init_argv(&cfg.image, names, uid, gid), DOCKER_RUN_TIMEOUT).await?;
        owner.docker_ok(holder_run_argv(cfg, names, seat, jobs_root, uid, gid), DOCKER_RUN_TIMEOUT).await?;

        // Wait for `status`. The holder enrols before it binds its control socket, so the wait
        // covers a real login against the vendor. Every call inside the loop is bounded, so the
        // loop ends at START_TIMEOUT even when `docker exec` hangs.
        let started = std::time::Instant::now();
        let status = loop {
            let (code, stdout, stderr) = owner
                .docker(holderctl_argv(&names.container, &["status"]), DOCKER_CONTROL_TIMEOUT)
                .await?;
            if (code == 0 || code == 1)
                && !stdout.is_empty()
                && let Ok(status) = parse_status(&stdout)
            {
                break status;
            }
            // Gone already? Then its own last words are the diagnosis (an enrolment failure).
            let (_, state, _) = owner
                .docker(
                    vec![
                        "docker".into(),
                        "inspect".into(),
                        "--format".into(),
                        "{{.State.Status}}".into(),
                        names.container.clone(),
                    ],
                    DOCKER_QUERY_TIMEOUT,
                )
                .await?;
            if state == "exited" || state == "dead" {
                let (_, logs_out, logs_err) = owner
                    .docker(
                        vec!["docker".into(), "logs".into(), "--tail".into(), "20".into(), names.container.clone()],
                        DOCKER_QUERY_TIMEOUT,
                    )
                    .await?;
                return Err(format!(
                    "[sandbox] held_tool: the holder exited before it answered; its last output: {}",
                    if logs_err.is_empty() { logs_out } else { logs_err }
                ));
            }
            if started.elapsed() > START_TIMEOUT {
                return Err(format!(
                    "[sandbox] held_tool: the holder did not answer `status` within {}s ({stderr})",
                    START_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(START_POLL).await;
        };

        // The per-job socket mount needs `volume-subpath`; prove it now, once, or say so.
        owner
            .docker_ok(subpath_probe_argv(&cfg.image, names), DOCKER_RUN_TIMEOUT)
            .await
            .map_err(|error| {
                format!(
                    "[sandbox] held_tool: this docker daemon cannot mount a volume subpath, which every \
                     job's socket mount needs (Docker Engine 26 or newer): {error}"
                )
            })?;

        Ok(Self {
            seat: seat.to_owned(),
            names: names.clone(),
            image: cfg.image.clone(),
            server_name: server_name.to_owned(),
            required: cfg.required,
            status,
            stopped: AtomicBool::new(false),
        })
    }

    pub fn container(&self) -> &str {
        &self.names.container
    }

    pub fn names(&self) -> &HolderNames {
        &self.names
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn required(&self) -> bool {
        self.required
    }

    pub fn status(&self) -> &HolderStatus {
        &self.status
    }

    /// The seller boot line for this tool. Never carries a credential or a path into the holder's
    /// state; it names the image, the container, the health, and whether a login was resumed.
    pub fn boot_line(&self) -> String {
        let session = if self.status.resumed_existing_session {
            "resumed the persisted login (no new enrolment)".to_owned()
        } else {
            format!("enrolled ({} login this start)", self.status.enrollments_this_process)
        };
        if self.status.healthy {
            format!(
                "seller node: [sandbox] held_tool: {} in container {} is HEALTHY, {session}; jobs see it \
                 as MCP server `{}` over a per-job socket",
                self.image, self.names.container, self.server_name
            )
        } else {
            format!(
                "seller node: [sandbox] held_tool: {} in container {} is UNHEALTHY ({}), {session}; a \
                 job that calls it will see the failure",
                self.image, self.names.container, self.status.health_detail
            )
        }
    }

    /// Attach `job_id`: the holder creates the job's socket and records the job's directory
    /// (`/srv/jobs/<job_id>` inside the holder, which is `<home>/seller-jobs/<job_id>` on the host —
    /// the directory MUST exist before this call, because the holder canonicalizes it). Bounded by
    /// [`DOCKER_CONTROL_TIMEOUT`].
    pub async fn attach(&self, job_id: &str) -> Result<JobToolEndpoint, String> {
        let job_root = format!("{HOLDER_JOBS_DIR}/{job_id}");
        let detach_argv = holderctl_argv(&self.names.container, &["detach", "--job-id", job_id]);
        // The attachment is owned across a dropped future: a holder that attached the job after
        // this daemon stopped waiting is told to detach it again (`OwnedCall`).
        let mut owner = OwnedCall::new({
            let detach_argv = detach_argv.clone();
            move || {
                let _ = run_bounded(&detach_argv, DOCKER_CONTROL_TIMEOUT);
            }
        });
        let attached = owner
            .docker(
                holderctl_argv(&self.names.container, &["attach", "--job-id", job_id, "--job-root", &job_root]),
                DOCKER_CONTROL_TIMEOUT,
            )
            .await;
        owner.disarm();
        let stdout = match attach_answer(attached) {
            AttachAnswer::Attached(stdout) => stdout,
            AttachAnswer::Refused(why) => {
                // The holder answered and refused: nothing was attached by this call. The
                // refusal of a DUPLICATE id names an attachment that exists and belongs to
                // someone else; it is preserved, not detached.
                return Err(format!("[sandbox] held_tool: attach {job_id} failed: {why}"));
            }
            AttachAnswer::Unknown(why) => {
                // The call did not complete (killed at its deadline, or `docker` itself failed).
                // A killed `docker exec` does not stop the `holderctl` it started, so the attach
                // may have landed anyway. Best effort, so a later attach of this id is not refused
                // as live; a holder that never attached it answers "no such attached job".
                let _ = docker(detach_argv, DOCKER_CONTROL_TIMEOUT).await;
                return Err(format!("[sandbox] held_tool: attach {job_id} failed: {why}"));
            }
        };
        let reply: Value = serde_json::from_str(stdout.trim())
            .map_err(|error| format!("[sandbox] held_tool: attach reply is not JSON: {error}"))?;
        let expected_socket = format!("{HOLDER_RUNTIME_DIR}/jobs/{job_id}/job.sock");
        if reply["socket"].as_str() != Some(expected_socket.as_str()) {
            // The holder holds an attachment this daemon will not use: take it back, so the job's
            // directory is not recorded under a socket nobody mounts.
            let _ = docker(
                holderctl_argv(&self.names.container, &["detach", "--job-id", job_id]),
                DOCKER_CONTROL_TIMEOUT,
            )
            .await;
            return Err(format!(
                "[sandbox] held_tool: the holder placed the socket at {:?}, not at {expected_socket}; \
                 the job mount would miss it",
                reply["socket"]
            ));
        }
        Ok(JobToolEndpoint {
            container: self.names.container.clone(),
            runtime_volume: self.names.runtime_volume.clone(),
            job_id: job_id.to_owned(),
            server_name: self.server_name.clone(),
            detached: false,
        })
    }

    /// Stop the holder politely, then remove its container and its runtime volume. The state
    /// volume stays: it is the login the next boot resumes.
    ///
    /// `Ok` only when the container is confirmed gone. On `Err` the holder stays marked running, so
    /// the drop fallback retries the removal; the error says what is still there.
    pub async fn shutdown(&self) -> Result<(), String> {
        if self.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        let _ = docker(holderctl_argv(&self.names.container, &["shutdown"]), DOCKER_CONTROL_TIMEOUT).await;
        remove_container(&self.names.container).await.map_err(|error| {
            let message = format!(
                "[sandbox] held_tool: holder {} is NOT removed ({error}); the drop fallback retries",
                self.names.container
            );
            eprintln!("seller node: {message}");
            message
        })?;
        self.stopped.store(true, Ordering::SeqCst);
        if let Err(error) = remove_volume(&self.names.runtime_volume).await {
            eprintln!(
                "seller node: [sandbox] held_tool: runtime volume {} is not removed: {error}",
                self.names.runtime_volume
            );
        }
        eprintln!(
            "seller node: [sandbox] held_tool: holder {} stopped; the login persists in volume {} for the next boot",
            self.names.container, self.names.state_volume
        );
        Ok(())
    }

    pub fn seat(&self) -> &str {
        &self.seat
    }
}

impl Drop for HeldTool {
    /// The backstop for a daemon that never reached [`Self::shutdown`], or whose shutdown could not
    /// confirm the removal: remove the container and the runtime volume, blocking and bounded.
    fn drop(&mut self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        remove_holder_blocking(&self.names);
    }
}

/// One job's endpoint on the seat's held tool: created by [`HeldTool::attach`], removed by
/// [`Self::detach`] — or, as a fallback, on drop.
pub struct JobToolEndpoint {
    container: String,
    runtime_volume: String,
    job_id: String,
    server_name: String,
    detached: bool,
}

impl JobToolEndpoint {
    /// What the job gets for this tool: its own socket directory mounted at
    /// `/run/holder/<server name>` (a subpath of the holder's runtime volume, so no other job's
    /// socket and none of the holder's state come with it), and one stdio MCP server entry that
    /// spawns the bridge with `--socket` pointing into that mount. A flag, not an environment
    /// variable: a stdio server's arguments reach the child on every harness.
    pub fn attachments(&self) -> JobAttachments {
        JobAttachments {
            mcp_servers: vec![McpServer::Stdio(McpServerStdio {
                name: self.server_name.clone(),
                command: CONTAINER_TOOL_BRIDGE_BIN.to_owned(),
                args: vec!["--socket".to_owned(), job_socket_path(&self.server_name)],
                env: Vec::new(),
            })],
            extra_mounts: vec![ExtraMount::VolumeSubpath {
                volume: self.runtime_volume.clone(),
                subpath: format!("jobs/{}", self.job_id),
                container: job_socket_mount(&self.server_name),
            }],
        }
    }

    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    /// Detach: the holder closes and removes this job's socket. The tool stays enrolled — the
    /// holder's reply says so, and that is the property the corrected model turns on.
    ///
    /// `Ok` when the holder confirmed the detach, or said the job was not attached: the socket is
    /// gone either way. On `Err` the endpoint stays marked attached, so the drop fallback retries
    /// once, bounded.
    pub async fn detach(mut self) -> Result<(), String> {
        let (code, stdout, stderr) =
            docker(holderctl_argv(&self.container, &["detach", "--job-id", &self.job_id]), DOCKER_CONTROL_TIMEOUT)
                .await
                .map_err(|error| format!("[sandbox] held_tool: detach {} failed: {error}", self.job_id))?;
        if code == 0 {
            self.detached = true;
            if !stdout.contains("\"tool_still_enrolled\": true") {
                eprintln!(
                    "seller node: [sandbox] held_tool: detach {} did not confirm the tool stayed enrolled: {stdout}",
                    self.job_id
                );
            }
            return Ok(());
        }
        if stderr.contains("no such attached job") {
            self.detached = true;
            return Ok(());
        }
        Err(format!(
            "[sandbox] held_tool: detach {} failed: holderctl exited {code}: {}",
            self.job_id,
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }
}

impl Drop for JobToolEndpoint {
    /// A job that left without a confirmed detach (a panic, an early `?`, a failed detach call)
    /// still gets its socket removed. Off the runtime, on its own thread: `Drop` cannot await, and
    /// the holder call is a blocking exec. Bounded, so the thread ends even when `docker` hangs.
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        let argv = holderctl_argv(&self.container, &["detach", "--job-id", &self.job_id]);
        std::thread::spawn(move || {
            let _ = run_bounded(&argv, DOCKER_CONTROL_TIMEOUT);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HeldToolConfig {
        HeldToolConfig {
            server_name: "figma".into(),
            image: "my-holder:latest".into(),
            config: "/etc/maxplayer/seller-tool-config.json".into(),
            credential_file: "/home/seller/.config/maxplayer/vendor-cred.json".into(),
            vendor_base_url: Some("http://vendor:8080".into()),
            vendor_cli: None,
            network: Some("maxplayer-tools".into()),
            required: false,
        }
    }

    const SEAT: &str = "25f6b60a3e3870d5533b7f08133fc1cdff4c43bad2c30974faec8b059dde019f";

    #[test]
    fn the_names_derive_from_the_seat_and_the_tool_and_nothing_random() {
        let names = holder_names(SEAT, "figma");
        assert_eq!(names.container, "maxplayer-held-tool-25f6b60a3e3870d5-figma");
        assert_eq!(names.state_volume, "maxplayer-held-tool-state-25f6b60a3e3870d5-figma");
        assert_eq!(names.runtime_volume, "maxplayer-held-tool-runtime-25f6b60a3e3870d5-figma");
        assert_eq!(holder_names(SEAT, "figma"), names, "the same seat and tool always name the same holder");
        assert_ne!(holder_names(SEAT, "jira"), names, "two tools of one seat never share a holder");
        assert_eq!(job_socket_mount("figma"), "/run/holder/figma");
        assert_eq!(job_socket_path("figma"), "/run/holder/figma/job.sock");
    }

    /// The holder is handed exactly five mounts — config and credential read-only, two volumes,
    /// the jobs directory — runs as the job uid, carries no environment, and is not `--rm`.
    #[test]
    fn the_holder_mounts_what_it_needs_read_only_and_runs_as_the_job_uid() {
        let names = holder_names(SEAT, "figma");
        let argv = holder_run_argv(&cfg(), &names, SEAT, Path::new("/home/seller/.maxplayer/seller-jobs"), 501, 20);
        let text = argv.join(" ");
        assert!(text.starts_with("docker run -d --name maxplayer-held-tool-25f6b60a3e3870d5-figma "));
        assert!(text.contains(" --label maxplayer.held-tool.seat=25f6b60a3e3870d5533b7f08133fc1cdff4c43bad2c30974faec8b059dde019f "));
        assert!(text.contains(" --user 501:20 "));
        assert!(text.contains(" --network maxplayer-tools "));
        assert!(text.contains(" -v /etc/maxplayer/seller-tool-config.json:/etc/maxplayer/seller-tool-config.json:ro "));
        assert!(text.contains(" -v /home/seller/.config/maxplayer/vendor-cred.json:/run/secrets/cred.json:ro "));
        assert!(text.contains(" -v maxplayer-held-tool-state-25f6b60a3e3870d5-figma:/var/lib/maxplayer-holder "));
        assert!(text.contains(" -v maxplayer-held-tool-runtime-25f6b60a3e3870d5-figma:/run/maxplayer-holder "));
        assert!(text.contains(" -v /home/seller/.maxplayer/seller-jobs:/srv/jobs my-holder:latest tool-holderd "));
        assert!(text.ends_with(
            "--config /etc/maxplayer/seller-tool-config.json --state /var/lib/maxplayer-holder --runtime \
             /run/maxplayer-holder --credential-file /run/secrets/cred.json --vendor-base-url http://vendor:8080"
        ));
        assert_eq!(argv.iter().filter(|a| *a == "-v").count(), 5, "exactly five mounts");
        assert!(!argv.iter().any(|a| a == "-e"), "no environment: the holder reads its config and credential from files");
        assert!(!argv.iter().any(|a| a == "--rm"), "never --rm: a stale holder must stay attributable");
        for flag in ["--cap-drop", "--security-opt", "--init"] {
            assert!(argv.iter().any(|a| a == flag), "hardening flag {flag} missing");
        }
    }

    #[test]
    fn optional_knobs_appear_only_when_set() {
        let mut bare = cfg();
        bare.network = None;
        bare.vendor_base_url = None;
        let argv = holder_run_argv(&bare, &holder_names(SEAT, "figma"), SEAT, Path::new("/jobs"), 1000, 1000);
        assert!(!argv.iter().any(|a| a == "--network"));
        assert!(!argv.iter().any(|a| a == "--vendor-base-url"));
        let mut with_cli = cfg();
        with_cli.vendor_cli = Some("/opt/vendor/bin/vendorcli".into());
        let argv = holder_run_argv(&with_cli, &holder_names(SEAT, "figma"), SEAT, Path::new("/jobs"), 1000, 1000);
        let i = argv.iter().position(|a| a == "--vendor-cli").expect("--vendor-cli");
        assert_eq!(argv[i + 1], "/opt/vendor/bin/vendorcli");
    }

    #[test]
    fn holderctl_is_reached_through_docker_exec_on_the_control_socket() {
        assert_eq!(
            holderctl_argv("maxplayer-held-tool-abc", &["attach", "--job-id", "job1", "--job-root", "/srv/jobs/job1"]),
            vec![
                "docker", "exec", "maxplayer-held-tool-abc", "holderctl", "attach", "--job-id", "job1",
                "--job-root", "/srv/jobs/job1", "--socket", "/run/maxplayer-holder/holder.sock",
            ]
        );
    }

    #[test]
    fn the_volume_init_runs_as_root_only_to_chown_and_the_probe_mounts_a_subpath() {
        let names = holder_names(SEAT, "figma");
        let init = volume_init_argv("my-holder:latest", &names, 501, 20).join(" ");
        assert!(init.contains(" --rm --user 0:0 --entrypoint sh "));
        assert!(init.ends_with(" my-holder:latest -c chown 501:20 /var/lib/maxplayer-holder /run/maxplayer-holder"));
        let probe = subpath_probe_argv("my-holder:latest", &names).join(" ");
        assert!(probe.contains("--entrypoint true"));
        assert!(probe.contains(
            "--mount type=volume,src=maxplayer-held-tool-runtime-25f6b60a3e3870d5-figma,dst=/probe,volume-subpath=jobs"
        ));
    }

    #[test]
    fn stale_holders_are_this_seats_unconfigured_holders_and_nothing_else() {
        let listed = "maxplayer-held-tool-25f6b60a3e3870d5-figma\n\
                      maxplayer-held-tool-25f6b60a3e3870d5-jira\n\
                      maxplayer-held-tool-25f6b60a3e3870d5-old-name\n\
                      maxplayer-held-tool-0000000000000000-figma\n\
                      some-other-container\n\n";
        let stale = stale_holders(SEAT, listed, &["figma".to_owned(), "jira".to_owned()]);
        assert_eq!(
            stale,
            vec![HolderNames {
                container: "maxplayer-held-tool-25f6b60a3e3870d5-old-name".into(),
                state_volume: "maxplayer-held-tool-state-25f6b60a3e3870d5-old-name".into(),
                runtime_volume: "maxplayer-held-tool-runtime-25f6b60a3e3870d5-old-name".into(),
            }],
            "a configured holder stays, another seat's name and a foreign name are never touched"
        );
        assert!(stale_holders(SEAT, "", &["figma".to_owned()]).is_empty());
        let all_gone = stale_holders(SEAT, "maxplayer-held-tool-25f6b60a3e3870d5-figma\n", &[]);
        assert_eq!(all_gone.len(), 1, "a seat that removed every tool removes every holder");
    }

    /// The deadline is real: a child that never exits is killed and reported, and the call returns
    /// well before the child would have. A child that exits normally reports its code and both pipes.
    #[test]
    fn a_bounded_docker_call_is_killed_at_its_deadline() {
        let started = std::time::Instant::now();
        let error = run_bounded(&["sh".into(), "-c".into(), "sleep 30".into()], Duration::from_millis(300))
            .expect_err("a child past its deadline is an error");
        assert!(error.contains("did not finish within") && error.contains("was killed"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10), "the call must return at the deadline, not at the child's end");
        let (code, stdout, stderr) =
            run_bounded(&["sh".into(), "-c".into(), "echo out; echo err >&2; exit 3".into()], Duration::from_secs(10))
                .expect("a child that exits is reported");
        assert_eq!((code, stdout.as_str(), stderr.as_str()), (3, "out", "err"));
        assert!(run_bounded(&[], Duration::from_secs(1)).is_err(), "an empty argv is refused");
    }

    /// A child that exits but leaves a descendant holding its pipes is a failure of the call, never
    /// a success with empty output: a boot that read "" from `docker ps` would remove nothing and
    /// believe it. The group is killed, so the descendant does not survive the call.
    #[test]
    fn a_descendant_that_holds_the_pipes_fails_the_call_instead_of_emptying_its_output() {
        let started = std::time::Instant::now();
        let error = run_bounded(
            &["sh".into(), "-c".into(), "echo out; sleep 30 & exit 0".into()],
            Duration::from_secs(2),
        )
        .expect_err("pipes held past the deadline must fail the call");
        assert!(error.contains("stayed open") && error.contains("exited 0"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10), "the call ends at its deadline, not the descendant's");
        let (code, out, _) = run_bounded(&["sh".into(), "-c".into(), "echo fast".into()], Duration::from_secs(5))
            .expect("a child that closes its pipes is collected");
        assert_eq!((code, out.as_str()), (0, "fast"));
    }

    #[test]
    fn inspect_words_decide_presence_and_an_unknown_answer_is_an_error() {
        assert_eq!(inspect_outcome("h", 0, ""), Ok(true));
        assert_eq!(inspect_outcome("h", 1, "Error: No such object: h"), Ok(false));
        assert_eq!(inspect_outcome("h", 1, "Error response from daemon: No such container: h"), Ok(false));
        let down = inspect_outcome("h", 1, "Cannot connect to the Docker daemon at unix:///var/run/docker.sock");
        assert!(down.is_err(), "a daemon that is down is unknown, not absent: {down:?}");
        assert!(inspect_outcome("h", 125, "").is_err());
    }

    /// The handoff: a future dropped while its child runs does not clean up at once (the effect
    /// does not exist yet) and does not forget (the blocking task cleans up when the child ends).
    #[tokio::test]
    async fn an_owned_call_dropped_in_flight_cleans_up_exactly_once_after_the_child_ends() {
        use std::sync::atomic::AtomicUsize;
        let cleaned = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&cleaned);
        let owner = OwnedCall::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let pending = Arc::clone(&owner.pending);
        let task = tokio::spawn(async move {
            let _ = owner.docker(vec!["sh".into(), "-c".into(), "sleep 1".into()], Duration::from_secs(10)).await;
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();
        let _ = task.await;
        assert_eq!(cleaned.load(Ordering::SeqCst), 0, "the effect does not exist yet: no cleanup at the drop");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while cleaned.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(cleaned.load(Ordering::SeqCst), 1, "the task cleans up once the child ended");
        assert!(!lock(&pending).in_flight);
    }

    #[tokio::test]
    async fn an_owned_call_that_completes_cleans_up_only_when_it_stays_armed() {
        use std::sync::atomic::AtomicUsize;
        let cleaned = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&cleaned);
        let mut owner = OwnedCall::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let (code, out, _) = owner
            .docker(vec!["sh".into(), "-c".into(), "echo hi".into()], Duration::from_secs(5))
            .await
            .expect("the call runs");
        assert_eq!((code, out.as_str()), (0, "hi"));
        owner.disarm();
        drop(owner);
        assert_eq!(cleaned.load(Ordering::SeqCst), 0, "a disarmed owner cleans up nothing");

        let counter = Arc::clone(&cleaned);
        let owner = OwnedCall::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let _ = owner.docker(vec!["sh".into(), "-c".into(), "exit 3".into()], Duration::from_secs(5)).await;
        drop(owner);
        assert_eq!(cleaned.load(Ordering::SeqCst), 1, "an armed owner whose call ended cleans up at the drop");
    }

    /// A refusal the holder answered (the duplicate id above all) attached nothing and names an
    /// attachment that is someone else's; only a call that did not complete is unknown.
    #[test]
    fn an_attach_the_holder_refused_is_not_taken_back_but_an_unknown_one_is() {
        assert_eq!(
            attach_answer(Ok((0, "{\"socket\": \"/run/maxplayer-holder/jobs/j1/job.sock\"}".into(), String::new()))),
            AttachAnswer::Attached("{\"socket\": \"/run/maxplayer-holder/jobs/j1/job.sock\"}".into())
        );
        let duplicate = attach_answer(Ok((1, String::new(), "holderctl: job \"j1\" is already attached; detach it first".into())));
        assert!(matches!(&duplicate, AttachAnswer::Refused(why) if why.contains("already attached")), "{duplicate:?}");
        let killed = attach_answer(Err("`docker exec …` did not finish within 30s and was killed".into()));
        assert!(matches!(killed, AttachAnswer::Unknown(_)));
    }

    #[test]
    fn a_status_document_parses_whether_healthy_or_not() {
        let healthy = parse_status(
            r#"{"healthy": true, "health": "healthy", "resumed_existing_session": true, "enrollments_this_process": 0}"#,
        )
        .expect("parse");
        assert!(healthy.healthy);
        assert!(healthy.resumed_existing_session);
        assert_eq!(healthy.enrollments_this_process, 0);
        assert_eq!(healthy.health_detail, "");
        let sick = parse_status(
            r#"{"healthy": false, "health": {"unhealthy": "vendor rejected the stored session"}, "resumed_existing_session": false, "enrollments_this_process": 1}"#,
        )
        .expect("parse");
        assert!(!sick.healthy);
        assert!(sick.health_detail.contains("rejected the stored session"), "{}", sick.health_detail);
        assert!(parse_status("not json").is_err());
        assert!(parse_status(r#"{"resumed_existing_session": true}"#).is_err(), "no healthy field");
    }

    /// Per tool, the job gets the bridge entry with `--socket` into that tool's mount, and ONE mount:
    /// the tool's runtime volume `jobs/<job>` subpath at `/run/holder/<tool>`. Two tools merge into
    /// two entries and two mounts at distinct paths.
    #[test]
    fn job_endpoints_attach_one_socket_mount_and_one_bridge_entry_per_tool() {
        let endpoint = |tool: &str| JobToolEndpoint {
            container: format!("maxplayer-held-tool-abc-{tool}"),
            runtime_volume: format!("maxplayer-held-tool-runtime-abc-{tool}"),
            job_id: "job-1".into(),
            server_name: tool.into(),
            detached: true, // no docker call on drop in a unit test
        };
        let attachments = endpoint("figma").attachments();
        assert_eq!(
            attachments.mcp_servers,
            vec![McpServer::Stdio(McpServerStdio {
                name: "figma".into(),
                command: "/usr/local/bin/tool-mcp-bridge".into(),
                args: vec!["--socket".into(), "/run/holder/figma/job.sock".into()],
                env: Vec::new(),
            })]
        );
        assert_eq!(
            attachments.extra_mounts,
            vec![ExtraMount::VolumeSubpath {
                volume: "maxplayer-held-tool-runtime-abc-figma".into(),
                subpath: "jobs/job-1".into(),
                container: "/run/holder/figma".into(),
            }]
        );
        assert_eq!(
            attachments.extra_mounts[0].argv(),
            vec![
                "--mount",
                "type=volume,src=maxplayer-held-tool-runtime-abc-figma,dst=/run/holder/figma,volume-subpath=jobs/job-1",
            ]
        );
        let both = JobAttachments::merge([endpoint("figma").attachments(), endpoint("jira").attachments()]);
        assert_eq!(both.mcp_servers.len(), 2);
        assert_eq!(both.extra_mounts.len(), 2);
        assert_eq!(both.mcp_servers[1].name(), "jira");
        assert!(matches!(&both.extra_mounts[1], ExtraMount::VolumeSubpath { container, .. } if container == "/run/holder/jira"));
        let wire = serde_json::to_string(&both.mcp_servers).unwrap();
        assert!(wire.contains("/run/holder/figma/job.sock") && wire.contains("/run/holder/jira/job.sock"));
    }
}

/// LIVE end-to-end proof of the Holder route through the REAL daemon code: [`HeldTool::start`],
/// [`HeldTool::attach`], the real `prepare_launch` and `launch_with_mounts`, the sandbox image with
/// the bridge, and the real cleanup capture — against the kit's fake vendors, whose counters are the
/// independent oracle. Two held tools, so the per-tool naming, mounting and addressing is what is
/// proved. `#[ignore]`d: it needs docker, the kit image, and the sandbox image.
///
///   cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture held_tool::live
///
/// Knobs: MAXPLAYER_HELD_TOOL_LIVE_IMAGE (default `maxplayer-tool-kit:demo`), MAXPLAYER_SANDBOX_IMAGE
/// (default `maxplayer-sandbox:tools`), MAXPLAYER_HELD_TOOL_LIVE_NETWORK + MAXPLAYER_HELD_TOOL_LIVE_PROXY_PORTS
/// (run the contained job under egress containment), MAXPLAYER_HELD_TOOL_LIVE_EVIDENCE_DIR (write the record).
///
/// Synthetic throughout: each credential is generated here and exists only inside its fake vendor.
#[cfg(all(test, feature = "acp"))]
mod live_tests {
    use super::*;
    use crate::home::{SandboxConfig, SandboxMode};
    use crate::seller_exec::{
        cleanup_job_container, job_container_name, job_id_of, job_identity, prepare_launch, CleanupPolicy,
        JobContainer, JobLaunch, SandboxPolicy,
    };
    use crate::seller_git::DeliveryAgentIdentity;
    use serde_json::json;
    use std::path::PathBuf;

    const SEAT: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    /// Two tools from the same kit image, two vendors, two logins — the case the list exists for.
    const TOOLS: [&str; 2] = ["text-a", "text-b"];

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.trim().is_empty())
    }

    fn sh(argv: &[&str]) -> Result<String, String> {
        let done = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .output()
            .map_err(|e| format!("run {}: {e}", argv[0]))?;
        if done.status.success() {
            Ok(String::from_utf8_lossy(&done.stdout).trim().to_owned())
        } else {
            Err(format!(
                "`{}` exited {:?}: {}",
                argv.join(" "),
                done.status.code(),
                String::from_utf8_lossy(&done.stderr).trim()
            ))
        }
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    out.extend(walk(&path));
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    /// One fake vendor per tool, published on loopback so the HOST reads its counters.
    struct Vendor {
        container: String,
        url: String,
        secret: String,
    }

    /// The whole fixture: a scratch home, one credential and one offering per tool, a docker network,
    /// and one fake vendor per tool. Torn down on drop.
    struct Fixture {
        root: PathBuf,
        network: String,
        vendors: Vec<Vendor>,
        cfgs: Vec<HeldToolConfig>,
        evidence: Option<PathBuf>,
    }

    impl Fixture {
        fn up(names: &[&str]) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let tag = format!("{}-{stamp}", std::process::id());
            let root = std::env::temp_dir().join(format!("maxplayer-held-tool-live-{tag}"));
            std::fs::create_dir_all(root.join("seller-jobs")).unwrap();
            let image = env("MAXPLAYER_HELD_TOOL_LIVE_IMAGE").unwrap_or_else(|| "maxplayer-tool-kit:demo".into());
            let network = format!("mx-held-live-{tag}");
            sh(&["docker", "network", "create", &network]).expect("create the test network");
            let fixture_config = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../maxplayer-tool-kit/fixtures/seller-tool-config.json");

            let mut vendors = Vec::new();
            let mut cfgs = Vec::new();
            for name in names {
                // A synthetic credential per tool, generated now, never on a command line.
                let mut raw = [0u8; 12];
                getrandom::fill(&mut raw).unwrap();
                let secret = format!("synthetic-held-tool-secret-{name}-{}", hex::encode(raw));
                let credential_file = root.join(format!("cred-{name}.json"));
                std::fs::write(
                    &credential_file,
                    json!({"client_id": format!("synthetic-seller-client-{name}"), "client_secret": secret}).to_string(),
                )
                .unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&credential_file, std::fs::Permissions::from_mode(0o600)).unwrap();
                }
                // The kit's fixture offering, one copy per tool, so the holder gets an absolute host path.
                let config = root.join(format!("offering-{name}.json"));
                std::fs::copy(&fixture_config, &config).expect("copy the kit fixture config");
                let container = format!("mx-held-vendor-{tag}-{name}");
                let alias = format!("vendor-{name}");
                sh(&[
                    "docker", "run", "-d", "--name", &container, "--network", &network, "--network-alias", &alias,
                    "-p", "127.0.0.1:0:8080",
                    "-v", &format!("{}:/run/secrets/cred.json:ro", credential_file.display()),
                    &image, "vendor-service", "--listen", "0.0.0.0:8080", "--credential-file", "/run/secrets/cred.json",
                ])
                .expect("start a fake vendor");
                let port = sh(&["docker", "port", &container, "8080/tcp"]).expect("vendor port");
                let url = format!("http://{}", port.lines().next().unwrap().trim());
                vendors.push(Vendor { container, url, secret });
                cfgs.push(HeldToolConfig {
                    server_name: (*name).to_owned(),
                    image: image.clone(),
                    config,
                    credential_file,
                    vendor_base_url: Some(format!("http://{alias}:8080")),
                    vendor_cli: None,
                    network: Some(network.clone()),
                    required: true,
                });
            }
            let this = Self {
                root,
                network,
                vendors,
                cfgs,
                evidence: env("MAXPLAYER_HELD_TOOL_LIVE_EVIDENCE_DIR").map(PathBuf::from),
            };
            // Every vendor answers before anything depends on it.
            for vendor in &this.vendors {
                let mut ready = false;
                for _ in 0..50 {
                    if Self::stats_blocking(&vendor.url).is_ok() {
                        ready = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                assert!(ready, "the fake vendor {} did not come up", vendor.container);
            }
            this
        }

        /// A plain-socket GET of a vendor's counters, for the readiness poll inside `up()`. Plain
        /// std, not reqwest's blocking client: that client owns a runtime of its own, and dropping it
        /// inside this test's runtime is what tokio refuses.
        fn stats_blocking(url: &str) -> Result<Value, String> {
            use std::io::{Read, Write};
            let authority = url.trim_start_matches("http://");
            let mut stream = std::net::TcpStream::connect(authority).map_err(|e| e.to_string())?;
            stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
            write!(stream, "GET /admin/stats HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .map_err(|e| e.to_string())?;
            let mut raw = String::new();
            stream.read_to_string(&mut raw).map_err(|e| e.to_string())?;
            let body = raw.split_once("\r\n\r\n").map(|(_, body)| body).ok_or("no body")?;
            serde_json::from_str(body.trim()).map_err(|e| e.to_string())
        }

        async fn stats(&self, tool: usize) -> Value {
            let url = format!("{}/admin/stats", self.vendors[tool].url);
            let body = reqwest::get(&url).await.expect("vendor stats").text().await.expect("stats body");
            serde_json::from_str(&body).expect("stats json")
        }

        async fn login_count(&self, tool: usize) -> u64 {
            self.stats(tool).await["login_count"].as_u64().unwrap_or(u64::MAX)
        }

        fn write_evidence(&self, name: &str, text: &str) {
            if let Some(dir) = &self.evidence {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join(name), text).unwrap();
            }
        }

        fn assert_secret_absent(&self, label: &str, text: &str) {
            for vendor in &self.vendors {
                assert!(!text.contains(&vendor.secret), "a credential appears in {label}");
            }
        }

        fn job_workdir(&self, job_id: &str) -> PathBuf {
            let dir = self.root.join("seller-jobs").join(job_id);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for vendor in &self.vendors {
                let _ = sh(&["docker", "rm", "--force", &vendor.container]);
            }
            for cfg in &self.cfgs {
                let names = holder_names(SEAT, &cfg.server_name);
                let _ = sh(&["docker", "rm", "--force", &names.container]);
                let _ = sh(&["docker", "volume", "rm", &names.runtime_volume, &names.state_volume]);
            }
            let _ = sh(&["docker", "network", "rm", &self.network]);
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn sandbox_policy(contained: bool) -> SandboxPolicy {
        let config = SandboxConfig {
            mode: SandboxMode::Docker,
            image: Some(env("MAXPLAYER_SANDBOX_IMAGE").unwrap_or_else(|| "maxplayer-sandbox:tools".into())),
            network: if contained { env("MAXPLAYER_HELD_TOOL_LIVE_NETWORK") } else { None },
            proxy_port_range: if contained { env("MAXPLAYER_HELD_TOOL_LIVE_PROXY_PORTS") } else { None },
            ..Default::default()
        };
        SandboxPolicy::from_config(Some(&config)).expect("the sandbox config resolves")
    }

    /// Start every configured holder, concurrently, as the daemon does.
    async fn start_all(fx: &Fixture) -> Vec<HeldTool> {
        let (uid, gid) = job_identity();
        let jobs_root = fx.root.join("seller-jobs");
        let started = futures_util::future::join_all(
            fx.cfgs.iter().map(|cfg| HeldTool::start(cfg, SEAT, &jobs_root, uid, gid)),
        )
        .await;
        started
            .into_iter()
            .zip(&fx.cfgs)
            .map(|(outcome, cfg)| outcome.unwrap_or_else(|e| panic!("the holder for {} starts: {e}", cfg.server_name)))
            .collect()
    }

    struct Dialogue {
        transcript: Vec<String>,
        replies: Vec<Value>,
        exit_ok: bool,
    }

    /// Drive one bridge, running as the job container's command, with `requests`; one reply line per
    /// request. On the blocking pool, because the caller's runtime thread must stay free.
    fn dialogue(program: &str, args: &[String], requests: Vec<Value>) -> Result<Dialogue, String> {
        use std::io::{BufRead, Write};
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawn docker run: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let mut stdout = std::io::BufReader::new(child.stdout.take().ok_or("no stdout")?);
        let mut transcript = Vec::new();
        let mut replies = Vec::new();
        for request in requests {
            let line = request.to_string();
            transcript.push(format!("-> {line}"));
            writeln!(stdin, "{line}").map_err(|e| format!("write: {e}"))?;
            stdin.flush().map_err(|e| format!("flush: {e}"))?;
            let mut reply = String::new();
            if stdout.read_line(&mut reply).map_err(|e| format!("read: {e}"))? == 0 {
                return Err("the bridge closed stdout before answering".into());
            }
            transcript.push(format!("<- {}", reply.trim_end()));
            replies.push(serde_json::from_str(reply.trim()).map_err(|e| format!("reply is not JSON: {e}"))?);
        }
        drop(stdin);
        let status = child.wait().map_err(|e| format!("wait: {e}"))?;
        Ok(Dialogue { transcript, replies, exit_ok: status.success() })
    }

    fn call(id: u64, name: &str, arguments: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
    }

    /// One job through the real launch with EVERY tool attached: the container gets one socket mount
    /// per tool, and its command is the bridge for the tool at `drive`, addressed exactly as the
    /// session entry addresses it (`--socket /run/holder/<tool>/job.sock`). Capture, remove, detach.
    /// Returns the dialogue, the `docker inspect` view of what the container was given, and the tool
    /// list if one was requested.
    async fn run_job(
        fx: &Fixture,
        tools: &[HeldTool],
        job_id: &str,
        contained: bool,
        drive: usize,
        requests: Vec<Value>,
    ) -> (Dialogue, Value, Vec<Value>) {
        let workdir = fx.job_workdir(job_id);
        let policy = sandbox_policy(contained);
        let identity = DeliveryAgentIdentity::for_seller(SEAT);
        let mut endpoints = Vec::new();
        for tool in tools {
            endpoints.push(tool.attach(job_id).await.expect("attach the job"));
        }
        let attachments = JobAttachments::merge(endpoints.iter().map(JobToolEndpoint::attachments));
        assert_eq!(attachments.extra_mounts.len(), tools.len(), "one socket mount per tool");
        assert_eq!(attachments.mcp_servers.len(), tools.len(), "one bridge entry per tool");
        // The agent's MCP client would spawn exactly this for the tool it calls.
        let McpServer::Stdio(entry) = &attachments.mcp_servers[drive] else { panic!("stdio entries") };
        let mut command = vec![entry.command.clone()];
        command.extend(entry.args.iter().cloned());
        let prepared = prepare_launch(&command, &policy, &workdir, &identity, Duration::from_secs(120))
            .await
            .expect("prepare the launch");
        let mut servers = prepared.mcp_servers.clone();
        servers.extend(attachments.mcp_servers.iter().cloned());
        let launch = policy
            .launch_with_mounts(
                &prepared.effective_command,
                &JobLaunch {
                    workdir: &workdir,
                    env: &prepared.env,
                    uid: prepared.uid,
                    gid: prepared.gid,
                    netns: prepared.holder_name.as_deref(),
                    mcp_servers: &servers,
                    resolv_conf: prepared.resolv_conf.as_deref(),
                },
                &attachments.extra_mounts,
            )
            .expect("build the docker argv");
        for arg in std::iter::once(&launch.program).chain(launch.args.iter()) {
            fx.assert_secret_absent("the docker argv", arg);
        }
        let container_name = job_container_name(&job_id_of(&workdir));
        let mut container = JobContainer::adopt(container_name.clone());
        let (program, args) = (launch.program.clone(), launch.args.clone());
        let dialogue = tokio::time::timeout(
            Duration::from_secs(180),
            tokio::task::spawn_blocking(move || dialogue(&program, &args, requests)),
        )
        .await
        .expect("the dialogue finishes")
        .expect("the dialogue task")
        .expect("the dialogue succeeds");

        // What the container was given.
        let inspected: Value = serde_json::from_str(&sh(&["docker", "inspect", &container_name]).expect("inspect"))
            .expect("inspect json");
        let view = json!({
            "Mounts": inspected[0]["Mounts"].as_array().map(|mounts| mounts.iter().map(|m| json!({
                "Type": m["Type"], "Name": m["Name"], "Source": m["Source"], "Destination": m["Destination"], "RW": m["RW"],
            })).collect::<Vec<_>>()),
            "Env": inspected[0]["Config"]["Env"],
            "Cmd": inspected[0]["Config"]["Cmd"],
            "NetworkMode": inspected[0]["HostConfig"]["NetworkMode"],
            "User": inspected[0]["Config"]["User"],
        });
        fx.assert_secret_absent("docker inspect of the job container", &view.to_string());
        fx.assert_secret_absent("the MCP transcript", &dialogue.transcript.join("\n"));

        // The real cleanup: capture, then remove.
        cleanup_job_container(
            std::mem::replace(&mut container, JobContainer::adopt("unused".into())),
            &workdir,
            workdir.join(crate::seller_git::SELLER_RUN_LOG),
            prepared.forwarded_secrets.clone(),
            CleanupPolicy::CaptureThenRemove,
        )
        .await;
        container.settle();
        for file in walk(&fx.root.join("seller-diagnostics")) {
            let text = String::from_utf8_lossy(&std::fs::read(&file).unwrap()).to_string();
            fx.assert_secret_absent(&file.display().to_string(), &text);
        }
        for endpoint in endpoints {
            endpoint.detach().await.expect("the job detaches");
        }
        let tool_list = dialogue
            .replies
            .iter()
            .find_map(|reply| reply["result"]["tools"].as_array().cloned())
            .unwrap_or_default();
        (dialogue, view, tool_list)
    }

    #[test]
    #[ignore = "needs docker, the kit image (maxplayer-tool-kit:demo) and the sandbox image with tool-mcp-bridge"]
    fn live_two_tools_serve_two_jobs_on_one_enrolment_each_and_a_restart_resumes_them() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let fx = Fixture::up(&TOOLS);
            for tool in 0..TOOLS.len() {
                assert_eq!(fx.login_count(tool).await, 0, "nothing has logged in yet");
            }

            // Boot: two holders start, each enrols ONCE with its own vendor, both healthy.
            let tools = start_all(&fx).await;
            let mut boot_lines = Vec::new();
            for (i, tool) in tools.iter().enumerate() {
                println!("{}", tool.boot_line());
                boot_lines.push(tool.boot_line());
                assert!(tool.status().healthy, "{:?}", tool.status());
                assert!(!tool.status().resumed_existing_session);
                assert_eq!(tool.status().enrollments_this_process, 1);
                assert_eq!(tool.server_name(), TOOLS[i]);
                assert_eq!(fx.login_count(i).await, 1, "vendor {i} saw exactly one login");
            }
            assert_ne!(tools[0].container(), tools[1].container(), "two tools, two holders");

            // Job A, uncontained, drives tool 0: initialize, list, transform, and an escape attempt.
            let job_a = "job-a-held-live";
            let workdir_a = fx.job_workdir(job_a);
            std::fs::write(workdir_a.join("input.txt"), "first job payload").unwrap();
            let (dialogue_a, view_a, tools_a) = run_job(&fx, &tools, job_a, false, 0, vec![
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}}}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
                call(3, "transform-file", json!({"input": "input.txt", "output": "out-a.txt", "mode": "upper"})),
                call(4, "transform-file", json!({"input": "../job-b-held-live/input.txt", "output": "stolen.txt", "mode": "upper"})),
            ])
            .await;
            assert!(dialogue_a.exit_ok, "the bridge exits cleanly once stdin closes");
            assert!(dialogue_a.replies[0].get("result").is_some(), "initialize: {}", dialogue_a.replies[0]);
            assert!(tools_a.iter().any(|t| t["name"] == json!("transform-file")), "the offering lists transform-file: {tools_a:?}");
            assert_eq!(dialogue_a.replies[2]["result"]["isError"], json!(false), "the transform ran: {}", dialogue_a.replies[2]);
            assert_eq!(std::fs::read_to_string(workdir_a.join("out-a.txt")).unwrap(), "FIRST JOB PAYLOAD");
            let escape = &dialogue_a.replies[3];
            assert!(
                escape.get("error").is_some() || escape["result"]["isError"] == json!(true),
                "a path outside the job directory must be refused: {escape}"
            );
            assert!(!workdir_a.join("stolen.txt").exists() && !fx.root.join("seller-jobs/stolen.txt").exists());
            // The container was given the workdir and ONE volume subpath PER TOOL, each at its own path,
            // and nothing of the holders'.
            let mounts_a = view_a["Mounts"].as_array().expect("mounts").clone();
            assert_eq!(mounts_a.len(), 1 + TOOLS.len(), "workdir + one socket directory per tool: {mounts_a:?}");
            for (i, tool) in tools.iter().enumerate() {
                let mount = mounts_a
                    .iter()
                    .find(|m| m["Destination"] == json!(job_socket_mount(TOOLS[i])))
                    .unwrap_or_else(|| panic!("the socket mount for {}", TOOLS[i]));
                assert_eq!(mount["Type"], json!("volume"));
                assert_eq!(mount["Name"], json!(tool.names().runtime_volume));
            }
            assert!(!view_a.to_string().contains(HOLDER_STATE_DIR), "no holder's state volume is mounted");
            assert!(!view_a.to_string().contains("cred-"), "no credential is mounted");

            // The same job drives tool 1 too: its own socket, its own vendor, its own output.
            let (dialogue_a2, _, _) = run_job(&fx, &tools, job_a, false, 1, vec![
                call(1, "transform-file", json!({"input": "input.txt", "output": "out-b.txt", "mode": "reverse"})),
            ])
            .await;
            assert_eq!(dialogue_a2.replies[0]["result"]["isError"], json!(false), "{}", dialogue_a2.replies[0]);
            assert_eq!(std::fs::read_to_string(workdir_a.join("out-b.txt")).unwrap(), "daolyap boj tsrif");
            assert_eq!(fx.stats(0).await["transform_count"], json!(1), "tool 0's vendor ran one transform");
            assert_eq!(fx.stats(1).await["transform_count"], json!(1), "tool 1's vendor ran one transform");

            // Job B, contained when the knobs say so, drives tool 1: the same offering, the same login.
            let job_b = "job-b-held-live";
            let workdir_b = fx.job_workdir(job_b);
            std::fs::write(workdir_b.join("input.txt"), "second job payload").unwrap();
            let contained = env("MAXPLAYER_HELD_TOOL_LIVE_NETWORK").is_some();
            let (dialogue_b, view_b, tools_b) = run_job(&fx, &tools, job_b, contained, 1, vec![
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
                call(2, "transform-file", json!({"input": "input.txt", "output": "out.txt", "mode": "upper"})),
            ])
            .await;
            assert_eq!(dialogue_b.replies[1]["result"]["isError"], json!(false), "{}", dialogue_b.replies[1]);
            assert_eq!(std::fs::read_to_string(workdir_b.join("out.txt")).unwrap(), "SECOND JOB PAYLOAD");
            assert_eq!(tools_a, tools_b, "both jobs see the seller-level offering, whole schema compared");
            if contained {
                assert!(view_b["NetworkMode"].as_str().unwrap_or("").starts_with("container:"), "{view_b}");
            }
            for tool in 0..TOOLS.len() {
                assert_eq!(fx.login_count(tool).await, 1, "two jobs, one login per vendor");
            }

            // Daemon stop, daemon start: every persisted login is resumed, none re-established.
            for tool in &tools {
                tool.shutdown().await.expect("the holder stops");
            }
            let tools = start_all(&fx).await;
            for (i, tool) in tools.iter().enumerate() {
                println!("{}", tool.boot_line());
                assert!(tool.status().healthy);
                assert!(tool.status().resumed_existing_session, "the state volume of {} carried the login", TOOLS[i]);
                assert_eq!(tool.status().enrollments_this_process, 0);
                assert_eq!(fx.login_count(i).await, 1, "a restart is not a login");
            }
            std::fs::write(workdir_b.join("input.txt"), "post restart payload").unwrap();
            let (dialogue_c, _, _) = run_job(&fx, &tools, job_b, false, 0, vec![
                call(1, "transform-file", json!({"input": "input.txt", "output": "out.txt", "mode": "upper"})),
            ])
            .await;
            assert_eq!(dialogue_c.replies[0]["result"]["isError"], json!(false), "{}", dialogue_c.replies[0]);
            assert_eq!(std::fs::read_to_string(workdir_b.join("out.txt")).unwrap(), "POST RESTART PAYLOAD");
            let final_stats: Vec<Value> = vec![fx.stats(0).await, fx.stats(1).await];
            for stats in &final_stats {
                assert_eq!(stats["login_count"], json!(1));
                assert_eq!(stats["auth_failures"], json!(0));
            }
            let restart_lines: Vec<String> = tools.iter().map(HeldTool::boot_line).collect();
            for tool in &tools {
                tool.shutdown().await.expect("the holder stops");
            }

            let summary = json!({
                "acceptance": "Holder route, TWO held tools, through the real daemon code: HeldTool::start/attach/shutdown per tool, prepare_launch, launch_with_mounts, cleanup capture",
                "tools": TOOLS,
                "holder_image": fx.cfgs[0].image,
                "sandbox_image": env("MAXPLAYER_SANDBOX_IMAGE").unwrap_or_else(|| "maxplayer-sandbox:tools".into()),
                "boot_lines_first_start": boot_lines,
                "boot_lines_after_restart": restart_lines,
                "vendor_stats_final": final_stats,
                "job_a_mounts": view_a["Mounts"],
                "job_b_network_mode": view_b["NetworkMode"],
                "tools_identical_for_both_jobs": tools_a == tools_b,
                "escape_attempt_reply": dialogue_a.replies[3],
                "credential_absent_from": ["docker argv", "docker inspect (both jobs)", "MCP transcripts", "diagnostics capture"],
            });
            fx.write_evidence("holder-summary.json", &serde_json::to_string_pretty(&summary).unwrap());
            fx.write_evidence("holder-job-a-tool-a-transcript.txt", &dialogue_a.transcript.join("\n"));
            fx.write_evidence("holder-job-a-tool-b-transcript.txt", &dialogue_a2.transcript.join("\n"));
            fx.write_evidence("holder-job-b-transcript.txt", &dialogue_b.transcript.join("\n"));
            fx.write_evidence("holder-job-b-after-restart-transcript.txt", &dialogue_c.transcript.join("\n"));
            fx.write_evidence("holder-job-a-inspect.json", &serde_json::to_string_pretty(&view_a).unwrap());
            fx.write_evidence("holder-job-b-inspect.json", &serde_json::to_string_pretty(&view_b).unwrap());
            if let Some(dir) = &fx.evidence {
                for file in walk(dir) {
                    let text = String::from_utf8_lossy(&std::fs::read(&file).unwrap()).to_string();
                    fx.assert_secret_absent(&file.display().to_string(), &text);
                }
            }
        });
    }

    /// The production path end to end with TWO held tools: a REAL agent turn (`claude-agent-acp`)
    /// driven by `run_agent_job_in_env` exactly as `execute_job` does. The agent must call each
    /// tool through its own socket bridge, and both files it asked for must appear. Needs the agent
    /// credential in this process's environment (`CLAUDE_CODE_OAUTH_TOKEN`).
    #[test]
    #[ignore = "needs docker, the kit image, the sandbox image with tool-mcp-bridge, and an agent credential"]
    fn live_a_real_agent_turn_uses_two_held_tools_through_their_socket_bridges() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            assert!(
                crate::seller_exec::FORWARDED_AGENT_ENV.iter().any(|name| env(name).is_some()),
                "an agent credential (e.g. CLAUDE_CODE_OAUTH_TOKEN) must be in this process's environment"
            );
            let fx = Fixture::up(&TOOLS);
            let tools = start_all(&fx).await;
            for tool in &tools {
                assert!(tool.status().healthy);
            }
            let job_id = "job-agent-held-live";
            let workdir = fx.job_workdir(job_id);
            let identity = DeliveryAgentIdentity::for_seller(SEAT);
            crate::seller_git::init_empty_delivery_workdir_off_runtime(workdir.clone(), identity.clone())
                .await
                .expect("init the job workdir");
            std::fs::write(workdir.join("input.txt"), "agent payload").unwrap();
            let mut endpoints = Vec::new();
            for tool in &tools {
                endpoints.push(tool.attach(job_id).await.expect("attach"));
            }
            let attachments = JobAttachments::merge(endpoints.iter().map(JobToolEndpoint::attachments));
            let contained = env("MAXPLAYER_HELD_TOOL_LIVE_NETWORK").is_some();
            let policy = sandbox_policy(contained);
            let prompt = "You have two MCP servers, `text-a` and `text-b`, each with one tool, `transform-file`. \
                          Call `transform-file` on server `text-a` exactly once with arguments input=\"input.txt\", \
                          output=\"out-a.txt\", mode=\"upper\". Then call `transform-file` on server `text-b` exactly \
                          once with arguments input=\"input.txt\", output=\"out-b.txt\", mode=\"reverse\". Do not read, \
                          create, edit or delete any file yourself, and do not call any other tool. Then reply with \
                          exactly one line: `done`.";
            let report = crate::seller_exec::run_agent_job_in_env(
                &["claude-agent-acp".to_owned()],
                &policy,
                prompt,
                &workdir,
                &identity,
                crate::seller_exec::AgentRunTimeout::JobDeadline(Duration::from_secs(420)),
                None,
                attachments,
            )
            .await
            .expect("the agent turn completes");
            for endpoint in endpoints {
                endpoint.detach().await.expect("the job detaches");
            }
            let out_a = std::fs::read_to_string(workdir.join("out-a.txt")).expect("tool text-a wrote its output");
            let out_b = std::fs::read_to_string(workdir.join("out-b.txt")).expect("tool text-b wrote its output");
            assert_eq!(out_a, "AGENT PAYLOAD");
            assert_eq!(out_b, "daolyap tnega");
            let wire = walk(&fx.root.join("seller-diagnostics"))
                .into_iter()
                .filter(|f| f.file_name().is_some_and(|n| n == "logs.txt"))
                .map(|f| std::fs::read_to_string(&f).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(wire.contains("mcp__text-a__transform-file"), "the agent called tool text-a through its bridge");
            assert!(wire.contains("mcp__text-b__transform-file"), "the agent called tool text-b through its bridge");
            fx.assert_secret_absent("the ACP wire", &wire);
            fx.assert_secret_absent("the agent's message", report.last_agent_message.as_deref().unwrap_or(""));
            let stats: Vec<Value> = vec![fx.stats(0).await, fx.stats(1).await];
            for s in &stats {
                assert_eq!(s["login_count"], json!(1));
                assert_eq!(s["transform_count"], json!(1));
            }
            for tool in &tools {
                tool.shutdown().await.expect("the holder stops");
            }
            fx.write_evidence(
                "holder-agent-summary.json",
                &serde_json::to_string_pretty(&json!({
                    "acceptance": "a real claude-agent-acp turn used TWO held tools, each through its own tool-mcp-bridge over its own socket",
                    "tools": TOOLS,
                    "out_a": out_a,
                    "out_b": out_b,
                    "last_agent_message": report.last_agent_message,
                    "usage": report.usage,
                    "tool_calls_on_the_acp_wire": ["mcp__text-a__transform-file", "mcp__text-b__transform-file"],
                    "vendor_stats": stats,
                }))
                .unwrap(),
            );
            fx.write_evidence("holder-agent-acp-wire.txt", &wire);
        });
    }
}
