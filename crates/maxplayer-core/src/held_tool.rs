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
//!   and the job container mounts exactly that directory at `/run/holder`. The agent reaches it
//!   through `tool-mcp-bridge`, baked into the sandbox image, as a stdio MCP server. Job end
//!   DETACHES the socket; the tool stays enrolled.
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
/// Where a job container mounts its own socket directory. The bridge's default socket path is
/// `/run/holder/job.sock`, so no environment is needed.
pub const JOB_SOCKET_MOUNT: &str = "/run/holder";
/// The socket bridge inside the sandbox image (`docker/maxplayer-sandbox/Dockerfile`).
pub const CONTAINER_TOOL_BRIDGE_BIN: &str = "/usr/local/bin/tool-mcp-bridge";
/// The MCP server name the agent sees when the seat names none.
pub const DEFAULT_SERVER_NAME: &str = "seller-tool";
/// The label every holder container carries, valued with the seat's pubkey hex, so a stale holder
/// can be attributed to the seat that leaked it.
pub const HOLDER_LABEL: &str = "maxplayer.held-tool.seat";

/// How long boot waits for the holder to answer `status` — enrolment against a vendor is inside it.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const START_POLL: Duration = Duration::from_millis(500);

/// The docker names one seat's holder uses: the container, and its two volumes. Derived from the
/// seat, never random, for the reason `sandbox_netns::holder_name` gives: a stale one can be
/// attributed, and a second daemon on the same seat collides loudly instead of leaking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolderNames {
    pub container: String,
    pub state_volume: String,
    pub runtime_volume: String,
}

/// The names for `seat` (the seller pubkey hex; the first 16 characters are the suffix).
pub fn holder_names(seat: &str) -> HolderNames {
    let suffix: String = seat.chars().take(16).collect();
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

/// One `docker` CLI run on the blocking pool: exit code, stdout, stderr.
async fn docker(argv: Vec<String>) -> Result<(i32, String, String), String> {
    tokio::task::spawn_blocking(move || {
        let (program, args) = argv.split_first().ok_or("an empty docker argv")?;
        let done = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| format!("could not run `{program}`: {error}"))?;
        Ok((
            done.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&done.stdout).trim().to_owned(),
            String::from_utf8_lossy(&done.stderr).trim().to_owned(),
        ))
    })
    .await
    .map_err(|error| format!("docker task panicked: {error}"))?
}

/// [`docker`], succeeding only on exit 0; the error carries the command's own words.
async fn docker_ok(argv: Vec<String>) -> Result<String, String> {
    let shown = argv.join(" ");
    let (code, stdout, stderr) = docker(argv).await?;
    if code == 0 {
        Ok(stdout)
    } else {
        Err(format!(
            "`{shown}` exited {code}: {}",
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }
}

/// The seat's held tool: a running holder container the daemon owns for its whole life.
///
/// Dropping it removes the container (blocking, like `NetnsHolder`), unless [`Self::shutdown`]
/// already did so politely. The state volume is never removed here: it holds the vendor login the
/// next boot resumes, which is the whole point of the enroll-once model.
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
    /// Start the seat's holder and wait until it answers, enrolled or resumed.
    ///
    /// `jobs_root` is the seat's `seller-jobs` directory on the host; `uid`/`gid` the identity job
    /// containers run as ([`crate::seller_exec::job_identity`]). Fails — and the caller decides
    /// whether that refuses the boot — when a host path is missing, the image cannot run, the holder
    /// exits before answering (an enrolment failure exits it), or this daemon cannot mount a volume
    /// subpath. A holder that runs but reports UNHEALTHY starts successfully; the status says so.
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
            return Err("[sandbox] held_tool: image must not be empty".into());
        }
        std::fs::create_dir_all(jobs_root)
            .map_err(|error| format!("[sandbox] held_tool: cannot create {}: {error}", jobs_root.display()))?;

        let names = holder_names(seat);
        // A stale holder from a daemon that died without its shutdown path: remove it by name, so
        // this boot's container is the one the name addresses.
        let _ = docker(vec!["docker".into(), "rm".into(), "--force".into(), names.container.clone()]).await;
        for volume in [&names.state_volume, &names.runtime_volume] {
            docker_ok(vec!["docker".into(), "volume".into(), "create".into(), volume.clone()]).await?;
        }
        docker_ok(volume_init_argv(&cfg.image, &names, uid, gid)).await?;
        docker_ok(holder_run_argv(cfg, &names, seat, jobs_root, uid, gid)).await?;

        // Wait for `status`. The holder enrols before it binds its control socket, so the wait
        // covers a real login against the vendor.
        let started = std::time::Instant::now();
        let status = loop {
            let (code, stdout, stderr) = docker(holderctl_argv(&names.container, &["status"])).await?;
            if (code == 0 || code == 1)
                && !stdout.is_empty()
                && let Ok(status) = parse_status(&stdout)
            {
                break status;
            }
            // Gone already? Then its own last words are the diagnosis (an enrolment failure).
            let (_, state, _) = docker(vec![
                "docker".into(),
                "inspect".into(),
                "--format".into(),
                "{{.State.Status}}".into(),
                names.container.clone(),
            ])
            .await?;
            if state == "exited" || state == "dead" {
                let (_, logs_out, logs_err) =
                    docker(vec!["docker".into(), "logs".into(), "--tail".into(), "20".into(), names.container.clone()]).await?;
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
        docker_ok(subpath_probe_argv(&cfg.image, &names)).await.map_err(|error| {
            format!(
                "[sandbox] held_tool: this docker daemon cannot mount a volume subpath, which every \
                 job's socket mount needs (Docker Engine 26 or newer): {error}"
            )
        })?;

        Ok(Self {
            seat: seat.to_owned(),
            names,
            image: cfg.image.clone(),
            server_name: cfg
                .server_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(DEFAULT_SERVER_NAME)
                .to_owned(),
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
    /// the directory MUST exist before this call, because the holder canonicalizes it).
    pub async fn attach(&self, job_id: &str) -> Result<JobToolEndpoint, String> {
        let job_root = format!("{HOLDER_JOBS_DIR}/{job_id}");
        let stdout = docker_ok(holderctl_argv(
            &self.names.container,
            &["attach", "--job-id", job_id, "--job-root", &job_root],
        ))
        .await
        .map_err(|error| format!("[sandbox] held_tool: attach {job_id} failed: {error}"))?;
        let reply: Value = serde_json::from_str(stdout.trim())
            .map_err(|error| format!("[sandbox] held_tool: attach reply is not JSON: {error}"))?;
        let expected_socket = format!("{HOLDER_RUNTIME_DIR}/jobs/{job_id}/job.sock");
        if reply["socket"].as_str() != Some(expected_socket.as_str()) {
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
    pub async fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = docker(holderctl_argv(&self.names.container, &["shutdown"])).await;
        if let Err(error) = docker_ok(vec![
            "docker".into(),
            "rm".into(),
            "--force".into(),
            self.names.container.clone(),
        ])
        .await
        {
            eprintln!("seller node: [sandbox] held_tool: could not remove {}: {error}", self.names.container);
        }
        let _ = docker(vec!["docker".into(), "volume".into(), "rm".into(), self.names.runtime_volume.clone()]).await;
        eprintln!(
            "seller node: [sandbox] held_tool: holder {} stopped; the login persists in volume {} for the next boot",
            self.names.container, self.names.state_volume
        );
    }

    pub fn seat(&self) -> &str {
        &self.seat
    }
}

impl Drop for HeldTool {
    /// The backstop for a daemon that never reached [`Self::shutdown`]: remove the container,
    /// blocking, for the reason `NetnsHolder::drop` gives — a task spawned from `Drop` can be
    /// discarded when the runtime shuts down, and that is exactly the path an aborted daemon takes.
    fn drop(&mut self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let outcome = std::process::Command::new("docker")
            .args(["rm", "--force", &self.names.container])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output();
        if let Ok(done) = outcome
            && !done.status.success()
        {
            eprintln!(
                "seller node: [sandbox] held_tool: fallback removal of {} failed: {}",
                self.names.container,
                String::from_utf8_lossy(&done.stderr).trim()
            );
        }
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
    /// What the job gets: its own socket directory mounted at [`JOB_SOCKET_MOUNT`] (a subpath of the
    /// holder's runtime volume, so no other job's socket and none of the holder's state come with
    /// it), and one stdio MCP server entry that spawns the bridge. The bridge reads
    /// `/run/holder/job.sock` by default, so the entry carries no arguments and no environment.
    pub fn attachments(&self) -> JobAttachments {
        JobAttachments {
            mcp_servers: vec![McpServer::Stdio(McpServerStdio {
                name: self.server_name.clone(),
                command: CONTAINER_TOOL_BRIDGE_BIN.to_owned(),
                args: Vec::new(),
                env: Vec::new(),
            })],
            extra_mounts: vec![ExtraMount::VolumeSubpath {
                volume: self.runtime_volume.clone(),
                subpath: format!("jobs/{}", self.job_id),
                container: JOB_SOCKET_MOUNT.to_owned(),
            }],
        }
    }

    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    /// Detach: the holder closes and removes this job's socket. The tool stays enrolled — the
    /// holder's reply says so, and that is the property the corrected model turns on. Best effort:
    /// a failure is logged, because the job is over either way.
    pub async fn detach(mut self) {
        self.detached = true;
        match docker_ok(holderctl_argv(&self.container, &["detach", "--job-id", &self.job_id])).await {
            Ok(reply) => {
                if !reply.contains("\"tool_still_enrolled\": true") {
                    eprintln!(
                        "seller node: [sandbox] held_tool: detach {} did not confirm the tool stayed enrolled: {reply}",
                        self.job_id
                    );
                }
            }
            Err(error) => eprintln!("seller node: [sandbox] held_tool: detach {} failed: {error}", self.job_id),
        }
    }
}

impl Drop for JobToolEndpoint {
    /// A job that left without detaching (a panic, an early `?`) still gets its socket removed. Off
    /// the runtime, on its own thread: `Drop` cannot await, and the holder call is a blocking exec.
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        let argv = holderctl_argv(&self.container, &["detach", "--job-id", &self.job_id]);
        std::thread::spawn(move || {
            let (program, args) = argv.split_first().expect("a docker argv");
            let _ = std::process::Command::new(program)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HeldToolConfig {
        HeldToolConfig {
            image: "my-holder:latest".into(),
            config: "/etc/maxplayer/seller-tool-config.json".into(),
            credential_file: "/home/seller/.config/maxplayer/vendor-cred.json".into(),
            vendor_base_url: Some("http://vendor:8080".into()),
            vendor_cli: None,
            network: Some("maxplayer-tools".into()),
            server_name: None,
            required: false,
        }
    }

    const SEAT: &str = "25f6b60a3e3870d5533b7f08133fc1cdff4c43bad2c30974faec8b059dde019f";

    #[test]
    fn the_names_derive_from_the_seat_and_nothing_random() {
        let names = holder_names(SEAT);
        assert_eq!(names.container, "maxplayer-held-tool-25f6b60a3e3870d5");
        assert_eq!(names.state_volume, "maxplayer-held-tool-state-25f6b60a3e3870d5");
        assert_eq!(names.runtime_volume, "maxplayer-held-tool-runtime-25f6b60a3e3870d5");
        assert_eq!(holder_names(SEAT), names, "the same seat always names the same holder");
    }

    /// The holder is handed exactly five mounts — config and credential read-only, two volumes,
    /// the jobs directory — runs as the job uid, carries no environment, and is not `--rm`.
    #[test]
    fn the_holder_mounts_what_it_needs_read_only_and_runs_as_the_job_uid() {
        let names = holder_names(SEAT);
        let argv = holder_run_argv(&cfg(), &names, SEAT, Path::new("/home/seller/.maxplayer/seller-jobs"), 501, 20);
        let text = argv.join(" ");
        assert!(text.starts_with("docker run -d --name maxplayer-held-tool-25f6b60a3e3870d5 "));
        assert!(text.contains(" --label maxplayer.held-tool.seat=25f6b60a3e3870d5533b7f08133fc1cdff4c43bad2c30974faec8b059dde019f "));
        assert!(text.contains(" --user 501:20 "));
        assert!(text.contains(" --network maxplayer-tools "));
        assert!(text.contains(" -v /etc/maxplayer/seller-tool-config.json:/etc/maxplayer/seller-tool-config.json:ro "));
        assert!(text.contains(" -v /home/seller/.config/maxplayer/vendor-cred.json:/run/secrets/cred.json:ro "));
        assert!(text.contains(" -v maxplayer-held-tool-state-25f6b60a3e3870d5:/var/lib/maxplayer-holder "));
        assert!(text.contains(" -v maxplayer-held-tool-runtime-25f6b60a3e3870d5:/run/maxplayer-holder "));
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
        let argv = holder_run_argv(&bare, &holder_names(SEAT), SEAT, Path::new("/jobs"), 1000, 1000);
        assert!(!argv.iter().any(|a| a == "--network"));
        assert!(!argv.iter().any(|a| a == "--vendor-base-url"));
        let mut with_cli = cfg();
        with_cli.vendor_cli = Some("/opt/vendor/bin/vendorcli".into());
        let argv = holder_run_argv(&with_cli, &holder_names(SEAT), SEAT, Path::new("/jobs"), 1000, 1000);
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
        let names = holder_names(SEAT);
        let init = volume_init_argv("my-holder:latest", &names, 501, 20).join(" ");
        assert!(init.contains(" --rm --user 0:0 --entrypoint sh "));
        assert!(init.ends_with(" my-holder:latest -c chown 501:20 /var/lib/maxplayer-holder /run/maxplayer-holder"));
        let probe = subpath_probe_argv("my-holder:latest", &names).join(" ");
        assert!(probe.contains("--entrypoint true"));
        assert!(probe.contains(
            "--mount type=volume,src=maxplayer-held-tool-runtime-25f6b60a3e3870d5,dst=/probe,volume-subpath=jobs"
        ));
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

    /// The job gets the bridge entry (no args, no env: the socket path is the bridge's default) and
    /// ONE mount: the holder runtime volume's `jobs/<job>` subpath, at `/run/holder`.
    #[test]
    fn a_job_endpoint_attaches_one_socket_mount_and_one_bridge_entry() {
        let endpoint = JobToolEndpoint {
            container: "maxplayer-held-tool-abc".into(),
            runtime_volume: "maxplayer-held-tool-runtime-abc".into(),
            job_id: "job-1".into(),
            server_name: "seller-tool".into(),
            detached: true, // no docker call on drop in a unit test
        };
        let attachments = endpoint.attachments();
        assert_eq!(
            attachments.mcp_servers,
            vec![McpServer::Stdio(McpServerStdio {
                name: "seller-tool".into(),
                command: "/usr/local/bin/tool-mcp-bridge".into(),
                args: Vec::new(),
                env: Vec::new(),
            })]
        );
        assert_eq!(
            attachments.extra_mounts,
            vec![ExtraMount::VolumeSubpath {
                volume: "maxplayer-held-tool-runtime-abc".into(),
                subpath: "jobs/job-1".into(),
                container: "/run/holder".into(),
            }]
        );
        assert_eq!(
            attachments.extra_mounts[0].argv(),
            vec![
                "--mount",
                "type=volume,src=maxplayer-held-tool-runtime-abc,dst=/run/holder,volume-subpath=jobs/job-1",
            ]
        );
    }
}

/// LIVE end-to-end proof of the Holder route through the REAL daemon code: [`HeldTool::start`],
/// [`HeldTool::attach`], the real `prepare_launch` and `launch_with_mounts`, the sandbox image with
/// the bridge, and the real cleanup capture — against the kit's fake vendor, whose counters are the
/// independent oracle. `#[ignore]`d: it needs docker, the kit image, and the sandbox image.
///
///   cargo test -p maxplayer-core --features wallet,acp --lib -- --ignored --nocapture held_tool::live
///
/// Knobs: MAXPLAYER_HELD_TOOL_LIVE_IMAGE (default `maxplayer-tool-kit:demo`), MAXPLAYER_SANDBOX_IMAGE
/// (default `maxplayer-sandbox:tools`), MAXPLAYER_HELD_TOOL_LIVE_NETWORK + MAXPLAYER_HELD_TOOL_LIVE_PROXY_PORTS
/// (run job B under egress containment), MAXPLAYER_HELD_TOOL_LIVE_EVIDENCE_DIR (write the record there).
///
/// Synthetic throughout: the credential is generated here and exists only inside the fake vendor.
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

    /// The whole fixture: a scratch home, the synthetic credential, the tool config, a docker network,
    /// and the fake vendor published on loopback so the HOST reads its counters. Torn down on drop.
    struct Fixture {
        root: PathBuf,
        network: String,
        vendor: String,
        vendor_url: String,
        secret: String,
        cfg: HeldToolConfig,
        evidence: Option<PathBuf>,
    }

    impl Fixture {
        fn up() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let tag = format!("{}-{stamp}", std::process::id());
            let root = std::env::temp_dir().join(format!("maxplayer-held-tool-live-{tag}"));
            std::fs::create_dir_all(root.join("seller-jobs")).unwrap();
            let image = env("MAXPLAYER_HELD_TOOL_LIVE_IMAGE").unwrap_or_else(|| "maxplayer-tool-kit:demo".into());

            // A synthetic credential, generated now, never on a command line.
            let mut raw = [0u8; 12];
            getrandom::fill(&mut raw).unwrap();
            let secret = format!("synthetic-held-tool-secret-{}", hex::encode(raw));
            let credential_file = root.join("cred.json");
            std::fs::write(
                &credential_file,
                json!({"client_id": "synthetic-seller-client", "client_secret": secret}).to_string(),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&credential_file, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            // The kit's fixture offering, copied so the holder gets an absolute host path.
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../maxplayer-tool-kit/fixtures/seller-tool-config.json");
            let config = root.join("seller-tool-config.json");
            std::fs::copy(&fixture, &config).expect("copy the kit fixture config");

            let network = format!("mx-held-live-{tag}");
            sh(&["docker", "network", "create", &network]).expect("create the test network");
            let vendor = format!("mx-held-vendor-{tag}");
            sh(&[
                "docker", "run", "-d", "--name", &vendor, "--network", &network, "--network-alias", "vendor",
                "-p", "127.0.0.1:0:8080",
                "-v", &format!("{}:/run/secrets/cred.json:ro", credential_file.display()),
                &image, "vendor-service", "--listen", "0.0.0.0:8080", "--credential-file", "/run/secrets/cred.json",
            ])
            .expect("start the fake vendor");
            let port = sh(&["docker", "port", &vendor, "8080/tcp"]).expect("vendor port");
            let vendor_url = format!("http://{}", port.lines().next().unwrap().trim());
            let this = Self {
                root,
                network: network.clone(),
                vendor,
                vendor_url,
                secret,
                cfg: HeldToolConfig {
                    image,
                    config,
                    credential_file,
                    vendor_base_url: Some("http://vendor:8080".into()),
                    vendor_cli: None,
                    network: Some(network),
                    server_name: None,
                    required: true,
                },
                evidence: env("MAXPLAYER_HELD_TOOL_LIVE_EVIDENCE_DIR").map(PathBuf::from),
            };
            // The vendor answers before anything depends on it.
            for _ in 0..50 {
                if this.stats_blocking().is_ok() {
                    return this;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            panic!("the fake vendor did not come up");
        }

        /// A plain-socket GET of the vendor's counters, for the readiness poll inside `up()`. Plain
        /// std, not reqwest's blocking client: that client owns a runtime of its own, and dropping it
        /// inside this test's runtime is what tokio refuses.
        fn stats_blocking(&self) -> Result<Value, String> {
            use std::io::{Read, Write};
            let authority = self.vendor_url.trim_start_matches("http://");
            let mut stream = std::net::TcpStream::connect(authority).map_err(|e| e.to_string())?;
            stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
            write!(stream, "GET /admin/stats HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .map_err(|e| e.to_string())?;
            let mut raw = String::new();
            stream.read_to_string(&mut raw).map_err(|e| e.to_string())?;
            let body = raw.split_once("\r\n\r\n").map(|(_, body)| body).ok_or("no body")?;
            serde_json::from_str(body.trim()).map_err(|e| e.to_string())
        }

        async fn stats(&self) -> Value {
            let url = format!("{}/admin/stats", self.vendor_url);
            let body = reqwest::get(&url).await.expect("vendor stats").text().await.expect("stats body");
            serde_json::from_str(&body).expect("stats json")
        }

        async fn login_count(&self) -> u64 {
            self.stats().await["login_count"].as_u64().unwrap_or(u64::MAX)
        }

        fn write_evidence(&self, name: &str, text: &str) {
            if let Some(dir) = &self.evidence {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(dir.join(name), text).unwrap();
            }
        }

        fn assert_secret_absent(&self, label: &str, text: &str) {
            assert!(!text.contains(&self.secret), "the credential appears in {label}");
        }

        fn job_workdir(&self, job_id: &str) -> PathBuf {
            let dir = self.root.join("seller-jobs").join(job_id);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = sh(&["docker", "rm", "--force", &self.vendor]);
            let names = holder_names(SEAT);
            let _ = sh(&["docker", "rm", "--force", &names.container]);
            let _ = sh(&["docker", "volume", "rm", &names.runtime_volume, &names.state_volume]);
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

    struct Dialogue {
        transcript: Vec<String>,
        replies: Vec<Value>,
        exit_ok: bool,
    }

    /// Drive the bridge, running as the job container's command, with `requests`; one reply line per
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

    /// One job through the real launch: attach, launch the bridge as the container command with the
    /// endpoint's attachments, hold `requests`, capture and remove the container, detach. Returns the
    /// dialogue and the `docker inspect` view of what the container was given.
    async fn run_job(
        fx: &Fixture,
        tool: &HeldTool,
        job_id: &str,
        contained: bool,
        requests: Vec<Value>,
    ) -> (Dialogue, Value, Vec<Value>) {
        let workdir = fx.job_workdir(job_id);
        let policy = sandbox_policy(contained);
        let identity = DeliveryAgentIdentity::for_seller(SEAT);
        let endpoint = tool.attach(job_id).await.expect("attach the job");
        let attachments = endpoint.attachments();
        let prepared = prepare_launch(&[CONTAINER_TOOL_BRIDGE_BIN.to_owned()], &policy, &workdir, &identity, Duration::from_secs(120))
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
        endpoint.detach().await;
        let tools = dialogue
            .replies
            .iter()
            .find_map(|reply| reply["result"]["tools"].as_array().cloned())
            .unwrap_or_default();
        (dialogue, view, tools)
    }

    #[test]
    #[ignore = "needs docker, the kit image (maxplayer-tool-kit:demo) and the sandbox image with tool-mcp-bridge"]
    fn live_two_jobs_share_one_enrolment_and_a_daemon_restart_resumes_it() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let fx = Fixture::up();
            let (uid, gid) = job_identity();
            let jobs_root = fx.root.join("seller-jobs");
            assert_eq!(fx.login_count().await, 0, "nothing has logged in yet");

            // Boot: the holder starts, enrols ONCE, and is healthy.
            let tool = HeldTool::start(&fx.cfg, SEAT, &jobs_root, uid, gid).await.expect("the holder starts");
            println!("{}", tool.boot_line());
            assert!(tool.status().healthy, "{:?}", tool.status());
            assert!(!tool.status().resumed_existing_session);
            assert_eq!(tool.status().enrollments_this_process, 1);
            assert_eq!(fx.login_count().await, 1, "the vendor saw exactly one login");
            let boot_line_1 = tool.boot_line();

            // Job A, uncontained: initialize, list, transform, and an escape attempt.
            let job_a = "job-a-held-live";
            let workdir_a = fx.job_workdir(job_a);
            std::fs::write(workdir_a.join("input.txt"), "first job payload").unwrap();
            let (dialogue_a, view_a, tools_a) = run_job(&fx, &tool, job_a, false, vec![
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}}}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
                call(3, "transform-file", json!({"input": "input.txt", "output": "out.txt", "mode": "upper"})),
                call(4, "transform-file", json!({"input": "../job-b-held-live/input.txt", "output": "stolen.txt", "mode": "upper"})),
            ])
            .await;
            assert!(dialogue_a.exit_ok, "the bridge exits cleanly once stdin closes");
            assert!(dialogue_a.replies[0].get("result").is_some(), "initialize: {}", dialogue_a.replies[0]);
            assert!(tools_a.iter().any(|t| t["name"] == json!("transform-file")), "the offering lists transform-file: {tools_a:?}");
            assert_eq!(dialogue_a.replies[2]["result"]["isError"], json!(false), "the transform ran: {}", dialogue_a.replies[2]);
            assert_eq!(std::fs::read_to_string(workdir_a.join("out.txt")).unwrap(), "FIRST JOB PAYLOAD");
            let escape = &dialogue_a.replies[3];
            assert!(
                escape.get("error").is_some() || escape["result"]["isError"] == json!(true),
                "a path outside the job directory must be refused: {escape}"
            );
            assert!(!workdir_a.join("stolen.txt").exists() && !fx.root.join("seller-jobs/stolen.txt").exists());
            // The container was given the workdir and ONE volume subpath, and nothing of the holder's.
            let mounts_a = view_a["Mounts"].as_array().expect("mounts").clone();
            assert_eq!(mounts_a.len(), 2, "workdir + the job's socket directory, nothing else: {mounts_a:?}");
            let socket_mount = mounts_a.iter().find(|m| m["Destination"] == json!(JOB_SOCKET_MOUNT)).expect("the socket mount");
            assert_eq!(socket_mount["Type"], json!("volume"));
            assert_eq!(socket_mount["Name"], json!(tool.names().runtime_volume));
            assert!(!view_a.to_string().contains(HOLDER_STATE_DIR), "the holder's state volume is not mounted");
            assert!(!view_a.to_string().contains("cred.json"), "the credential is not mounted");
            assert_eq!(fx.login_count().await, 1, "job A caused no login");

            // Job B, contained when the knobs say so: the same offering, the same login, its own socket.
            let job_b = "job-b-held-live";
            let workdir_b = fx.job_workdir(job_b);
            std::fs::write(workdir_b.join("input.txt"), "second job payload").unwrap();
            let contained = env("MAXPLAYER_HELD_TOOL_LIVE_NETWORK").is_some();
            let (dialogue_b, view_b, tools_b) = run_job(&fx, &tool, job_b, contained, vec![
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
                call(2, "transform-file", json!({"input": "input.txt", "output": "out.txt", "mode": "reverse"})),
            ])
            .await;
            assert_eq!(dialogue_b.replies[1]["result"]["isError"], json!(false), "{}", dialogue_b.replies[1]);
            assert_eq!(std::fs::read_to_string(workdir_b.join("out.txt")).unwrap(), "daolyap boj dnoces");
            assert_eq!(tools_a, tools_b, "both jobs see the seller-level offering, whole schema compared");
            if contained {
                assert!(view_b["NetworkMode"].as_str().unwrap_or("").starts_with("container:"), "{view_b}");
            }
            assert_eq!(fx.login_count().await, 1, "two jobs, one login");

            // Daemon stop, daemon start: the persisted login is resumed, not re-established.
            tool.shutdown().await;
            let tool = HeldTool::start(&fx.cfg, SEAT, &jobs_root, uid, gid).await.expect("the holder restarts");
            println!("{}", tool.boot_line());
            assert!(tool.status().healthy);
            assert!(tool.status().resumed_existing_session, "the state volume carried the login");
            assert_eq!(tool.status().enrollments_this_process, 0);
            assert_eq!(fx.login_count().await, 1, "a restart is not a login");
            std::fs::write(workdir_b.join("input.txt"), "post restart payload").unwrap();
            let (dialogue_c, _, _) = run_job(&fx, &tool, job_b, false, vec![
                call(1, "transform-file", json!({"input": "input.txt", "output": "out.txt", "mode": "upper"})),
            ])
            .await;
            assert_eq!(dialogue_c.replies[0]["result"]["isError"], json!(false), "{}", dialogue_c.replies[0]);
            assert_eq!(std::fs::read_to_string(workdir_b.join("out.txt")).unwrap(), "POST RESTART PAYLOAD");
            let final_stats = fx.stats().await;
            assert_eq!(final_stats["login_count"], json!(1));
            assert_eq!(final_stats["auth_failures"], json!(0));
            tool.shutdown().await;

            let summary = json!({
                "acceptance": "Holder route through the real daemon code: HeldTool::start/attach/shutdown, prepare_launch, launch_with_mounts, cleanup capture",
                "holder_image": fx.cfg.image,
                "sandbox_image": env("MAXPLAYER_SANDBOX_IMAGE").unwrap_or_else(|| "maxplayer-sandbox:tools".into()),
                "boot_line_first_start": boot_line_1,
                "boot_line_after_restart": tool.boot_line(),
                "vendor_stats_final": final_stats,
                "job_a_mounts": view_a["Mounts"],
                "job_b_network_mode": view_b["NetworkMode"],
                "tools_identical_for_both_jobs": tools_a == tools_b,
                "escape_attempt_reply": dialogue_a.replies[3],
                "credential_absent_from": ["docker argv", "docker inspect (both jobs)", "MCP transcripts", "diagnostics capture"],
            });
            fx.write_evidence("holder-summary.json", &serde_json::to_string_pretty(&summary).unwrap());
            fx.write_evidence("holder-job-a-transcript.txt", &dialogue_a.transcript.join("\n"));
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

    /// The production path end to end: a REAL agent turn (`claude-agent-acp`) driven by
    /// `run_agent_job_in_env` with the held tool attached, exactly as `execute_job` does. The agent
    /// must call the seller's tool through the socket bridge, and the file it asked for must appear.
    /// Needs the agent credential in this process's environment (`CLAUDE_CODE_OAUTH_TOKEN`).
    #[test]
    #[ignore = "needs docker, the kit image, the sandbox image with tool-mcp-bridge, and an agent credential"]
    fn live_a_real_agent_turn_uses_the_held_tool_through_the_socket_bridge() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            assert!(
                crate::seller_exec::FORWARDED_AGENT_ENV.iter().any(|name| env(name).is_some()),
                "an agent credential (e.g. CLAUDE_CODE_OAUTH_TOKEN) must be in this process's environment"
            );
            let fx = Fixture::up();
            let (uid, gid) = job_identity();
            let tool = HeldTool::start(&fx.cfg, SEAT, &fx.root.join("seller-jobs"), uid, gid).await.expect("the holder starts");
            assert!(tool.status().healthy);
            let job_id = "job-agent-held-live";
            let workdir = fx.job_workdir(job_id);
            let identity = DeliveryAgentIdentity::for_seller(SEAT);
            crate::seller_git::init_empty_delivery_workdir_off_runtime(workdir.clone(), identity.clone())
                .await
                .expect("init the job workdir");
            std::fs::write(workdir.join("input.txt"), "agent payload").unwrap();
            let endpoint = tool.attach(job_id).await.expect("attach");
            let attachments = endpoint.attachments();
            let contained = env("MAXPLAYER_HELD_TOOL_LIVE_NETWORK").is_some();
            let policy = sandbox_policy(contained);
            let prompt = "You have an MCP server named `seller-tool` with one tool, `transform-file`. Call it exactly \
                          once with arguments input=\"input.txt\", output=\"out.txt\", mode=\"upper\". Do not read, \
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
            endpoint.detach().await;
            let out = std::fs::read_to_string(workdir.join("out.txt")).expect("the tool wrote the output through the holder");
            assert_eq!(out, "AGENT PAYLOAD");
            let wire = walk(&fx.root.join("seller-diagnostics"))
                .into_iter()
                .filter(|f| f.file_name().is_some_and(|n| n == "logs.txt"))
                .map(|f| std::fs::read_to_string(&f).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(wire.contains("mcp__seller-tool__transform-file"), "the agent called the seller's tool through the bridge");
            fx.assert_secret_absent("the ACP wire", &wire);
            fx.assert_secret_absent("the agent's message", report.last_agent_message.as_deref().unwrap_or(""));
            let stats = fx.stats().await;
            assert_eq!(stats["login_count"], json!(1));
            tool.shutdown().await;
            fx.write_evidence(
                "holder-agent-summary.json",
                &serde_json::to_string_pretty(&json!({
                    "acceptance": "a real claude-agent-acp turn used the held tool through tool-mcp-bridge over the job's socket",
                    "output": out,
                    "last_agent_message": report.last_agent_message,
                    "usage": report.usage,
                    "tool_call_on_the_acp_wire": true,
                    "vendor_stats": stats,
                }))
                .unwrap(),
            );
            fx.write_evidence("holder-agent-acp-wire.txt", &wire);
        });
    }
}
