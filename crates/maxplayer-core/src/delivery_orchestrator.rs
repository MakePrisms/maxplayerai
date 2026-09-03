//! Container-side delivery orchestration (Track B): the git steps that must NOT run against a
//! repository the job agent could write.
//!
//! ## The exploit this closes
//! The job agent has full write access to its workdir, INCLUDING `workdir/.git/config`. libgit2
//! applies `url.<other>.insteadOf = <relay>` rewrite rules from the config of the repository that
//! RUNS an operation — at connect time, for `remote_anonymous` too. [`crate::git_transport`] empties
//! the global/XDG/system config search paths so no ambient config is read, but a repo-LOCAL
//! `.git/config` in the agent's own workdir is still consulted. So if the seller host opens the
//! agent's workdir and pushes, the agent's planted `insteadOf` silently redirects that push — with
//! the seller's signed NIP-98 token — to a host the agent chose. Test:
//! `tests/hostile_local_git_config.rs`.
//!
//! ## The fix: neutralise the config, then push
//! Before the push, [`crate::seller_git::neutralize_push_config`] REPLACES the workdir's `.git/config`
//! with a fixed, minimal, redirect-free file (shipped as the interim host fix; the container push
//! reuses it — one definition). That is a whole-file replacement, not a targeted edit, so it removes
//! every redirect knob at once — `insteadOf`, `pushInsteadOf`, `remote.*.pushurl`, and any
//! `[include]`/`[includeIf]` or worktree-config file the agent could have pointed at — rather than a
//! fragile blocklist. [`push_delivery`] then pushes from the workdir: the config has no rewrite rule,
//! so the token can only go to the URL we named. Branch-scoping (the token names one ref) is the
//! second layer if a token leaks anyway.
//!
//! The push runs in the CONTAINER (or, on the interim host path, after the container that ran the
//! agent has exited), so no live agent parses the seller key — an unknown git-parser bug at push time
//! hits a process that is already the sandbox, not the host. The caller reaps the agent's process
//! group before the push, so nothing can re-plant the config in the window.
//!
//! The completion gate and the execution sentinel are NOT re-implemented here: the delivery commit is
//! produced by [`crate::seller_git::snapshot_delivery_at`], which runs the §19 gate (refusing an empty
//! tree) and force-stages the sentinel. This module provisions, runs the agent, gates, and pushes.
//!
//! ## One container, driven from inside (Task B9 + B2)
//! Behind `[sandbox] container_delivery = true` the seller host launches ONE container whose command
//! is `maxplayer __deliver phase1 <inputs>`. [`run_phase1_entry`] then, in order: reads the inputs and
//! DELETES the file (C3, fail closed); provisions the workdir; DRIVES the ACP agent itself through
//! [`crate::seller_exec::run_agent_with_retry`] under a pass-through policy (the container is the
//! sandbox — no nested docker), with an explicit environment allowlist (C4); gates and commits; reaps
//! every other process in the container; writes the [`AGENT_DONE_MARKER`]; obtains the push token per
//! [`PushTokenSource`]; pushes with the gated oid as the expected oid (C6); and writes the oid and the
//! [`Phase1Outcome`] for the host. The host runs no git and no ACP: it mints tokens (crypto), reads
//! two small files, and publishes the kind-3403.
//!
//! ## The exchange directory
//! The host owns a per-job directory (mode `0700`, outside the agent-writable workdir) and mounts it
//! at [`CONTAINER_EXCHANGE_DIR`]. The host writes [`PHASE1_INPUTS_FILE`] (mode `0600`) before launch
//! and, in the fresh-after-agent mode, [`PUSH_TOKEN_FILE`] (mode `0600`) after the marker appears. The
//! container writes [`AGENT_DONE_MARKER`], [`DELIVERY_OID_FILE`] and [`OUTCOME_FILE`].
//!
//! ⚠ Who can read what, and when. The container runs every process as ONE uid (`--user`, no
//! capabilities), so the agent and the orchestrator share file permissions inside the container. The
//! contract therefore rests on TIME, not on modes: the inputs file (`job_hash`, and in long-lived mode
//! the token) is deleted BEFORE the agent is spawned, and the fresh token file is written only AFTER
//! the agent and every other process are dead and the marker — which carries a nonce only the
//! orchestrator learned from the deleted inputs — has been verified by the host. A job process that
//! survives cannot forge the marker without the nonce, and the token is branch-scoped in any case, so a
//! leak pushes nothing but the seller's own delivery branch.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::git_transport::{self, TransportError};
use crate::seller_git::{self, DeliveryAgentIdentity, SellerGitError};

/// A failure in the delivery (provision / gate / push) path.
#[derive(Debug)]
pub enum OrchestratorError {
    /// Filesystem / repository open/init failure.
    Io(String),
    /// Provisioning the agent workdir (clone of the base, or init) failed.
    Provision(SellerGitError),
    /// The completion gate / snapshot refused or failed. Carries the [`SellerGitError`] so the caller
    /// can still tell `NoExecutionObserved` (map to `no_sentinel`) from any other failure.
    Gate(SellerGitError),
    /// The push failed and its error is not retryable (permission, allowlist, …).
    Push(TransportError),
    /// Every retry attempt was spent without a successful push.
    PushExhausted { attempts: u32, last: String },
    /// The agent run failed (spawn, protocol, or a turn that ended non-completed). Carries the detail.
    Agent(String),
    /// The job deadline passed while the agent was still running.
    DeadlineExceeded,
    /// No agent can run here at all: the binary lacks the `acp` feature, or the agent command is
    /// misconfigured. Carries the detail. Distinct from [`Self::Agent`] so the host can attribute it
    /// to the seat, not the harness.
    AgentUnavailable(String),
    /// The push token was not obtained: the fresh token file did not arrive in time, or the host
    /// refused to mint it. NEVER carries a token.
    TokenUnavailable(String),
    /// Evidence that something other than the orchestrator touched the delivery between the gate and
    /// the push — a process that survived the agent, a moved branch, a forged marker. Fail closed.
    Tampered(String),
}

impl std::fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "delivery orchestrator io: {message}"),
            Self::Provision(error) => write!(f, "delivery workdir provision failed: {error}"),
            Self::Gate(error) => write!(f, "delivery gate refused: {error}"),
            Self::Push(error) => write!(f, "delivery push failed: {error}"),
            Self::PushExhausted { attempts, last } => write!(
                f,
                "delivery push failed after {attempts} attempts; last error: {last}"
            ),
            Self::Agent(message) => write!(f, "delivery agent run failed: {message}"),
            Self::DeadlineExceeded => write!(f, "job deadline reached while the agent was running"),
            Self::AgentUnavailable(message) => {
                write!(f, "delivery agent cannot run in this container: {message}")
            }
            Self::TokenUnavailable(message) => write!(f, "delivery push token: {message}"),
            Self::Tampered(message) => write!(f, "delivery refused (tamper evidence): {message}"),
        }
    }
}

impl std::error::Error for OrchestratorError {}

// The push-config hardening lives in [`crate::seller_git::neutralize_push_config`] (shipped as the
// interim host fix). The container push below reuses it — one definition of the config rewrite.

/// The pinned base a contribution delivery forks from. `None` at the [`run_phase1`] call site means a
/// from-scratch delivery (a root commit whose tree is the whole workdir).
#[derive(Clone, Copy, Debug)]
pub struct Phase1Base<'a> {
    /// The base repo to clone. MUST be allowlisted (https / relay-git); the clone asserts it.
    pub clone_url: &'a str,
    /// The base branch to fetch.
    pub branch: &'a str,
    /// The pinned base commit the delivery is parented on.
    pub oid: &'a str,
}

/// What phase 1 produced: the gated commit, and the repo phase 2 pushes it from.
#[derive(Clone, Debug)]
pub struct Phase1Output {
    /// The gated delivery commit oid (full hex). Deterministic; the host names it in the kind-3403.
    pub delivery_oid: String,
    /// The committed repo phase 2 pushes from. This is the agent's workdir: the delivery branch
    /// points at the gated commit here. Phase 2 pushes it with a branch-scoped token, so even if the
    /// agent planted an `insteadOf` the leaked token is worthless (Track A).
    pub delivery_repo_dir: PathBuf,
}

/// Phase 1 of container-side delivery: provision the workdir, run the agent, gate + sentinel +
/// commit — all with NO push credential present. The host mints the scoped token only after this
/// returns; phase 2 ([`push_delivery`]) then pushes the committed repo. No seller key and no push
/// token exists in this process.
///
/// This does not push. The push ([`push_delivery`]) hardens the workdir config (via
/// [`crate::seller_git::neutralize_push_config`]) and pushes from the same workdir, so a planted
/// `insteadOf` cannot redirect the token — no separate clean repo needed.
///
/// `spawn_agent(workdir)` runs the agent against the freshly-provisioned workdir and returns when it
/// exits. It is a seam: production inherits the container's stdin/stdout to the agent child (so the
/// host drives ACP unchanged) and waits; tests write a deliverable directly. The completion gate
/// inside [`seller_git::snapshot_delivery_at`] refuses an empty tree, so a quota-dead agent that
/// wrote nothing yields [`OrchestratorError::Gate`] wrapping `NoExecutionObserved` and mints no
/// sentinel — exactly as on the host today.
pub fn run_phase1(
    agent_workdir: &Path,
    identity: &DeliveryAgentIdentity,
    base: Option<Phase1Base<'_>>,
    delivery_branch: &str,
    message: &str,
    author_date_unix: i64,
    job_hash: &str,
    spawn_agent: impl FnOnce(&Path) -> Result<(), OrchestratorError>,
) -> Result<Phase1Output, OrchestratorError> {
    let base_oid = base.map(|b| b.oid);

    // 1. Provision: clone the pinned base (contribution) or init an empty repo (from-scratch).
    match base {
        Some(b) => seller_git::init_contribution_workdir(
            agent_workdir,
            identity,
            b.clone_url,
            b.branch,
            b.oid,
            delivery_branch,
            None, // public buyer repo: no auth. A relay-git base would pass a PushAuth here.
        )
        .map_err(OrchestratorError::Provision)?,
        None => seller_git::init_empty_delivery_workdir(agent_workdir, identity)
            .map_err(OrchestratorError::Provision)?,
    }

    // 2. Run the agent against the provisioned workdir.
    spawn_agent(agent_workdir)?;

    // 3. Gate + sentinel + commit. The gate refuses an empty tree; only a real tree gets a sentinel.
    //    The delivery branch now points at the gated commit in this workdir; phase 2 pushes it.
    let delivery_oid = seller_git::snapshot_delivery_at(
        agent_workdir,
        identity,
        base_oid,
        delivery_branch,
        message,
        author_date_unix,
        job_hash,
    )
    .map_err(OrchestratorError::Gate)?;

    Ok(Phase1Output {
        delivery_oid,
        delivery_repo_dir: agent_workdir.to_path_buf(),
    })
}

/// The file phase 1 writes the delivery oid to, in the host-mounted output directory, so the host
/// reads the delivery result WITHOUT running a git command against the agent's repo.
pub const DELIVERY_OID_FILE: &str = "delivery-oid";

/// Write the delivery oid to `DELIVERY_OID_FILE` under `out_dir` (one line, trailing newline).
pub fn write_delivery_oid(out_dir: &Path, oid: &str) -> Result<(), OrchestratorError> {
    std::fs::write(out_dir.join(DELIVERY_OID_FILE), format!("{oid}\n"))
        .map_err(|error| OrchestratorError::Io(format!("write delivery oid: {error}")))
}

/// Read the delivery oid the host will name in the kind-3403, validating it is a plausible git oid
/// (40 hex chars). Fails closed on anything else, so a truncated or garbage file never becomes a
/// published commit reference.
pub fn read_delivery_oid(out_dir: &Path) -> Result<String, OrchestratorError> {
    let raw = std::fs::read_to_string(out_dir.join(DELIVERY_OID_FILE))
        .map_err(|error| OrchestratorError::Io(format!("read delivery oid: {error}")))?;
    let oid = raw.trim().to_owned();
    if oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(oid)
    } else {
        Err(OrchestratorError::Io(format!(
            "delivery oid file is not a 40-char hex oid: {oid:?}"
        )))
    }
}

/// The pinned base, owned, for [`Phase1Inputs`] (serde cannot borrow across the input file).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Phase1BaseOwned {
    pub clone_url: String,
    pub branch: String,
    pub oid: String,
}

/// Where the container-side orchestrator obtains the branch-scoped push token.
///
/// The token is a NIP-98 `Authorization` header the host signs for ONE ref
/// ([`git_transport::delivery_ref`] of the delivery branch). Who can read it, and when:
/// - `LongLived`: it rides in the inputs file, which the orchestrator deletes before the agent
///   exists; from then on it is in the orchestrator's memory only.
/// - `FreshAfterAgent`: it never exists while the agent does. The host writes it (mode `0600`) into
///   the exchange directory after it has verified the marker, and the orchestrator reads and deletes
///   it. Every other process in the container has been reaped by then.
/// - `None`: a public/anonymous https remote takes no header.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum PushTokenSource {
    /// No header: a public/anonymous https remote.
    None,
    /// The host minted the header before launch with a NIP-40 `expiration` of
    /// `deadline + PUSH_MARGIN_SECS`. Valid only against a relay that honours `expiration` for scoped
    /// tokens (relay Requirement B).
    LongLived { header: String },
    /// The host mints a fresh 60 s header when it sees the marker, and writes it to
    /// [`PUSH_TOKEN_FILE`]. The orchestrator waits up to `wait_secs` for it.
    FreshAfterAgent { wait_secs: u64 },
}

/// Everything the container orchestrator needs, handed to it through a file the host writes (mode
/// `0600`, in the host-owned exchange directory) and the orchestrator reads. The orchestrator DELETES
/// this file before it starts the agent — and refuses to start it if the delete fails (C3) — so
/// `job_hash`, the nonce and (in long-lived mode) the token then live only in the orchestrator's
/// memory. None of them is ever placed in the agent's env or argv (C4).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Phase1Inputs {
    pub job_hash: String,
    pub seller_pubkey_hex: String,
    pub base: Option<Phase1BaseOwned>,
    pub delivery_branch: String,
    pub message: String,
    pub author_date_unix: i64,
    /// The ACP harness argv the orchestrator drives (Task B9), as the host would have spawned it:
    /// the preset command plus any containment redirect flags. Carries NO secret.
    pub agent_argv: Vec<String>,
    /// The workdir INSIDE the container ([`crate::seller_exec::CONTAINER_WORKDIR`]).
    pub workdir: PathBuf,
    /// The exchange directory INSIDE the container ([`CONTAINER_EXCHANGE_DIR`]).
    pub out_dir: PathBuf,
    /// The composed agent prompt (`seller_exec::compose_agent_prompt` output). Public text.
    pub prompt: String,
    /// The job's absolute deadline; the ACP idle timeout and the retry bound derive from it.
    pub deadline_unix: u64,
    /// Bounded agent-run attempts within the deadline (`run_agent_with_retry`).
    pub max_agent_attempts: u32,
    /// C4: the NAMES of the variables the host placed in the container environment for the agent —
    /// credential placeholders, proxy base URLs, forwarded names, the git identity. The orchestrator
    /// gives the agent exactly these (with their values read from the container environment) plus the
    /// runtime baseline ([`AGENT_ENV_BASELINE`]) and the delivery identity, and nothing else.
    pub agent_env_names: Vec<String>,
    /// The delivery remote the orchestrator pushes to (the seller's `git_remote`).
    pub relay_url: String,
    /// How the push token is obtained. See [`PushTokenSource`] for who can read it and when.
    pub push_token: PushTokenSource,
    /// Random, host-generated, one per launch. The orchestrator echoes it in [`AGENT_DONE_MARKER`] so
    /// the host can tell the orchestrator's marker from one a job process forged: the agent never
    /// sees this file, so it never learns the nonce.
    pub handoff_nonce: String,
}

/// Everything the phase-2 (push-only) container needs. No agent runs in phase 2, so the scoped token
/// never coexists with a job agent. The token is a full NIP-98 `Authorization` header, or `None` for
/// a public/anonymous https remote.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Phase2Inputs {
    /// The committed repo to push from — phase 1's workdir, on the shared volume. Its delivery branch
    /// points at the gated commit.
    pub repo_dir: PathBuf,
    pub relay_url: String,
    pub delivery_branch: String,
    pub header: Option<String>,
    pub out_dir: PathBuf,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, OrchestratorError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| OrchestratorError::Io(format!("read inputs {}: {error}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|error| OrchestratorError::Io(format!("parse inputs {}: {error}", path.display())))
}

/// Serialise `inputs` to `path` as JSON (the host side of the input channel), mode `0600`, and never
/// over an existing file. The file may carry the long-lived push token, so it is created owner-only
/// from the first byte ([`write_secret_file`]) — there is no window in which it is world-readable.
/// Fails closed when the mode cannot be set.
pub fn write_phase1_inputs(path: &Path, inputs: &Phase1Inputs) -> Result<(), OrchestratorError> {
    let json = serde_json::to_string(inputs)
        .map_err(|error| OrchestratorError::Io(format!("encode phase1 inputs: {error}")))?;
    write_secret_file(path, &json)
}

/// Serialise `inputs` to `path` as JSON (phase 2).
pub fn write_phase2_inputs(path: &Path, inputs: &Phase2Inputs) -> Result<(), OrchestratorError> {
    let json = serde_json::to_string(inputs)
        .map_err(|error| OrchestratorError::Io(format!("encode phase2 inputs: {error}")))?;
    std::fs::write(path, json)
        .map_err(|error| OrchestratorError::Io(format!("write phase2 inputs: {error}")))
}

/// The container orchestrator entrypoint (`maxplayer __deliver phase1 <inputs>`), driving the real
/// ACP agent. Reads `inputs_path`, DELETES it (C3, fail closed), provisions the workdir, runs the agent
/// through [`crate::seller_exec::run_agent_with_retry`] under a pass-through policy with an explicit
/// environment allowlist (C4), gates and commits, reaps every other process, hands off the token per
/// [`PushTokenSource`], pushes with the gated oid as the expected oid (C6), and writes the oid and the
/// [`Phase1Outcome`] for the host. The seller key never enters this process; in long-lived mode the
/// scoped token does, and it is in memory only by the time the agent exists.
#[cfg(feature = "wallet")]
pub fn run_phase1_entry(inputs_path: &Path) -> Result<Phase1Output, OrchestratorError> {
    run_phase1_entry_with(inputs_path, drive_acp_agent)
}

/// [`run_phase1_entry`] over an injected agent runner, so the whole contract — the fail-closed inputs
/// delete, the marker, the token hand-off, the expected-oid push, the outcome file — is exercised
/// without a real ACP harness. `run_agent(inputs, workdir)` runs the agent against the provisioned
/// workdir and returns what it reported.
///
/// The outcome file is written on EVERY exit, success or failure, so the host learns why a container
/// exited non-zero without parsing its logs.
pub fn run_phase1_entry_with(
    inputs_path: &Path,
    run_agent: impl FnOnce(&Phase1Inputs, &Path) -> Result<AgentOutcome, OrchestratorError>,
) -> Result<Phase1Output, OrchestratorError> {
    let inputs = read_json::<Phase1Inputs>(inputs_path)?;
    // C3: the inputs file holds job_hash, the nonce and possibly the token. It must be gone before
    // any job code exists. A delete that fails is a refusal to run the agent, not a warning.
    delete_inputs_fail_closed(inputs_path)?;

    let started = std::time::Instant::now();
    let mut agent: Option<AgentOutcome> = None;
    let result = deliver_in_container(&inputs, &mut agent, run_agent);
    let outcome = Phase1Outcome::from_result(&result, agent, started.elapsed());
    if let Err(error) = write_outcome(&inputs.out_dir, &outcome) {
        eprintln!("sandbox orchestrator: could not write the outcome file: {error}");
    }
    result
}

/// The delivery proper, after the inputs are in memory and the file is gone: provision → agent →
/// gate + commit → reap → marker → token → push → oid file. Split from [`run_phase1_entry_with`] so
/// the outcome file can be written from the result of the whole sequence.
fn deliver_in_container(
    inputs: &Phase1Inputs,
    agent: &mut Option<AgentOutcome>,
    run_agent: impl FnOnce(&Phase1Inputs, &Path) -> Result<AgentOutcome, OrchestratorError>,
) -> Result<Phase1Output, OrchestratorError> {
    let identity = DeliveryAgentIdentity::for_seller(&inputs.seller_pubkey_hex);
    let base = inputs.base.as_ref().map(|b| Phase1Base {
        clone_url: &b.clone_url,
        branch: &b.branch,
        oid: &b.oid,
    });
    let output = run_phase1(
        &inputs.workdir,
        &identity,
        base,
        &inputs.delivery_branch,
        &inputs.message,
        inputs.author_date_unix,
        &inputs.job_hash,
        |workdir| {
            *agent = Some(run_agent(inputs, workdir)?);
            Ok(())
        },
    )?;

    // The gate has run and the delivery commit exists. From here on nothing but this process may
    // touch the workdir: reap whatever the agent left behind BEFORE the marker invites a token in
    // and BEFORE the push reads the branch (C6's threat is exactly a survivor re-pointing it).
    reap_other_processes()?;
    write_agent_done_marker(&inputs.out_dir, &inputs.handoff_nonce, &output.delivery_oid)?;

    let header = match &inputs.push_token {
        PushTokenSource::None => None,
        PushTokenSource::LongLived { header } => Some(header.clone()),
        PushTokenSource::FreshAfterAgent { wait_secs } => {
            Some(await_push_token(&inputs.out_dir, Duration::from_secs(*wait_secs))?)
        }
    };
    let pushed = push_delivery(
        &output.delivery_repo_dir,
        &inputs.relay_url,
        &inputs.delivery_branch,
        header,
        &PushRetryPolicy::default(),
        Some(&output.delivery_oid),
    )?;
    write_delivery_oid(&inputs.out_dir, &pushed)?;
    Ok(output)
}

/// C3: delete the inputs file, and refuse to go on if it is still there. The file holds `job_hash`,
/// the hand-off nonce and, in long-lived mode, the push token; the agent must never find it.
fn delete_inputs_fail_closed(path: &Path) -> Result<(), OrchestratorError> {
    std::fs::remove_file(path).map_err(|error| {
        OrchestratorError::Io(format!(
            "delete inputs {}: {error}; refusing to start the agent",
            path.display()
        ))
    })?;
    if path.symlink_metadata().is_ok() {
        return Err(OrchestratorError::Io(format!(
            "inputs {} still present after delete; refusing to start the agent",
            path.display()
        )));
    }
    Ok(())
}

/// Drive the real ACP agent inside the container (Task B9): [`crate::seller_exec::run_agent_with_retry`]
/// around [`crate::seller_exec::run_agent_job_with_env`] under a PASS-THROUGH policy — the container is
/// already the sandbox, so there is no nested docker — on a current-thread Tokio runtime built here,
/// because `maxplayer __deliver` is a synchronous CLI. The agent's environment is the allowlist from
/// [`agent_env_allowlist`], never this process's whole environment (C4).
#[cfg(feature = "wallet")]
fn drive_acp_agent(inputs: &Phase1Inputs, workdir: &Path) -> Result<AgentOutcome, OrchestratorError> {
    use crate::seller_exec::{
        run_agent_job_with_env, run_agent_with_retry, unified_job_timeout, AgentRunTimeout,
        ExecError, SandboxPolicy,
    };
    let identity = DeliveryAgentIdentity::for_seller(&inputs.seller_pubkey_hex);
    let env = agent_env_allowlist(&inputs.agent_env_names, &identity, |key| std::env::var(key).ok());
    let policy = SandboxPolicy::passthrough();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| OrchestratorError::Io(format!("tokio runtime: {error}")))?;
    let result = runtime.block_on(run_agent_with_retry(
        inputs.deadline_unix,
        inputs.max_agent_attempts.max(1),
        unix_now,
        |_attempt| {
            let timeout = unified_job_timeout(inputs.deadline_unix, unix_now());
            run_agent_job_with_env(
                &inputs.agent_argv,
                &policy,
                &inputs.prompt,
                workdir,
                &identity,
                AgentRunTimeout::JobDeadline(timeout),
                Some(env.clone()),
            )
        },
    ));
    match result {
        Ok(report) => Ok(AgentOutcome {
            usage: report.usage,
            last_agent_message: report.last_agent_message,
        }),
        Err(ExecError::DeadlineExceeded) => Err(OrchestratorError::DeadlineExceeded),
        Err(error @ (ExecError::AcpRequired | ExecError::Config(_))) => {
            Err(OrchestratorError::AgentUnavailable(error.to_string()))
        }
        Err(error) => Err(OrchestratorError::Agent(error.to_string())),
    }
}

/// Seconds since the Unix epoch, saturating at zero on a clock before 1970.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Phase-2 container entrypoint. Reads `inputs_path` and pushes the committed repo (phase 1's
/// workdir) with the scoped token, retrying transient/conflict failures. No agent runs here, so the
/// token never coexists with job code. Writes the pushed oid to the OID file as confirmation.
pub fn run_phase2_entry(inputs_path: &Path) -> Result<String, OrchestratorError> {
    let inputs = read_json::<Phase2Inputs>(inputs_path)?;
    let oid = push_delivery(
        &inputs.repo_dir,
        &inputs.relay_url,
        &inputs.delivery_branch,
        inputs.header,
        &PushRetryPolicy::default(),
        None,
    )?;
    write_delivery_oid(&inputs.out_dir, &oid)?;
    Ok(oid)
}

/// Retry policy for the delivery push. Exponential backoff with full jitter, bounded by an attempt
/// count. The caller must pick values whose worst-case total sleep stays inside the relay's NIP-98
/// token age window (±60 s), because the push token is minted just before the first attempt.
#[derive(Clone, Copy, Debug)]
pub struct PushRetryPolicy {
    /// Total attempts, including the first. `1` disables retrying.
    pub max_attempts: u32,
    /// Backoff before the second attempt; doubles each further attempt, capped at `max_delay`.
    pub base_delay: Duration,
    /// Upper bound on any single backoff.
    pub max_delay: Duration,
}

impl Default for PushRetryPolicy {
    /// 5 attempts, 0.5 s base, 8 s cap. Worst-case total backoff (full jitter) ≤ 15.5 s, well inside
    /// the 60 s token window even with per-attempt push time.
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

/// The un-jittered backoff before attempt `next_attempt` (2-based: the wait before attempt 2 is
/// `base_delay`). `base_delay * 2^(next_attempt-2)`, capped at `max_delay`. Saturating, so a large
/// attempt count never overflows.
fn backoff_delay(policy: &PushRetryPolicy, next_attempt: u32) -> Duration {
    let shift = next_attempt.saturating_sub(2);
    let factor = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
    policy
        .base_delay
        .saturating_mul(factor)
        .min(policy.max_delay)
}

/// Whether a push error is worth retrying. A concurrent receive-pack to one repo (the relay
/// serialises pack ingestion) and a transient network/status failure clear on a retry; a permission
/// (403, including a ref-scope refusal) or an allowlist refusal never will.
pub fn default_is_retryable(error: &TransportError) -> bool {
    match error {
        // Transient transport/status (connect, TLS, unexpected status incl. a 409 ingestion
        // conflict) and a receive-pack rejection (the per-repo push lock) can clear on a retry.
        TransportError::Io(_) | TransportError::Rejected(_) => true,
        // A permission signal (a bad/expired token, or the relay refusing the ref scope) and an
        // allowlist refusal are permanent — retrying only wastes the token window.
        TransportError::Auth(_) | TransportError::Transport(_) => false,
    }
}

/// Drive a push with bounded, jittered-exponential-backoff retries. Pure and deterministic in its
/// injected dependencies, so the retry logic is unit-testable without real time or entropy:
/// - `push(attempt)` performs one push attempt (1-based) and returns the pushed oid.
/// - `is_retryable(&err)` decides whether a failure is worth another attempt.
/// - `sleep(d)` waits (production: thread sleep; tests: record the durations).
/// - `jitter(d)` maps a backoff to an actual wait (production: full jitter; tests: identity).
///
/// Returns the pushed oid, or [`OrchestratorError::PushExhausted`] once attempts run out or a
/// non-retryable error is seen. Attempts never overlap — each `push` returns before the next.
pub fn push_with_retry(
    policy: &PushRetryPolicy,
    mut push: impl FnMut(u32) -> Result<String, TransportError>,
    is_retryable: impl Fn(&TransportError) -> bool,
    mut sleep: impl FnMut(Duration),
    mut jitter: impl FnMut(Duration) -> Duration,
) -> Result<String, OrchestratorError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match push(attempt) {
            Ok(oid) => return Ok(oid),
            Err(error) if attempt < policy.max_attempts && is_retryable(&error) => {
                let wait = jitter(backoff_delay(policy, attempt + 1));
                sleep(wait);
                continue;
            }
            Err(error) => {
                return Err(OrchestratorError::PushExhausted {
                    attempts: attempt,
                    last: error.to_string(),
                })
            }
        }
    }
}

/// Full jitter: an actual wait uniformly in `[0, backoff]`. Decorrelates several jobs that push to
/// one repo at once, so they do not retry in lockstep. Falls back to the full backoff if the OS RNG
/// is briefly unavailable (a longer wait is safe; a shorter correlated one is what we avoid).
fn full_jitter(backoff: Duration) -> Duration {
    let max_ms = backoff.as_millis();
    if max_ms == 0 {
        return Duration::ZERO;
    }
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return backoff;
    }
    let span = (max_ms as u64).saturating_add(1);
    Duration::from_millis(u64::from_le_bytes(bytes) % span)
}

/// Push the delivery `branch` from `repo_dir` to `relay_url` with the caller-minted NIP-98 `header`,
/// retrying transient/conflict failures per `policy`. This is the production phase-2 push: it wires
/// [`push_with_retry`] to a real thread sleep and full jitter, and pushes through
/// [`git_transport::push_branch_with_header`] (which re-asserts the OUTBOUND transport allowlist on
/// `relay_url`).
///
/// `repo_dir` is the committed workdir the delivery branch points into. `header` is `Some` for a
/// relay-git remote and `None` for a public/anonymous https remote.
///
/// The repo-local config is NEUTRALISED first ([`crate::seller_git::neutralize_push_config`]), so an
/// `insteadOf` / `pushInsteadOf` the agent may have planted cannot redirect this push. The caller MUST
/// have reaped the agent's process group already (see that function's docs).
///
/// `expected_oid` (C6): when `Some`, the delivery branch must point at exactly this commit — the one
/// the gate produced — both BEFORE the push and as the oid the push reports. A process that survived
/// the agent could otherwise re-point the branch between the gate and the push and have the seller
/// sign a kind-3403 for a commit the gate never saw. Any mismatch is [`OrchestratorError::Tampered`],
/// and no push happens on a pre-push mismatch.
pub fn push_delivery(
    repo_dir: &Path,
    relay_url: &str,
    branch: &str,
    header: Option<String>,
    policy: &PushRetryPolicy,
    expected_oid: Option<&str>,
) -> Result<String, OrchestratorError> {
    if let Some(expected) = expected_oid {
        let tip = local_branch_tip(repo_dir, branch)?;
        if tip != expected {
            return Err(OrchestratorError::Tampered(format!(
                "delivery branch {branch} points at {tip}, not at the gated commit {expected}; \
                 refusing to push"
            )));
        }
    }
    seller_git::neutralize_push_config(repo_dir)
        .map_err(|error| OrchestratorError::Io(error.to_string()))?;
    let pushed = push_with_retry(
        policy,
        |_attempt| {
            git_transport::push_branch_with_header(
                repo_dir,
                relay_url,
                branch,
                header.clone(),
            )
        },
        default_is_retryable,
        |wait| std::thread::sleep(wait),
        full_jitter,
    )?;
    if let Some(expected) = expected_oid
        && pushed != expected
    {
        return Err(OrchestratorError::Tampered(format!(
            "pushed {pushed} for {branch}, not the gated commit {expected}"
        )));
    }
    Ok(pushed)
}

/// The commit the local delivery branch points at in `repo_dir` (full hex). Reads the ref only —
/// no config, no network.
fn local_branch_tip(repo_dir: &Path, branch: &str) -> Result<String, OrchestratorError> {
    let repo = git2::Repository::open(repo_dir)
        .map_err(|error| OrchestratorError::Io(format!("open delivery repo: {error}")))?;
    repo.refname_to_id(&git_transport::delivery_ref(branch))
        .map(|oid| oid.to_string())
        .map_err(|error| OrchestratorError::Io(format!("resolve delivery branch {branch}: {error}")))
}

/// Off-runtime [`push_delivery`]: neutralise `workdir`'s config and push from it, on a blocking
/// thread. This is the delivery push the seller daemon runs — it hardens the workdir's git config so
/// an `insteadOf` the agent planted cannot redirect the token, then pushes. Returns the pushed oid.
///
/// Safe on the host interim path because the container that ran the agent has already exited: no agent
/// process is alive to re-plant the config between the neutralise and the push.
pub async fn push_delivery_off_runtime(
    workdir: PathBuf,
    remote_url: String,
    branch: String,
    header: Option<String>,
    policy: PushRetryPolicy,
    expected_oid: Option<String>,
) -> Result<String, OrchestratorError> {
    tokio::task::spawn_blocking(move || {
        push_delivery(&workdir, &remote_url, &branch, header, &policy, expected_oid.as_deref())
    })
    .await
    .map_err(|error| {
        OrchestratorError::Io(format!("blocking delivery task did not complete: {error}"))
    })?
}


// ─── The host↔container exchange contract ─────────────────────────────────────────────────────────

/// Where the host mounts the per-job exchange directory inside the container. Outside the workdir
/// mount on purpose: the workdir is the deliverable and is agent-writable.
pub const CONTAINER_EXCHANGE_DIR: &str = "/maxplayer-io";
/// The orchestrator binary inside the sandbox image (Task B7 puts it there). Absolute, so the launch
/// does not depend on the image's `PATH`.
pub const CONTAINER_ORCHESTRATOR_BIN: &str = "/usr/local/bin/maxplayer";
/// The inputs file the host writes into the exchange directory before launch (mode `0600`). Read
/// and DELETED by the orchestrator before the agent starts.
pub const PHASE1_INPUTS_FILE: &str = "inputs.json";
/// The marker the orchestrator writes after the agent is dead and the delivery commit is gated. Its
/// nonce proves the orchestrator, not a job process, wrote it; its `expected_oid` is the gated commit.
pub const AGENT_DONE_MARKER: &str = "agent-done";
/// The fresh push token the host writes (mode `0600`) after it verified the marker, in the
/// fresh-after-agent mode. Read and DELETED by the orchestrator. It never exists while the agent does.
pub const PUSH_TOKEN_FILE: &str = "push-token";
/// The orchestrator's account of the run, written on every exit ([`Phase1Outcome`]).
pub const OUTCOME_FILE: &str = "outcome.json";
/// Seconds past the job deadline the delivery may still take (gate + push). The long-lived token
/// expires at `deadline + PUSH_MARGIN_SECS`, and the host waits at most that long plus
/// [`CONTAINER_EXIT_GRACE_SECS`] for the container.
pub const PUSH_MARGIN_SECS: u64 = 300;
/// How long past `deadline + PUSH_MARGIN_SECS` the host waits for the container to exit before it
/// kills it.
pub const CONTAINER_EXIT_GRACE_SECS: u64 = 120;
/// How long the orchestrator waits for the fresh token file after it wrote the marker. The host
/// polls the marker every second, so the token normally arrives within a few seconds; the bound
/// covers a slow signer round-trip and a busy host.
pub const FRESH_TOKEN_WAIT_SECS: u64 = 120;
/// How often the orchestrator polls for the token file.
const TOKEN_POLL: Duration = Duration::from_millis(250);

/// What the agent run reported, in the orchestrator's own words. Mirrors
/// `seller_exec::AgentRunReport`, which is `wallet`-gated; this module is not.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentOutcome {
    /// Usage the driver surfaced. `None` when the harness exposed nothing.
    pub usage: Option<crate::driver::UsageMetadata>,
    /// The agent's last non-empty message, verbatim.
    pub last_agent_message: Option<String>,
}

/// How the run ended, for the host's feedback and harness attribution. Every arm maps onto a host
/// outcome that exists today: the host publishes the same reason codes it would for the host path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase1Status {
    /// Gated, pushed, oid written. The host reads [`DELIVERY_OID_FILE`].
    Delivered,
    /// The workdir could not be provisioned (clone/init).
    ProvisionFailed,
    /// The agent run failed. The host attributes this to the harness as unproven.
    AgentFailed,
    /// No agent can run in this container (missing `acp`, bad command). A seat problem.
    AgentUnavailable,
    /// The deadline passed while the agent was running.
    DeadlineExceeded,
    /// The gate saw no execution: an empty or base-identical tree. Maps to `no_sentinel`.
    NoSentinel,
    /// The snapshot failed for another reason.
    SnapshotFailed,
    /// The push token did not arrive, or was refused.
    TokenUnavailable,
    /// The push failed after retries, or permanently.
    PushFailed,
    /// Tamper evidence between the gate and the push. Nothing was published.
    Tampered,
    /// An I/O failure outside the arms above.
    Aborted,
}

/// The orchestrator's account of one run, written to [`OUTCOME_FILE`] on every exit. Carries the
/// agent's usage for the seller-claimed exec-metadata block and the failure class for the host's
/// feedback. NEVER carries a token, a key, or `job_hash`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Phase1Outcome {
    pub status: Phase1Status,
    /// Human-readable detail for the operator log. Never a secret.
    pub detail: String,
    /// The pushed oid on [`Phase1Status::Delivered`]; `None` otherwise.
    pub delivery_oid: Option<String>,
    /// What the agent reported, when it ran to completion.
    pub agent: Option<AgentOutcome>,
    /// Wall time of the whole container run, agent included.
    pub wall_time_ms: u64,
}

impl Phase1Outcome {
    /// Classify the delivery result. `agent` is what the agent reported, if it completed.
    pub fn from_result(
        result: &Result<Phase1Output, OrchestratorError>,
        agent: Option<AgentOutcome>,
        elapsed: Duration,
    ) -> Self {
        let wall_time_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        match result {
            Ok(output) => Self {
                status: Phase1Status::Delivered,
                detail: String::new(),
                delivery_oid: Some(output.delivery_oid.clone()),
                agent,
                wall_time_ms,
            },
            Err(error) => {
                let status = match error {
                    OrchestratorError::Provision(_) => Phase1Status::ProvisionFailed,
                    OrchestratorError::Agent(_) => Phase1Status::AgentFailed,
                    OrchestratorError::AgentUnavailable(_) => Phase1Status::AgentUnavailable,
                    OrchestratorError::DeadlineExceeded => Phase1Status::DeadlineExceeded,
                    OrchestratorError::Gate(SellerGitError::NoExecutionObserved(_)) => {
                        Phase1Status::NoSentinel
                    }
                    OrchestratorError::Gate(_) => Phase1Status::SnapshotFailed,
                    OrchestratorError::TokenUnavailable(_) => Phase1Status::TokenUnavailable,
                    OrchestratorError::Push(_) | OrchestratorError::PushExhausted { .. } => {
                        Phase1Status::PushFailed
                    }
                    OrchestratorError::Tampered(_) => Phase1Status::Tampered,
                    OrchestratorError::Io(_) => Phase1Status::Aborted,
                };
                Self {
                    status,
                    detail: error.to_string(),
                    delivery_oid: None,
                    agent,
                    wall_time_ms,
                }
            }
        }
    }
}

/// Write the outcome file (container side).
pub fn write_outcome(out_dir: &Path, outcome: &Phase1Outcome) -> Result<(), OrchestratorError> {
    let json = serde_json::to_string(outcome)
        .map_err(|error| OrchestratorError::Io(format!("encode outcome: {error}")))?;
    write_file_atomically(&out_dir.join(OUTCOME_FILE), &json, None)
}

/// Read the outcome file (host side). `Ok(None)` when the container never wrote one.
pub fn read_outcome(out_dir: &Path) -> Result<Option<Phase1Outcome>, OrchestratorError> {
    let path = out_dir.join(OUTCOME_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map(Some).map_err(|error| {
            OrchestratorError::Io(format!("parse outcome {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(OrchestratorError::Io(format!(
            "read outcome {}: {error}",
            path.display()
        ))),
    }
}

/// The marker the orchestrator writes after the agent is dead and the delivery commit is gated.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentDoneMarker {
    /// Echo of [`Phase1Inputs::handoff_nonce`]. Only the orchestrator knows it.
    pub nonce: String,
    /// The gated delivery commit the push must publish (C6).
    pub expected_oid: String,
}

/// Write the marker (container side). Atomic, so the host never reads a half-written marker.
pub fn write_agent_done_marker(
    out_dir: &Path,
    nonce: &str,
    expected_oid: &str,
) -> Result<(), OrchestratorError> {
    let marker = AgentDoneMarker {
        nonce: nonce.to_owned(),
        expected_oid: expected_oid.to_owned(),
    };
    let json = serde_json::to_string(&marker)
        .map_err(|error| OrchestratorError::Io(format!("encode marker: {error}")))?;
    write_file_atomically(&out_dir.join(AGENT_DONE_MARKER), &json, None)
}

/// Read and verify the marker (host side). `Ok(None)` while it has not appeared. A marker that does
/// not parse, carries the wrong nonce, or names a malformed oid is [`OrchestratorError::Tampered`]:
/// only a job process could have written it, and the host must not mint a token for it.
pub fn read_agent_done_marker(
    out_dir: &Path,
    expected_nonce: &str,
) -> Result<Option<AgentDoneMarker>, OrchestratorError> {
    let path = out_dir.join(AGENT_DONE_MARKER);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(OrchestratorError::Io(format!(
                "read marker {}: {error}",
                path.display()
            )))
        }
    };
    let marker: AgentDoneMarker = serde_json::from_str(&raw).map_err(|error| {
        OrchestratorError::Tampered(format!("marker does not parse ({error}); not minting a token"))
    })?;
    if marker.nonce != expected_nonce {
        return Err(OrchestratorError::Tampered(
            "marker nonce mismatch — a job process may have forged it; not minting a token".into(),
        ));
    }
    if marker.expected_oid.len() != 40 || !marker.expected_oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(OrchestratorError::Tampered(format!(
            "marker expected_oid is not a 40-char hex oid: {:?}",
            marker.expected_oid
        )));
    }
    Ok(Some(marker))
}

/// Wait for the fresh push token (container side, fresh-after-agent mode): poll for
/// [`PUSH_TOKEN_FILE`] up to `wait`, then read it and DELETE it, so the token does not stay on the
/// shared volume. Called only after the agent and every other process are dead. The returned header
/// goes straight into the push and is never logged.
fn await_push_token(out_dir: &Path, wait: Duration) -> Result<String, OrchestratorError> {
    let path = out_dir.join(PUSH_TOKEN_FILE);
    if !wait_for_file(&path, wait, TOKEN_POLL) {
        return Err(OrchestratorError::TokenUnavailable(format!(
            "no push token arrived within {}s of the agent-done marker",
            wait.as_secs()
        )));
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| OrchestratorError::TokenUnavailable(format!("read token file: {error}")))?;
    if let Err(error) = std::fs::remove_file(&path) {
        eprintln!("sandbox orchestrator: could not delete the consumed token file: {error}");
    }
    let header = raw.trim();
    if !header.starts_with("Nostr ") {
        return Err(OrchestratorError::TokenUnavailable(
            "token file does not hold a NIP-98 Authorization header".into(),
        ));
    }
    Ok(header.to_owned())
}

/// Poll until `path` exists or `timeout` elapses. `true` when it appeared. Pure over the
/// filesystem, so both directions of the hand-off can be tested with a background writer.
pub fn wait_for_file(path: &Path, timeout: Duration, poll: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll);
    }
}

/// The NIP-40 `expiration` for a long-lived scoped token, or a refusal (§7(b) of the relay brief):
/// the token must cover `deadline + PUSH_MARGIN_SECS`, and the relay rejects a scoped token whose
/// `expiration - created_at` exceeds its cap, so the host refuses at MINT time when the lifetime from
/// `now` would exceed `cap_secs` — a clear seller-side error instead of a 403 at push time.
pub fn long_lived_expiration(
    deadline_unix: u64,
    now_unix: u64,
    cap_secs: u64,
) -> Result<i64, OrchestratorError> {
    let expiry = deadline_unix.saturating_add(PUSH_MARGIN_SECS);
    let lifetime = expiry.saturating_sub(now_unix);
    if lifetime > cap_secs {
        return Err(OrchestratorError::TokenUnavailable(format!(
            "a long-lived push token would live {lifetime}s (job deadline + {PUSH_MARGIN_SECS}s \
             push margin), over the relay cap of {cap_secs}s; shorten the job deadline, raise \
             [sandbox] container_delivery_token_cap_secs to the relay's cap, or use \
             container_delivery_token = \"fresh-after-agent\""
        )));
    }
    i64::try_from(expiry).map_err(|_| {
        OrchestratorError::TokenUnavailable("token expiration does not fit an i64".into())
    })
}

/// The runtime variables every agent child needs regardless of harness: the process baseline the
/// image sets (`PATH`, `HOME`, the XDG dirs the sandbox image points at a writable home), locale,
/// TLS roots, and proxy settings. Names only — values come from the container environment.
pub const AGENT_ENV_BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "HOSTNAME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TMPDIR",
    "TZ",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "NODE_VERSION",
    "YARN_VERSION",
    "NODE_OPTIONS",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NIX_PATH",
    "NIX_CONFIG",
    "NIX_SSL_CERT_FILE",
    "CLAUDE_CODE_EXECUTABLE",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
];

/// C4: the agent's WHOLE environment. Exactly the names in [`AGENT_ENV_BASELINE`] and `host_names`
/// (what the host placed in the container for the agent) that `lookup` resolves, plus the delivery
/// identity's git env (which wins over any same-named entry). Anything else in the orchestrator's
/// environment is absent by construction — the push token, `job_hash` and the inputs path are never
/// in the environment at all, and this allowlist keeps that true even if a later change puts a
/// secret into the container environment for the orchestrator's own use.
pub fn agent_env_allowlist(
    host_names: &[String],
    identity: &DeliveryAgentIdentity,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for name in AGENT_ENV_BASELINE
        .iter()
        .copied()
        .chain(host_names.iter().map(String::as_str))
    {
        let name = name.trim();
        if name.is_empty() || seen.contains(&name) {
            continue;
        }
        seen.push(name);
        if let Some(value) = lookup(name) {
            out.push((name.to_owned(), value));
        }
    }
    for (key, value) in identity.git_env() {
        match out.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = value,
            None => out.push((key, value)),
        }
    }
    out
}

/// Create (or re-create) the host-side exchange directory for a job, mode `0700`. A stale directory
/// from an earlier attempt is removed first so no old marker or token can be read as this run's.
/// Fails closed when the mode cannot be set: the directory will hold a token.
pub fn create_exchange_dir(dir: &Path) -> Result<(), OrchestratorError> {
    if dir.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(dir).map_err(|error| {
            OrchestratorError::Io(format!("remove stale exchange dir {}: {error}", dir.display()))
        })?;
    }
    std::fs::create_dir_all(dir).map_err(|error| {
        OrchestratorError::Io(format!("create exchange dir {}: {error}", dir.display()))
    })?;
    restrict_mode(dir, 0o700)
}

/// Write `contents` to `path` as a NEW file with mode `0600` from its first byte (host side: the
/// inputs file and the fresh token file). Refuses to overwrite: a file already there is either stale
/// or planted, and neither may silently become this run's. Fails closed off unix, where the mode
/// cannot be guaranteed.
pub fn write_secret_file(path: &Path, contents: &str) -> Result<(), OrchestratorError> {
    write_file_atomically(path, contents, Some(0o600))
}

/// Create `path` atomically: write a sibling temp file (with `mode`, when given, applied at creation),
/// then rename it into place, so a reader on the other side of the bind mount never sees a partial
/// file. Refuses when `path` already exists.
fn write_file_atomically(
    path: &Path,
    contents: &str,
    mode: Option<u32>,
) -> Result<(), OrchestratorError> {
    if path.symlink_metadata().is_ok() {
        return Err(OrchestratorError::Io(format!(
            "{} already exists; refusing to overwrite",
            path.display()
        )));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| OrchestratorError::Io(format!("{} has no file name", path.display())))?;
    let tmp = path.with_file_name(format!("{name}.tmp"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    if let Some(mode) = mode {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            return Err(OrchestratorError::Io(format!(
                "cannot restrict {} to its owner on this platform; refusing to write it",
                path.display()
            )));
        }
    }
    let result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = options.open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(OrchestratorError::Io(format!("write {}: {error}", path.display())));
    }
    Ok(())
}

/// Set `mode` on `path`, failing closed off unix.
fn restrict_mode(path: &Path, mode: u32) -> Result<(), OrchestratorError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
            OrchestratorError::Io(format!(
                "could not restrict {} to its owner: {error}",
                path.display()
            ))
        })
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        Err(OrchestratorError::Io(format!(
            "cannot restrict {} to its owner on this platform",
            path.display()
        )))
    }
}

/// Kill every process in the container except PID 1 (docker-init) and this one, then confirm none
/// is left. Runs only inside a docker container (`/.dockerenv` present), because outside one it
/// would kill the operator's own processes; there the push relies on the expected-oid check alone.
///
/// The agent's process group is already gone (the ACP driver reaps it), so what this catches is a
/// double-forked survivor the agent left behind — the one thing that could re-point the delivery
/// branch or plant a config between the gate and the push. Fails closed when anything survives.
#[cfg(all(feature = "wallet", target_os = "linux"))]
fn reap_other_processes() -> Result<(), OrchestratorError> {
    if !Path::new("/.dockerenv").exists() {
        eprintln!(
            "sandbox orchestrator: not inside a docker container; skipping the process reap (the \
             push still requires the gated oid)"
        );
        return Ok(());
    }
    for _attempt in 0..20 {
        let live = other_live_pids()?;
        if live.is_empty() {
            return Ok(());
        }
        for pid in &live {
            // SAFETY: `kill` takes a pid and a signal and touches no memory of ours. The pid was
            // read from /proc a moment ago; a stale pid at worst names a process that is gone.
            unsafe {
                libc::kill(*pid as libc::pid_t, libc::SIGKILL);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let survivors = other_live_pids()?;
    if survivors.is_empty() {
        Ok(())
    } else {
        Err(OrchestratorError::Tampered(format!(
            "{} process(es) survived the agent (pids {:?}); refusing to push",
            survivors.len(),
            survivors
        )))
    }
}

/// The production orchestrator is a `wallet` build running in a Linux container; every other build
/// (a `git-delivery`-only test build, a macOS developer run) has no `libc` or no `/proc` to reap
/// with. The push still requires the gated oid (C6), so this degrades to that check alone.
#[cfg(not(all(feature = "wallet", target_os = "linux")))]
fn reap_other_processes() -> Result<(), OrchestratorError> {
    eprintln!(
        "sandbox orchestrator: process reap unavailable on this build/platform; skipping (the push \
         still requires the gated oid)"
    );
    Ok(())
}

/// Every pid in `/proc` except 1 and this process that is not already a zombie or dead.
#[cfg(all(feature = "wallet", target_os = "linux"))]
fn other_live_pids() -> Result<Vec<u32>, OrchestratorError> {
    let me = std::process::id();
    let entries = std::fs::read_dir("/proc")
        .map_err(|error| OrchestratorError::Io(format!("list /proc: {error}")))?;
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == 1 || pid == me {
            continue;
        }
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let state = status
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .and_then(|rest| rest.trim().chars().next());
        if matches!(state, Some('Z') | Some('X')) {
            continue;
        }
        live.push(pid);
    }
    Ok(live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seller_git::snapshot_delivery_at;
    use std::cell::RefCell;
    use std::fs;

    const JOB_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DATE: i64 = 1_700_000_000;

    // Build an agent workdir repo with one committed base, one agent-written file, and a delivery
    // commit produced by the real gate. Returns (tempdir root, agent_workdir, branch, delivery_oid).
    fn agent_repo_with_delivery(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, String, String) {
        let root = std::env::temp_dir().join(format!("maxplayer-orch-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("agent");
        fs::create_dir_all(&workdir).expect("mkdir workdir");

        let id = DeliveryAgentIdentity::for_seller("f".repeat(64).as_str());
        // A base commit so the delivery is a contribution parented on it.
        let repo = git2::Repository::init(&workdir).expect("init");
        {
            let mut cfg = repo.config().expect("config");
            cfg.set_str("user.name", "base").ok();
            cfg.set_str("user.email", "base@example.invalid").ok();
        }
        fs::write(workdir.join("README.md"), "base\n").expect("write base");
        let base_oid = {
            let mut index = repo.index().expect("index");
            index.add_path(Path::new("README.md")).expect("add");
            index.write().expect("write index");
            let tree = repo.find_tree(index.write_tree().expect("tree")).expect("find tree");
            let sig = git2::Signature::new("base", "base@example.invalid", &git2::Time::new(DATE, 0))
                .expect("sig");
            repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
                .expect("commit")
                .to_string()
        };
        drop(repo);

        // The agent writes a deliverable.
        fs::write(workdir.join("answer.txt"), "the agent did work\n").expect("write answer");

        let branch = "maxplayer/abc12345".to_owned();
        let oid = snapshot_delivery_at(
            &workdir,
            &id,
            Some(base_oid.as_str()),
            &branch,
            "maxplayer delivery: task",
            DATE + 5,
            JOB_HASH,
        )
        .expect("snapshot");
        (root, workdir, branch, oid)
    }

    // (The config-rewrite security property is proven by tests/hostile_local_git_config.rs against
    // seller_git::neutralize_push_config, which push_delivery reuses.)

    const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // A from-scratch inputs literal for `root`, pushing to a remote the allowlist REFUSES — so the
    // push fails fast and locally (`TransportError::Transport`, never retried), which is what lets
    // these tests drive the whole entry sequence without a relay.
    fn inputs_for(root: &Path, push_token: PushTokenSource) -> Phase1Inputs {
        Phase1Inputs {
            job_hash: JOB_HASH.to_owned(),
            seller_pubkey_hex: "e".repeat(64),
            base: None,
            delivery_branch: "maxplayer/abc12345".to_owned(),
            message: "delivery".to_owned(),
            author_date_unix: DATE,
            agent_argv: vec!["claude-agent-acp".to_owned()],
            workdir: root.join("work"),
            out_dir: root.join("io"),
            prompt: "do the task".to_owned(),
            deadline_unix: 4_000_000_000,
            max_agent_attempts: 3,
            agent_env_names: vec!["ANTHROPIC_API_KEY".to_owned(), "ANTHROPIC_BASE_URL".to_owned()],
            relay_url: "ext::sh -c evil".to_owned(),
            push_token,
            handoff_nonce: NONCE.to_owned(),
        }
    }

    fn fresh_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("maxplayer-orch-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("io")).expect("mkdir io");
        root
    }

    // Backoff doubles from the base and caps at max_delay.
    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = PushRetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        };
        assert_eq!(backoff_delay(&policy, 2), Duration::from_millis(500));
        assert_eq!(backoff_delay(&policy, 3), Duration::from_secs(1));
        assert_eq!(backoff_delay(&policy, 4), Duration::from_secs(2));
        assert_eq!(backoff_delay(&policy, 5), Duration::from_secs(4));
        assert_eq!(backoff_delay(&policy, 6), Duration::from_secs(8));
        assert_eq!(backoff_delay(&policy, 7), Duration::from_secs(8), "capped");
        assert_eq!(backoff_delay(&policy, 99), Duration::from_secs(8), "no overflow, capped");
    }

    // Retryable classification: transient/conflict retry, permission/allowlist do not.
    #[test]
    fn retryable_classification() {
        assert!(default_is_retryable(&TransportError::Io("connect".into())));
        assert!(default_is_retryable(&TransportError::Rejected("receive-pack busy".into())));
        assert!(!default_is_retryable(&TransportError::Auth("403 ref scope".into())));
        assert!(!default_is_retryable(&TransportError::Transport("ext:: banned".into())));
    }

    // A retryable failure then a success returns the oid and sleeps exactly once.
    #[test]
    fn push_retries_a_transient_failure_then_succeeds() {
        let policy = PushRetryPolicy::default();
        let calls = RefCell::new(0u32);
        let sleeps = RefCell::new(Vec::<Duration>::new());
        let out = push_with_retry(
            &policy,
            |attempt| {
                *calls.borrow_mut() += 1;
                if attempt == 1 {
                    Err(TransportError::Io("409 conflict".into()))
                } else {
                    Ok("deadbeef".to_owned())
                }
            },
            default_is_retryable,
            |d| sleeps.borrow_mut().push(d),
            |d| d, // identity jitter for determinism
        )
        .expect("succeeds on retry");
        assert_eq!(out, "deadbeef");
        assert_eq!(*calls.borrow(), 2, "one retry");
        assert_eq!(sleeps.borrow().as_slice(), &[Duration::from_millis(500)], "one backoff wait");
    }

    // A non-retryable failure stops immediately, no sleep, no further attempt.
    #[test]
    fn push_does_not_retry_a_permission_failure() {
        let policy = PushRetryPolicy::default();
        let calls = RefCell::new(0u32);
        let slept = RefCell::new(false);
        let err = push_with_retry(
            &policy,
            |_attempt| {
                *calls.borrow_mut() += 1;
                Err(TransportError::Auth("403".into()))
            },
            default_is_retryable,
            |_d| *slept.borrow_mut() = true,
            |d| d,
        )
        .expect_err("permission is terminal");
        assert!(matches!(err, OrchestratorError::PushExhausted { attempts: 1, .. }), "got {err}");
        assert_eq!(*calls.borrow(), 1, "no retry on permission");
        assert!(!*slept.borrow(), "no backoff before giving up");
    }

    // Attempts are bounded: a permanently-conflicting push gives up after max_attempts.
    #[test]
    fn push_gives_up_after_the_attempt_bound() {
        let policy = PushRetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(40),
        };
        let calls = RefCell::new(0u32);
        let err = push_with_retry(
            &policy,
            |_attempt| {
                *calls.borrow_mut() += 1;
                Err(TransportError::Io("409".into()))
            },
            default_is_retryable,
            |_d| {},
            |d| d,
        )
        .expect_err("exhausts");
        assert!(matches!(err, OrchestratorError::PushExhausted { attempts: 3, .. }), "got {err}");
        assert_eq!(*calls.borrow(), 3, "exactly max_attempts tries");
    }

    // Phase 1 end to end (from-scratch): provision an empty repo, the agent writes a deliverable, the
    // gate commits + sentinels it into the workdir repo — no laundering, no push credential anywhere.
    // Asserts the reported oid is the committed workdir tip phase 2 will push, and the OID file
    // round-trips.
    #[test]
    fn run_phase1_from_scratch_delivers_a_gated_commit() {
        let root = std::env::temp_dir().join(format!("maxplayer-orch-p1ok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("agent");
        let out = root.join("out");
        fs::create_dir_all(&workdir).expect("mkdir workdir");
        fs::create_dir_all(&out).expect("mkdir out");
        let id = DeliveryAgentIdentity::for_seller("e".repeat(64).as_str());
        let branch = "maxplayer/def67890";

        let result = run_phase1(
            &workdir,
            &id,
            None, // from-scratch
            branch,
            "maxplayer delivery: task",
            DATE + 9,
            JOB_HASH,
            |dir| {
                // The "agent": write a deliverable into the provisioned workdir.
                fs::write(dir.join("answer.txt"), "phase-1 agent output\n")
                    .map_err(|e| OrchestratorError::Io(e.to_string()))
            },
        )
        .expect("phase 1");

        // The committed workdir repo holds exactly the reported commit on the delivery branch.
        let repo = git2::Repository::open(&workdir).expect("open workdir");
        let tip = repo
            .refname_to_id(&git_transport::delivery_ref(branch))
            .expect("tip")
            .to_string();
        assert_eq!(tip, result.delivery_oid, "reported oid is the committed workdir tip");
        assert_eq!(result.delivery_repo_dir, workdir, "phase 2 pushes from the workdir");

        // The OID file the host reads round-trips.
        write_delivery_oid(&out, &result.delivery_oid).expect("write oid");
        assert_eq!(read_delivery_oid(&out).expect("read oid"), result.delivery_oid);
        let _ = fs::remove_dir_all(&root);
    }

    // The gate still fires inside phase 1: an agent that wrote nothing yields Gate(NoExecutionObserved)
    // and no commit — the quota-dead case, unchanged by relocation.
    #[test]
    fn run_phase1_gate_refuses_when_the_agent_wrote_nothing() {
        let root = std::env::temp_dir().join(format!("maxplayer-orch-p1empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("agent");
        fs::create_dir_all(&workdir).expect("mkdir workdir");
        let id = DeliveryAgentIdentity::for_seller("e".repeat(64).as_str());

        let err = run_phase1(
            &workdir,
            &id,
            None,
            "maxplayer/def67890",
            "msg",
            DATE + 9,
            JOB_HASH,
            |_dir| Ok(()), // the agent writes nothing
        )
        .expect_err("empty tree must be refused");
        assert!(
            matches!(err, OrchestratorError::Gate(SellerGitError::NoExecutionObserved(_))),
            "got {err}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    // The inputs round-trip through the on-disk channel unchanged — every field the container reads,
    // including the B9 agent inputs and the token source — and the file is created owner-only.
    #[test]
    fn phase1_inputs_round_trip() {
        let root = fresh_root("in1");
        let mut inputs = inputs_for(
            &root,
            PushTokenSource::LongLived {
                header: "Nostr abc".to_owned(),
            },
        );
        inputs.base = Some(Phase1BaseOwned {
            clone_url: "https://relay.example/git/o/r.git".to_owned(),
            branch: "main".to_owned(),
            oid: "b".repeat(40),
        });
        let path = root.join("io").join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&path, &inputs).expect("write");
        let back: Phase1Inputs = read_json(&path).expect("read");
        assert_eq!(back.job_hash, inputs.job_hash);
        assert_eq!(back.agent_argv, inputs.agent_argv);
        assert_eq!(back.base.expect("base").oid, "b".repeat(40));
        assert_eq!(back.prompt, inputs.prompt);
        assert_eq!(back.deadline_unix, inputs.deadline_unix);
        assert_eq!(back.max_agent_attempts, inputs.max_agent_attempts);
        assert_eq!(back.agent_env_names, inputs.agent_env_names);
        assert_eq!(back.relay_url, inputs.relay_url);
        assert_eq!(back.push_token, inputs.push_token);
        assert_eq!(back.handoff_nonce, inputs.handoff_nonce);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the inputs file is owner-only from its first byte");
        }
        // Refuses to overwrite: a file already there is stale or planted.
        assert!(write_phase1_inputs(&path, &inputs).is_err(), "no silent overwrite");
        // The token source serialises under a tagged `mode`, in the documented spelling.
        let json = serde_json::to_string(&PushTokenSource::FreshAfterAgent { wait_secs: 120 })
            .expect("encode");
        assert!(json.contains("\"mode\":\"fresh-after-agent\""), "{json}");
        let _ = fs::remove_dir_all(&root);
    }

    // The whole entry sequence, long-lived mode, with a fake agent that writes a deliverable: the
    // inputs file is DELETED before the agent runs (C3), the marker carries the nonce and the gated
    // oid, the push is attempted with the in-memory header and fails fast on the refused remote, and
    // the outcome file says so. No token file is ever involved in this mode.
    #[test]
    fn entry_deletes_inputs_before_the_agent_and_reports_the_outcome() {
        let root = fresh_root("entry-ll");
        let inputs = inputs_for(
            &root,
            PushTokenSource::LongLived {
                header: "Nostr long-lived-header".to_owned(),
            },
        );
        let inputs_path = root.join("io").join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");

        let saw_inputs_at_agent_time = RefCell::new(None);
        let result = run_phase1_entry_with(&inputs_path, |inputs, workdir| {
            *saw_inputs_at_agent_time.borrow_mut() =
                Some(root.join("io").join(PHASE1_INPUTS_FILE).exists());
            assert_eq!(workdir, inputs.workdir, "the agent runs in the provisioned workdir");
            fs::write(workdir.join("answer.txt"), "entry agent output\n")
                .map_err(|e| OrchestratorError::Io(e.to_string()))?;
            Ok(AgentOutcome {
                usage: None,
                last_agent_message: Some("done".to_owned()),
            })
        });
        assert_eq!(
            saw_inputs_at_agent_time.into_inner(),
            Some(false),
            "C3: the inputs file must be gone before the agent runs"
        );
        assert!(!inputs_path.exists());

        // The push was attempted and refused (allowlist), so the run is not delivered…
        let err = result.expect_err("the refused remote fails the push");
        assert!(matches!(err, OrchestratorError::PushExhausted { attempts: 1, .. }), "{err}");
        // …but the gate ran: the marker names the gated commit and echoes the nonce.
        let marker = read_agent_done_marker(&root.join("io"), NONCE)
            .expect("marker parses")
            .expect("marker written after the agent");
        let repo = git2::Repository::open(root.join("work")).expect("open workdir");
        let tip = repo
            .refname_to_id("refs/heads/maxplayer/abc12345")
            .expect("tip")
            .to_string();
        assert_eq!(marker.expected_oid, tip, "the marker names the gated commit");
        // The outcome file classifies the failure and carries the agent's report, no oid.
        let outcome = read_outcome(&root.join("io")).expect("outcome parses").expect("written");
        assert_eq!(outcome.status, Phase1Status::PushFailed);
        assert_eq!(outcome.delivery_oid, None);
        assert_eq!(
            outcome.agent.as_ref().and_then(|a| a.last_agent_message.as_deref()),
            Some("done")
        );
        assert!(
            !outcome.detail.contains("long-lived-header"),
            "the outcome never carries the token: {}",
            outcome.detail
        );
        assert!(!root.join("io").join(PUSH_TOKEN_FILE).exists(), "no token file in this mode");
        let _ = fs::remove_dir_all(&root);
    }

    // Fresh-after-agent mode: the orchestrator writes the marker, then WAITS for the token file; a
    // "host" thread that sees the marker writes the token; the orchestrator consumes (deletes) it and
    // pushes. Proves both directions of the hand-off and that the token file is gone afterwards.
    #[test]
    fn entry_waits_for_the_fresh_token_after_the_marker_and_consumes_it() {
        let root = fresh_root("entry-fresh");
        let inputs = inputs_for(&root, PushTokenSource::FreshAfterAgent { wait_secs: 10 });
        let inputs_path = root.join("io").join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");

        // The host side: poll for the marker, verify it, drop a token.
        let io = root.join("io");
        let host = std::thread::spawn(move || {
            let marker_path = io.join(AGENT_DONE_MARKER);
            assert!(
                wait_for_file(&marker_path, Duration::from_secs(10), Duration::from_millis(10)),
                "the marker must appear"
            );
            let marker = read_agent_done_marker(&io, NONCE).expect("valid").expect("present");
            write_secret_file(&io.join(PUSH_TOKEN_FILE), "Nostr fresh-header\n").expect("token");
            marker
        });

        let agent_ran_before_marker = RefCell::new(false);
        let result = run_phase1_entry_with(&inputs_path, |_inputs, workdir| {
            *agent_ran_before_marker.borrow_mut() = !root.join("io").join(AGENT_DONE_MARKER).exists();
            fs::write(workdir.join("answer.txt"), "fresh agent output\n")
                .map_err(|e| OrchestratorError::Io(e.to_string()))?;
            Ok(AgentOutcome::default())
        });
        let marker = host.join().expect("host thread");
        assert!(agent_ran_before_marker.into_inner(), "the marker is written after the agent");
        // The token reached the push (which then failed on the refused remote), and was consumed.
        let err = result.expect_err("refused remote");
        assert!(matches!(err, OrchestratorError::PushExhausted { .. }), "{err}");
        assert!(
            !root.join("io").join(PUSH_TOKEN_FILE).exists(),
            "the consumed token file is deleted"
        );
        let outcome = read_outcome(&root.join("io")).expect("parses").expect("written");
        assert_eq!(outcome.status, Phase1Status::PushFailed, "{}", outcome.detail);
        assert!(!outcome.detail.contains("fresh-header"), "no token in the outcome");
        assert_eq!(marker.nonce, NONCE);
        let _ = fs::remove_dir_all(&root);
    }

    // Fresh-after-agent mode with a host that never answers: the wait is bounded and the outcome
    // says the token did not arrive. Nothing is pushed.
    #[test]
    fn entry_times_out_when_no_token_arrives() {
        let root = fresh_root("entry-timeout");
        let inputs = inputs_for(&root, PushTokenSource::FreshAfterAgent { wait_secs: 1 });
        let inputs_path = root.join("io").join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");
        let started = std::time::Instant::now();
        let err = run_phase1_entry_with(&inputs_path, |_inputs, workdir| {
            fs::write(workdir.join("answer.txt"), "output\n")
                .map_err(|e| OrchestratorError::Io(e.to_string()))?;
            Ok(AgentOutcome::default())
        })
        .expect_err("no token ⇒ no push");
        assert!(matches!(err, OrchestratorError::TokenUnavailable(_)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(8), "the wait is bounded");
        let outcome = read_outcome(&root.join("io")).expect("parses").expect("written");
        assert_eq!(outcome.status, Phase1Status::TokenUnavailable);
        let _ = fs::remove_dir_all(&root);
    }

    // C3 fail-closed: when the inputs file cannot be deleted, the agent never runs.
    #[cfg(unix)]
    #[test]
    fn entry_refuses_to_run_the_agent_when_the_inputs_delete_fails() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = fresh_root("entry-c3");
        // Root ignores directory modes; this test proves nothing there.
        if fs::metadata(&root).expect("stat").uid() == 0 {
            return;
        }
        let inputs = inputs_for(&root, PushTokenSource::None);
        let locked = root.join("locked");
        fs::create_dir_all(&locked).expect("mkdir");
        let inputs_path = locked.join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");
        // A read-only parent makes the unlink fail with EACCES.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");

        let agent_ran = RefCell::new(false);
        let err = run_phase1_entry_with(&inputs_path, |_inputs, _workdir| {
            *agent_ran.borrow_mut() = true;
            Ok(AgentOutcome::default())
        })
        .expect_err("an undeletable inputs file refuses the run");
        assert!(matches!(err, OrchestratorError::Io(_)), "{err}");
        assert!(err.to_string().contains("refusing to start the agent"), "{err}");
        assert!(!agent_ran.into_inner(), "C3: the agent must not run");

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod back");
        let _ = fs::remove_dir_all(&root);
    }

    // The gate still fires inside the entry: an agent that wrote nothing yields no_sentinel in the
    // outcome, no marker, no push.
    #[test]
    fn entry_reports_no_sentinel_when_the_agent_wrote_nothing() {
        let root = fresh_root("entry-nosentinel");
        let inputs = inputs_for(&root, PushTokenSource::None);
        let inputs_path = root.join("io").join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");
        let err = run_phase1_entry_with(&inputs_path, |_inputs, _workdir| Ok(AgentOutcome::default()))
            .expect_err("empty tree");
        assert!(
            matches!(err, OrchestratorError::Gate(SellerGitError::NoExecutionObserved(_))),
            "{err}"
        );
        assert!(!root.join("io").join(AGENT_DONE_MARKER).exists(), "no marker without a commit");
        let outcome = read_outcome(&root.join("io")).expect("parses").expect("written");
        assert_eq!(outcome.status, Phase1Status::NoSentinel);
        let _ = fs::remove_dir_all(&root);
    }

    // C6: the push refuses a branch that no longer points at the gated commit, BEFORE any transport.
    #[test]
    fn push_refuses_when_the_branch_moved_off_the_gated_commit() {
        let (root, workdir, branch, gated) = agent_repo_with_delivery("c6");
        // A survivor re-points the delivery branch at another commit.
        {
            let repo = git2::Repository::open(&workdir).expect("open");
            let sig = git2::Signature::new("x", "x@example.invalid", &git2::Time::new(DATE, 0))
                .expect("sig");
            let gated_commit =
                repo.find_commit(git2::Oid::from_str(&gated).expect("oid")).expect("commit");
            let tree = gated_commit.tree().expect("tree");
            let other = repo
                .commit(None, &sig, &sig, "re-pointed", &tree, &[&gated_commit])
                .expect("commit");
            repo.reference(&git_transport::delivery_ref(&branch), other, true, "re-point")
                .expect("re-point");
        }

        let err = push_delivery(
            &workdir,
            "https://relay.example/git/o/r.git", // never contacted: the check comes first
            &branch,
            None,
            &PushRetryPolicy::default(),
            Some(&gated),
        )
        .expect_err("a moved branch is tamper evidence");
        assert!(matches!(err, OrchestratorError::Tampered(_)), "{err}");
        // The config was NOT yet neutralised — nothing ran past the check.
        let config = fs::read_to_string(workdir.join(".git").join("config")).expect("config");
        assert!(config.contains("user.name") || config.contains("[user]"), "untouched: {config}");
        // Control: with the branch back on the gated commit the check passes and the push proceeds
        // to the transport, which the refused remote then stops.
        let repo = git2::Repository::open(&workdir).expect("open");
        let gated_oid = git2::Oid::from_str(&gated).expect("oid");
        repo.reference(&git_transport::delivery_ref(&branch), gated_oid, true, "restore")
            .expect("restore");
        drop(repo);
        let err = push_delivery(&workdir, "ext::sh -c evil", &branch, None, &PushRetryPolicy::default(), Some(&gated))
            .expect_err("refused remote");
        assert!(matches!(err, OrchestratorError::PushExhausted { .. }), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    // The marker round-trips, and a forged one (wrong nonce, garbage, bad oid) is tamper evidence
    // rather than a token mint.
    #[test]
    fn marker_round_trips_and_rejects_a_forgery() {
        let root = fresh_root("marker");
        let io = root.join("io");
        assert_eq!(read_agent_done_marker(&io, NONCE).expect("absent is fine"), None);
        write_agent_done_marker(&io, NONCE, &"c".repeat(40)).expect("write");
        let marker = read_agent_done_marker(&io, NONCE).expect("valid").expect("present");
        assert_eq!(marker.expected_oid, "c".repeat(40));
        assert!(
            matches!(read_agent_done_marker(&io, "other-nonce"), Err(OrchestratorError::Tampered(_))),
            "wrong nonce is tamper evidence"
        );
        assert!(write_agent_done_marker(&io, NONCE, &"c".repeat(40)).is_err(), "no overwrite");

        fs::remove_file(io.join(AGENT_DONE_MARKER)).expect("rm");
        fs::write(io.join(AGENT_DONE_MARKER), "not json").expect("plant");
        assert!(matches!(read_agent_done_marker(&io, NONCE), Err(OrchestratorError::Tampered(_))));
        fs::remove_file(io.join(AGENT_DONE_MARKER)).expect("rm");
        write_agent_done_marker(&io, NONCE, "short").expect("write");
        assert!(matches!(read_agent_done_marker(&io, NONCE), Err(OrchestratorError::Tampered(_))));
        let _ = fs::remove_dir_all(&root);
    }

    // The file wait: present (written by a background thread) and timeout, both bounded.
    #[test]
    fn wait_for_file_sees_a_late_file_and_gives_up_on_time() {
        let root = fresh_root("wait");
        let late = root.join("io").join("late");
        let writer_target = late.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            fs::write(writer_target, "x").expect("write");
        });
        assert!(wait_for_file(&late, Duration::from_secs(5), Duration::from_millis(5)));
        writer.join().expect("writer");
        let started = std::time::Instant::now();
        assert!(!wait_for_file(
            &root.join("io").join("never"),
            Duration::from_millis(120),
            Duration::from_millis(10)
        ));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(120) && waited < Duration::from_secs(2), "{waited:?}");
        let _ = fs::remove_dir_all(&root);
    }

    // §7(b): a long-lived token that would outlive the relay cap is refused at mint time.
    #[test]
    fn long_lived_expiration_refuses_over_cap_and_covers_the_margin() {
        let now = 1_000_000;
        // Deadline one hour out: lifetime = 3600 + margin, under a 6 h cap.
        let expiry = long_lived_expiration(now + 3_600, now, 21_600).expect("under cap");
        assert_eq!(expiry, (now + 3_600 + PUSH_MARGIN_SECS) as i64, "expiry = deadline + margin");
        // Deadline exactly at cap minus margin fits; one second more does not.
        long_lived_expiration(now + 21_600 - PUSH_MARGIN_SECS, now, 21_600).expect("fits exactly");
        let err = long_lived_expiration(now + 21_600 - PUSH_MARGIN_SECS + 1, now, 21_600)
            .expect_err("over cap");
        assert!(matches!(err, OrchestratorError::TokenUnavailable(_)), "{err}");
        let message = err.to_string();
        assert!(message.contains("relay cap of 21600s"), "{message}");
        assert!(message.contains("fresh-after-agent"), "names the way out: {message}");
    }

    // C4: the agent's environment is the baseline + the host-named variables + the git identity,
    // and NOTHING else — a poisoned orchestrator environment does not reach the agent.
    #[test]
    fn agent_env_allowlist_excludes_everything_not_named() {
        let identity = DeliveryAgentIdentity::for_seller(&"a".repeat(64));
        let host_names = vec![
            "ANTHROPIC_API_KEY".to_owned(),
            "ANTHROPIC_BASE_URL".to_owned(),
            "CURSOR_AUTH_TOKEN".to_owned(),
            "GIT_AUTHOR_NAME".to_owned(),
        ];
        let env = agent_env_allowlist(&host_names, &identity, |key| match key {
            "PATH" => Some("/usr/local/bin:/usr/bin".to_owned()),
            "HOME" => Some("/home/agent".to_owned()),
            "ANTHROPIC_API_KEY" => Some("sk-ant-api03-placeholder".to_owned()),
            "ANTHROPIC_BASE_URL" => Some("http://host.docker.internal:9100".to_owned()),
            "CURSOR_AUTH_TOKEN" => Some("placeholder.jwt".to_owned()),
            // Poison: present in the orchestrator's environment, named by nobody.
            "MAXPLAYER_PUSH_TOKEN" => Some("Nostr secret".to_owned()),
            "MAXPLAYER_JOB_HASH" => Some(JOB_HASH.to_owned()),
            "MAXPLAYER_INPUTS" => Some("/maxplayer-io/inputs.json".to_owned()),
            "GIT_AUTHOR_NAME" => Some("attacker".to_owned()),
            _ => None,
        });
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"PATH") && keys.contains(&"HOME"));
        assert!(keys.contains(&"ANTHROPIC_API_KEY") && keys.contains(&"ANTHROPIC_BASE_URL"));
        assert!(keys.contains(&"CURSOR_AUTH_TOKEN"));
        for poison in ["MAXPLAYER_PUSH_TOKEN", "MAXPLAYER_JOB_HASH", "MAXPLAYER_INPUTS"] {
            assert!(!keys.contains(&poison), "{poison} must not reach the agent: {keys:?}");
        }
        let values = env.iter().map(|(_, v)| v.as_str()).collect::<Vec<_>>().join("\n");
        assert!(!values.contains("Nostr secret") && !values.contains(JOB_HASH));
        // The delivery identity wins over a same-named host value.
        let author = env.iter().find(|(k, _)| k == "GIT_AUTHOR_NAME").expect("identity env");
        assert_eq!(author.1, identity.name);
        assert!(env.iter().any(|(k, _)| k == "GIT_COMMITTER_EMAIL"));
        // Unset names are absent, never an empty pair.
        assert!(!keys.contains(&"XDG_RUNTIME_DIR"));
        let count = keys.iter().filter(|k| **k == "GIT_AUTHOR_NAME").count();
        assert_eq!(count, 1, "no duplicate keys");
    }

    // The outcome file round-trips every status, with the usage the driver surfaced.
    #[test]
    fn outcome_round_trips_and_classifies_errors() {
        let root = fresh_root("outcome");
        let io = root.join("io");
        assert_eq!(read_outcome(&io).expect("absent is fine"), None);
        let usage = crate::driver::UsageMetadata {
            model: Some("claude-x".to_owned()),
            input_tokens: Some(10),
            output_tokens: Some(20),
            ..Default::default()
        };
        let outcome = Phase1Outcome::from_result(
            &Ok(Phase1Output {
                delivery_oid: "d".repeat(40),
                delivery_repo_dir: PathBuf::from("/work"),
            }),
            Some(AgentOutcome {
                usage: Some(usage.clone()),
                last_agent_message: None,
            }),
            Duration::from_millis(1234),
        );
        write_outcome(&io, &outcome).expect("write");
        let back = read_outcome(&io).expect("parses").expect("present");
        assert_eq!(back, outcome);
        assert_eq!(back.status, Phase1Status::Delivered);
        assert_eq!(back.delivery_oid.as_deref(), Some("d".repeat(40).as_str()));
        assert_eq!(back.agent.and_then(|a| a.usage), Some(usage));
        assert_eq!(back.wall_time_ms, 1234);

        let classify = |error: OrchestratorError| {
            Phase1Outcome::from_result(&Err(error), None, Duration::ZERO).status
        };
        assert_eq!(classify(OrchestratorError::Agent("x".into())), Phase1Status::AgentFailed);
        assert_eq!(classify(OrchestratorError::DeadlineExceeded), Phase1Status::DeadlineExceeded);
        assert_eq!(
            classify(OrchestratorError::AgentUnavailable("x".into())),
            Phase1Status::AgentUnavailable
        );
        assert_eq!(
            classify(OrchestratorError::Gate(SellerGitError::NoExecutionObserved("x".into()))),
            Phase1Status::NoSentinel
        );
        assert_eq!(
            classify(OrchestratorError::Gate(SellerGitError::Io("x".into()))),
            Phase1Status::SnapshotFailed
        );
        assert_eq!(classify(OrchestratorError::TokenUnavailable("x".into())), Phase1Status::TokenUnavailable);
        assert_eq!(
            classify(OrchestratorError::PushExhausted { attempts: 1, last: "x".into() }),
            Phase1Status::PushFailed
        );
        assert_eq!(classify(OrchestratorError::Tampered("x".into())), Phase1Status::Tampered);
        assert_eq!(classify(OrchestratorError::Io("x".into())), Phase1Status::Aborted);
        let _ = fs::remove_dir_all(&root);
    }

    // The host-side exchange directory is owner-only, and a stale one is replaced.
    #[cfg(unix)]
    #[test]
    fn exchange_dir_is_owner_only_and_replaces_a_stale_one() {
        use std::os::unix::fs::PermissionsExt;
        let root = fresh_root("xdir");
        let dir = root.join("exchange");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join(PUSH_TOKEN_FILE), "stale").expect("stale token");
        create_exchange_dir(&dir).expect("create");
        assert!(!dir.join(PUSH_TOKEN_FILE).exists(), "stale contents are gone");
        let mode = fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        write_secret_file(&dir.join(PUSH_TOKEN_FILE), "Nostr x").expect("token");
        let mode = fs::metadata(dir.join(PUSH_TOKEN_FILE)).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the token file is owner-only from its first byte");
        assert!(!dir.join(format!("{PUSH_TOKEN_FILE}.tmp")).exists(), "no temp file left behind");
        let _ = fs::remove_dir_all(&root);
    }

    // The phase-2 entrypoint fails closed (never panics) when the remote is not allowlisted — the
    // parse + push wiring is exercised without a live relay.
    #[test]
    fn run_phase2_entry_fails_closed_on_a_bad_remote() {
        let root = std::env::temp_dir().join(format!("maxplayer-orch-entry2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let out = root.join("out");
        fs::create_dir_all(&out).expect("mkdir out");
        // A committed repo (the workdir, as phase 1 leaves it) so the push has something to send.
        let (agent_root, workdir, branch, _oid) = agent_repo_with_delivery("entry2");

        let inputs = Phase2Inputs {
            repo_dir: workdir,
            relay_url: "ext::sh -c evil".to_owned(), // not allowlisted
            delivery_branch: branch,
            header: None,
            out_dir: out,
        };
        let path = root.join("in2.json");
        write_phase2_inputs(&path, &inputs).expect("write");
        assert!(run_phase2_entry(&path).is_err(), "a non-allowlisted remote must fail closed");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&agent_root);
    }

    // The OID file fails closed on a truncated / garbage value, so it can never become a published
    // commit reference.
    #[test]
    fn read_delivery_oid_rejects_garbage() {
        let root = std::env::temp_dir().join(format!("maxplayer-orch-oid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(root.join(DELIVERY_OID_FILE), "not-an-oid\n").expect("write");
        assert!(read_delivery_oid(&root).is_err(), "non-hex is refused");
        fs::write(root.join(DELIVERY_OID_FILE), format!("{}\n", "a".repeat(40))).expect("write");
        assert_eq!(read_delivery_oid(&root).expect("valid"), "a".repeat(40));
        let _ = fs::remove_dir_all(&root);
    }
}
