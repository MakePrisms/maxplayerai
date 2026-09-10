# 09 — Production integration plan

This document is a detailed, code-grounded plan for wiring the seller-tool holder into the product.
It follows section 7 of `../../handoff/CONTINUATION-2026-09-10.md`. Every site named here was read
in the source on 2026-09-10.

Land all five steps together. Step 5 alone points every job at a socket that does not exist, so job
start fails. Gate the whole feature on a per-seat config: a seat with no held tool behaves exactly
as today.

## What exists today

- The kit is a standalone prototype. `seller_exec.rs:2411` sets `mcp_servers: Vec::new()`.
- A job runs in a Docker container. `DockerPolicy::run_argv` (`seller_exec.rs:794`) builds
  `docker run -i … -v {workdir}:/work --user uid:gid -e … <image> <agent_command>`. The container
  mounts only the per-job workdir at `CONTAINER_WORKDIR` (`/work`).
- `LaunchPolicy::launch_with_mounts(agent_command, job, extra_mounts)` (`seller_exec.rs:711`) adds
  read-write bind mounts as `(host_dir, container_path)` pairs, after the workdir mount. The
  container-delivery path already uses this. This is the hook for the per-job socket.
- The job agent runs `run_agent_job` / `run_agent_job_with_env` (`seller_exec.rs:2353`), dispatched
  from `execute_job` in `seller_node/run.rs`.
- The seller daemon boots at `SellerNode::boot_with_lock` (`run.rs:3991`) and stops through the
  signal seam in `seller_node/shutdown.rs`.

## The shape of the integrated system

- One `tool-holderd` runs per seller daemon. It enrols one time at boot and holds the session for
  the daemon's life.
- Each job container gets its own Unix socket at `/run/holder/job.sock`, and a `tool-mcp-bridge`
  that forwards to it.
- The credential never enters a job container. The holder reaches the vendor; the job reaches the
  holder by the socket. So a job needs no network for the tool, and its egress posture does not
  change.

## Step 1 — supervise the holder with the seller daemon

Site: `SellerNode::boot_with_lock` (`run.rs:3991`); the shutdown seam in `shutdown.rs`.

Do these steps.

1. Read the held-tool config (step 2). If a seat declares no held tool, skip the rest.
2. Start `tool-holderd` as a child of the daemon, after the config load and before the dispatch
   loop. Point it at the holder state directory, the runtime directory, the seller-tool config, and
   the credential file.
3. Wait for the control socket, then probe health. Enrolment happens one time here.
4. Hold the child and the control-socket path on the node struct, so the daemon owns the lifetime.
5. On shutdown, send `holder/shutdown` on the control socket, then reap the child.

Sketch:

```rust
// On the node struct:
struct SellerNode {
    // ...
    held_tool: Option<HeldTool>,   // None when the seat declares no held tool
}

struct HeldTool {
    child: std::process::Child,     // the tool-holderd process
    control_socket: PathBuf,        // <runtime>/holder.sock
    runtime: PathBuf,               // <runtime>, parent of jobs/<id>/job.sock
}
```

Decide the fail posture. Recommendation: if the holder cannot enrol at boot, log the seat as
unhealthy for the tool, but still boot. A job that calls the tool then sees an unhealthy endpoint.
Do not refuse the whole seat for one tool, unless a seat marks the tool as required.

## Step 2 — a config surface for the held tool

Site: `SellerConfig` (`home.rs:193`), which is `[seller]` in `config.toml`. The key never lives in
config; keep that rule.

Add an optional struct. Names are proposed.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldToolConfig {
    /// Path to the seller-tool config JSON (the offering; see crates/maxplayer-tool-kit).
    pub config: PathBuf,
    /// Path to the synthetic or real credential file. The holder reads it; a job never sees it.
    pub credential_file: PathBuf,
    /// The vendor base URL, when it is not already in the config JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_base_url: Option<String>,
    /// Refuse to run a job for this seat if the tool is unhealthy. Default false.
    #[serde(default)]
    pub required: bool,
}

// On SellerConfig:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub held_tool: Option<HeldToolConfig>,
```

## Step 3 — mount the per-job socket into the container

Site: `DockerPolicy::run_argv` (already renders `extra_mounts`); the call in `run_agent_job` that
uses `policy.launch`.

`run_agent_job` calls `policy.launch(&prepared.effective_command, &job)`, which passes no extra
mounts. Change it to call `launch_with_mounts` with one mount: the job's own socket directory.

```rust
let extra_mounts: Vec<(PathBuf, String)> = match &held {
    Some(endpoint) => vec![(endpoint.socket_dir.clone(), "/run/holder".to_string())],
    None => vec![],
};
let launch = policy.launch_with_mounts(&prepared.effective_command, &job, &extra_mounts)?;
```

`endpoint.socket_dir` is `<runtime>/jobs/<job_id>` on the host. It holds exactly `job.sock`. The
bridge in the container then finds `/run/holder/job.sock`. Mount nothing else from the holder. The
holder state directory and the vendor home stay unmounted, so the credential is absent by
construction, the same property the demo and the F2 tests prove.

## Step 4 — attach and detach around the job

Mirror `NetnsHolder` and `JobContainer` (`seller_exec.rs`): a guard that creates the endpoint when
it is constructed and removes it on `Drop`.

```rust
struct JobToolEndpoint {
    control_socket: PathBuf,
    job_id: String,
    socket_dir: PathBuf,     // <runtime>/jobs/<job_id>
}

impl JobToolEndpoint {
    fn attach(control: &Path, runtime: &Path, job_id: &str, job_root: &Path) -> Result<Self, ExecError> {
        // client::call(control, "holder/attach_job", { job_id, job_root })
        // returns the socket path; socket_dir is its parent.
    }
}

impl Drop for JobToolEndpoint {
    fn drop(&mut self) {
        // client::call(control, "holder/detach_job", { job_id })  — best effort, logged not propagated
    }
}
```

Construct it in `run_agent_job`, before `prepare_launch`, only when the seat has a held tool. Hold
it for the job's life. `job_root` is the host per-job workdir (`job_workdir`, already imported in
`run.rs`). Detach removes only the endpoint; the tool stays enrolled, which is the whole point of
the corrected model.

## Step 5 — wire the bridge into the agent session

Site: `seller_exec.rs:2411`.

```rust
let mcp_servers = match &held {
    Some(_) => vec![crate::driver::McpServer {
        name: "seller-tool".into(),
        command: vec!["tool-mcp-bridge".into()],
    }],
    None => Vec::new(),
};
// SessionConfig { cwd: launch.cwd, mcp_servers, env: identity.git_env() }
```

Two facts make this work.

1. The bridge binary must be inside the job container. Bake `tool-mcp-bridge` into the sandbox
   image (`SandboxConfig::image`, default `DEFAULT_SANDBOX_IMAGE`), at `/usr/local/bin`. Build it
   for that image's platform. Add it in `docker/maxplayer-sandbox/Dockerfile`.
2. `McpServer` carries no environment. The bridge reads `HOLDER_JOB_SOCKET`, which defaults to
   `/run/holder/job.sock`. Step 3 mounts the socket at exactly that path, so no environment is
   needed. If a different path is ever needed, extend `McpServer` with an `env` field, or carry it
   in `SandboxConfig::forward_env`.

## Ordering and the feature gate

- Land steps 1 to 5 together. Step 5 without steps 3 and 4 gives every job an MCP server that
  points at a missing socket, so the agent's MCP init fails and the job fails.
- Gate the whole feature on `held_tool`. A seat with `held_tool = None` mounts no socket, attaches
  no endpoint, and sets `mcp_servers` empty, exactly as today. This keeps every current seat
  unchanged.

## Egress and the credential — do not change these

- A job needs no network for the tool. It talks to the holder by a Unix socket. Keep the job's
  existing network namespace and credential proxy posture (`SandboxConfig::network`, `#797`).
- The holder reaches the vendor. Its credential and its vendor home stay on the holder's private
  state, never mounted into a job. This is the same boundary the F2 fix enforces inside the kit.

## Testing

Do these tests.

1. A core-side unit test: `run_agent_job` with a held tool mounts the socket directory at
   `/run/holder` and sets one `McpServer`; with no held tool it mounts nothing and sets none.
2. A boot test: the daemon starts the holder, enrols one time, and stops it on shutdown.
3. An end-to-end test that mirrors the kit demo: two real sequential jobs share one enrolment and
   one operation list; the credential is absent from the job container; a cross-job file is
   refused; a daemon stop and start restores the tool with no new login.
4. Keep the independent vendor counters as the oracle, not the holder's self-report.

Independent real-tool acceptance stays a separate stage. A fake CLI and a fake vendor prove
mechanism only.

## Open decisions

- **Bridge binary delivery.** Bake it into the maxplayer-sandbox image, or ship a static build and
  mount it. Recommendation: bake it in, because maxplayer owns that image.
- **Fail posture.** Boot without the tool when it cannot enrol, unless the seat marks it required.
- **One tool or several.** This plan assumes one held tool per seat. A list needs a socket and an
  `McpServer` per tool, and a config list in place of one struct.
