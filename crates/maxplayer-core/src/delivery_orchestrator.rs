//! Container-side delivery orchestration (Track B): the git steps that must NOT run against a
//! repository the job agent could write.
//!
//! ## The exploit this closes
//! The job agent has full write access to its workdir, INCLUDING `workdir/.git/config`. libgit2
//! applies `url.<other>.insteadOf = <relay>` rewrite rules from the config of the repository that
//! RUNS an operation — when it creates the remote, for `remote_anonymous` too. [`crate::git_transport`]
//! empties the global/XDG/system config search paths so no ambient config is read, but a repo-LOCAL
//! `.git/config` in the agent's own workdir is still consulted. So if the seller host opens the
//! agent's workdir and pushes, the agent's planted `insteadOf` silently redirects that push — with
//! the seller's signed NIP-98 token — to a host the agent chose. Test:
//! `tests/hostile_local_git_config.rs`.
//!
//! A config rewrite alone does not close this. libgit2 also reads `.git/commondir` and resolves the
//! config (and `objects` and `refs`) through the directory that file names; a `.git` that is a
//! gitfile or a symlink points elsewhere too. A job that leaves such a layout behind and exits makes
//! a rewrite of `workdir/.git/config` edit the wrong file.
//!
//! ## The fix: refuse the layout, replace the config, bind the destination
//! [`push_delivery`] runs three layers, in order, before and during the push:
//! 1. **Layout gate** ([`crate::seller_git::assert_plain_repo_layout`], run by both the config
//!    replacement and the push-side repository open): `.git` must be a real directory, no
//!    `.git/commondir` entry may exist, and `.git/config` must be a regular file or absent. The
//!    repository is opened with `NO_SEARCH` and its `path()` must canonicalize to `workdir/.git`.
//! 2. **Config replacement** ([`crate::seller_git::neutralize_push_config`]): `.git/config` is
//!    unlinked and re-created with a fixed, minimal, redirect-free file — a whole-file replacement
//!    that removes `insteadOf`, `pushInsteadOf`, `remote.*.pushurl`, and every `[include]` or
//!    worktree-config file at once.
//! 3. **Destination binding** ([`crate::git_transport`]): the resolved remote URL must equal the
//!    URL we named, and every https leg the transport builds must target that same URL. A rewrite
//!    from any source — config, commondir, a future mechanism — fails before a request exists.
//!
//! Branch-scoping (the token names one ref) bounds a token that leaks anyway: it can replay a push
//! to that one ref of that one repository until it expires. That is bounded authority, not zero.
//!
//! The push runs in the CONTAINER (or, on the interim host path, after the container that ran the
//! agent has exited), so no live agent parses the seller key — an unknown git-parser bug at push time
//! hits a process that is already the sandbox, not the host. The caller reaps the agent's process
//! group before the push, so nothing can re-plant the config in the window.
//!
//! ## What the push sends (C6)
//! The push source is the gated commit OBJECT, not the local branch name: the refspec is
//! `<gated oid>:refs/heads/<branch>`. A survivor that moves the local branch under the push changes
//! nothing about the bytes sent. The remote's status report must name exactly that ref, and the
//! remote's advertisement is then read back over a new connection and must show the ref at the gated
//! oid. The local ref is never re-resolved after the push. See
//! [`crate::git_transport::push_branch_with_header`].
//!
//! The completion gate and the execution sentinel are NOT re-implemented here: the delivery commit is
//! produced by [`crate::seller_git::snapshot_delivery_at`], which runs the §19 gate (refusing an empty
//! tree) and force-stages the sentinel. This module provisions, runs the agent, gates, and pushes.
//!
//! ## One container, driven from inside (Task B9 + B2)
//! On the container delivery path — the default for `[sandbox] mode = "docker"` — the seller host
//! launches ONE container whose command is `maxplayer __deliver phase1 <inputs>`.
//! [`run_phase1_entry`] then, in order: reads the inputs and DELETES the file (C3, fail closed);
//! provisions the workdir; DRIVES the ACP agent itself through
//! [`crate::seller_exec::run_agent_with_retry`] under a pass-through policy (the container is the
//! sandbox — no nested docker), with an explicit environment allowlist (C4); gates and commits; reaps
//! every other process in the container (mandatory — see below); writes the [`AGENT_DONE_MARKER`];
//! obtains the push token per [`PushTokenSource`]; pushes with the gated oid as the expected oid (C6);
//! and writes the oid and the [`Phase1Outcome`] for the host. The host runs no git and no ACP: it
//! mints tokens (crypto), reads two small files, and publishes the kind-3403. The entry runs only
//! where the host set [`CONTAINER_DELIVERY_ENV`] on the `docker run`; a developer who runs it on a
//! host by hand is refused before anything happens.
//!
//! ## The exchange directory
//! The host owns a per-job directory (mode `0700`, outside the agent-writable workdir) and mounts it
//! at [`CONTAINER_EXCHANGE_DIR`]. The host writes [`PHASE1_INPUTS_FILE`] (mode `0600`) before launch
//! and, in the fresh-after-agent mode, [`PUSH_TOKEN_FILE`] (mode `0600`) after the marker appears. The
//! container writes [`AGENT_DONE_MARKER`], [`DELIVERY_OID_FILE`] and [`OUTCOME_FILE`].
//!
//! Every file one side reads out of this directory goes through [`read_exchange_file`]: the open
//! follows no symlink and never blocks, the OPENED descriptor must be a regular file, and the read
//! stops at a small per-file cap ([`EXCHANGE_MARKER_MAX_BYTES`] and its siblings). The host polls the
//! marker WHILE the agent lives, so without this a job could plant, under the marker's name, a FIFO
//! (a following, blocking read parks the host past every deadline check) or a symlink to `/dev/zero`
//! (the host reads until allocation fails) — no container escape and no nonce needed. The host's
//! writes refuse any entry that already exists at the name, a symlink included.
//!
//! ⚠ Who can read what, and when. The container runs every process as ONE uid (`--user`, no
//! capabilities), so the agent and the orchestrator share file permissions inside the container. The
//! contract therefore rests on TIME, not on modes: the inputs file (`job_hash`, and in long-lived mode
//! the token) is deleted BEFORE the agent is spawned, and the fresh token file is written only AFTER
//! the agent and every other process are dead and the marker — which carries a nonce only the
//! orchestrator learned from the deleted inputs — has been verified by the host. Three things hold
//! that up, and none of them is a file the job could write:
//! - the host's reads of the exchange files are hardened ([`read_exchange_file`], above), so a live
//!   job cannot park or exhaust the host through the channel;
//! - the reap is MANDATORY ([`reap_other_processes`]): it always runs, and a `/proc` that cannot be
//!   listed or a survivor that will not die fails the delivery closed. It classifies a thread group
//!   by ALL of its tasks. A group is dead only when every entry of `/proc/<tgid>/task/` is `Z` or
//!   `X`. A zombie leader with a live sibling thread is live, and the reap kills it. The reason:
//!   `/proc/<tgid>/status` reports the leader only, and Linux keeps an exited leader as a zombie
//!   while a sibling thread runs. An enumeration that fails part way refuses the delivery, because
//!   an incomplete listing proves nothing. The reap is gated on the environment the host set at
//!   `docker run` ([`CONTAINER_DELIVERY_ENV`]), read once before the agent exists — never on a
//!   sentinel file, which a root job could unlink to skip it. `tests/reap_isolated_linux.rs` runs the
//!   real reaper against such a survivor; `scripts/reap-isolated-test.sh` runs that test in a
//!   fresh container;
//! - a seat whose daemon runs as uid 0 puts the job at root INSIDE the container, where this same-uid
//!   boundary is weakest, so container delivery is not the default there: it takes an explicit
//!   `[sandbox] container_delivery = true` (`SandboxConfig::container_delivery_enabled`).
//!
//! What is NOT bounded: the same-uid memory boundary. A job process that is alive at the same time
//! as the orchestrator shares its uid, and until the two run as different uids (issue #980) the
//! defence is that no such process survives the reap. The token is branch-scoped in any case, so a
//! leak can write nothing but the seller's own delivery branch — replayable authority on one ref for
//! the token's lifetime: a bounded harm, not none.

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

// The push-side hardening lives in `crate::seller_git` (layout gate + config replacement) and
// `crate::git_transport` (destination binding, object-sourced push, remote read-back). The container
// push below reuses it — one definition, shared with the host path.

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
    /// points at the gated commit here. Phase 2 refuses an unexpected repository layout, replaces
    /// the config, binds every leg to the relay URL, and pushes `delivery_oid` as an object with a
    /// branch-scoped token. A token that leaks anyway is bounded to that one ref (Track A).
    pub delivery_repo_dir: PathBuf,
}

/// Phase 1 of container-side delivery: provision the workdir, run the agent, gate + sentinel +
/// commit — all with NO push credential present. The host mints the scoped token only after this
/// returns; phase 2 ([`push_delivery`]) then pushes the committed repo. No seller key and no push
/// token exists in this process.
///
/// This does not push. The push ([`push_delivery`]) gates the workdir layout, replaces its config
/// ([`crate::seller_git::neutralize_push_config`]), binds the transport to the relay URL, and pushes
/// the gated object from the same workdir — no separate clean repo needed.
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
/// published commit reference. Reads through [`read_exchange_file`]: the file sits on the mount the
/// job could write, so no symlink is followed and no more than [`EXCHANGE_OID_MAX_BYTES`] is read.
pub fn read_delivery_oid(out_dir: &Path) -> Result<String, OrchestratorError> {
    let raw = read_exchange_file(out_dir, DELIVERY_OID_FILE, EXCHANGE_OID_MAX_BYTES)?
        .ok_or_else(|| OrchestratorError::Io("read delivery oid: the file is absent".into()))?;
    let raw = String::from_utf8(raw)
        .map_err(|_| OrchestratorError::Io("delivery oid file is not UTF-8".into()))?;
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
    /// The MCP servers the host minted for this launch — the Proxy swap vendor tools, one entry per
    /// `[sandbox] mcp_tools`, each carrying only a per-job placeholder and the proxy's address
    /// ([`crate::seller_exec::PreparedLaunch::mcp_servers`]). The orchestrator attaches them to the
    /// agent's session as they are; inside the container the policy is pass-through and mints
    /// nothing, so this is the only way they reach the session. Carries NO secret. Optional and
    /// empty by default, so an inputs file written by a host without this field still parses.
    #[serde(default)]
    pub mcp_servers: Vec<crate::driver::McpServer>,
    /// The delivery remote the orchestrator pushes to (the seller's `git_remote`).
    pub relay_url: String,
    /// How the push token is obtained. See [`PushTokenSource`] for who can read it and when.
    pub push_token: PushTokenSource,
    /// Random, host-generated, one per launch. The orchestrator echoes it in [`AGENT_DONE_MARKER`] so
    /// the host can tell the orchestrator's marker from one a job process forged: the agent never
    /// sees this file, so it never learns the nonce.
    pub handoff_nonce: String,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, OrchestratorError> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        OrchestratorError::Io(format!("read inputs {}: {error}", path.display()))
    })?;
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

/// The container orchestrator entrypoint (`maxplayer __deliver phase1 <inputs>`), driving the real
/// ACP agent. Reads `inputs_path`, DELETES it (C3, fail closed), provisions the workdir, runs the agent
/// through [`crate::seller_exec::run_agent_with_retry`] under a pass-through policy with an explicit
/// environment allowlist (C4), gates and commits, reaps every other process, hands off the token per
/// [`PushTokenSource`], pushes with the gated oid as the expected oid (C6), and writes the oid and the
/// [`Phase1Outcome`] for the host. The seller key never enters this process; in long-lived mode the
/// scoped token does, and it is in memory only by the time the agent exists.
#[cfg(feature = "wallet")]
pub fn run_phase1_entry(inputs_path: &Path) -> Result<Phase1Output, OrchestratorError> {
    // The reap needs `/proc` and `kill`: a Linux build. Any other build is not a supported delivery
    // container, and the refusal comes here, before an agent runs for nothing.
    if !cfg!(target_os = "linux") {
        return Err(OrchestratorError::AgentUnavailable(
            "the delivery container is a Linux build; this build cannot reap the container's \
             processes, so it refuses to run phase1"
                .into(),
        ));
    }
    run_phase1_entry_with(
        inputs_path,
        |key| std::env::var(key).ok(),
        drive_acp_agent,
        reap_other_processes,
    )
}

/// [`run_phase1_entry`] over injected seams, so the whole contract — the container gate, the
/// fail-closed inputs delete, the marker, the reap, the token hand-off, the expected-oid push, the
/// outcome file — is exercised without a container or a real ACP harness:
/// - `container_env(name)` reads the orchestrator's environment (production: `std::env::var`);
/// - `run_agent(inputs, workdir)` runs the agent against the provisioned workdir and returns what it
///   reported;
/// - `reap()` kills every other process in the container and fails when it cannot prove none is
///   left (production: [`reap_other_processes`]).
///
/// The FIRST thing that happens is the container gate ([`assert_delivery_container`]): without it
/// nothing is read, nothing is deleted, and no agent runs. The outcome file is written on EVERY later
/// exit, success or failure, so the host learns why a container exited non-zero without parsing its
/// logs.
pub fn run_phase1_entry_with(
    inputs_path: &Path,
    container_env: impl Fn(&str) -> Option<String>,
    run_agent: impl FnOnce(&Phase1Inputs, &Path) -> Result<AgentOutcome, OrchestratorError>,
    reap: impl FnOnce() -> Result<(), OrchestratorError>,
) -> Result<Phase1Output, OrchestratorError> {
    assert_delivery_container(container_env)?;
    let inputs = read_json::<Phase1Inputs>(inputs_path)?;
    // C3: the inputs file holds job_hash, the nonce and possibly the token. It must be gone before
    // any job code exists. A delete that fails is a refusal to run the agent, not a warning.
    delete_inputs_fail_closed(inputs_path)?;

    let started = std::time::Instant::now();
    let mut agent: Option<AgentOutcome> = None;
    let result = deliver_in_container(&inputs, &mut agent, run_agent, reap);
    let outcome = Phase1Outcome::from_result(&result, agent, started.elapsed());
    if let Err(error) = write_outcome(&inputs.out_dir, &outcome) {
        eprintln!("sandbox orchestrator: could not write the outcome file: {error}");
    }
    result
}

/// The container gate: `phase1` runs only where the host set [`CONTAINER_DELIVERY_ENV`] on the
/// `docker run`. Read once, before anything else — the job cannot change an environment the
/// orchestrator already read, and nothing on the shared filesystem is consulted. A developer who runs
/// `maxplayer __deliver phase1` on a host by hand is refused here, which is what keeps the mandatory
/// reap from killing that host's processes.
fn assert_delivery_container(
    container_env: impl Fn(&str) -> Option<String>,
) -> Result<(), OrchestratorError> {
    match container_env(CONTAINER_DELIVERY_ENV) {
        Some(value) if value == CONTAINER_DELIVERY_ENV_VALUE => Ok(()),
        _ => Err(OrchestratorError::AgentUnavailable(format!(
            "{CONTAINER_DELIVERY_ENV}={CONTAINER_DELIVERY_ENV_VALUE} is not set: `maxplayer \
             __deliver phase1` runs only as the command of the seller's delivery container, where \
             the host sets it; refusing to run the agent or to reap any process here"
        ))),
    }
}

/// The delivery proper, after the gate passed, the inputs are in memory and the file is gone:
/// provision → agent → gate + commit → reap → marker → token → push → oid file. Split from
/// [`run_phase1_entry_with`] so the outcome file can be written from the result of the whole
/// sequence.
fn deliver_in_container(
    inputs: &Phase1Inputs,
    agent: &mut Option<AgentOutcome>,
    run_agent: impl FnOnce(&Phase1Inputs, &Path) -> Result<AgentOutcome, OrchestratorError>,
    reap: impl FnOnce() -> Result<(), OrchestratorError>,
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
    // and BEFORE the push reads the branch (C6's threat is exactly a survivor re-pointing it). The
    // reap is mandatory: one that cannot prove the container empty fails the delivery closed here.
    reap()?;
    // The agent could write into the exchange mount while it lived (same uid). Now that nothing else
    // runs, discard anything it planted there, so every file the host reads from here on is ours.
    remove_planted_exchange_files(&inputs.out_dir);
    write_agent_done_marker(&inputs.out_dir, &inputs.handoff_nonce, &output.delivery_oid)?;

    let header = match &inputs.push_token {
        PushTokenSource::None => None,
        PushTokenSource::LongLived { header } => Some(header.clone()),
        PushTokenSource::FreshAfterAgent { wait_secs } => Some(await_push_token(
            &inputs.out_dir,
            Duration::from_secs(*wait_secs),
        )?),
    };
    let pushed = push_delivery(
        &output.delivery_repo_dir,
        &inputs.relay_url,
        &inputs.delivery_branch,
        header,
        &PushRetryPolicy::default(),
        &output.delivery_oid,
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
fn drive_acp_agent(
    inputs: &Phase1Inputs,
    workdir: &Path,
) -> Result<AgentOutcome, OrchestratorError> {
    use crate::seller_exec::{
        AgentRunTimeout, ExecError, JobAttachments, SandboxPolicy, run_agent_job_in_env,
        run_agent_with_retry, unified_job_timeout,
    };
    let identity = DeliveryAgentIdentity::for_seller(&inputs.seller_pubkey_hex);
    let env = agent_env_allowlist(&inputs.agent_env_names, &identity, |key| {
        std::env::var(key).ok()
    });
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
            // The host's MCP server list rides along: the pass-through policy here mints none, and
            // the vendor tools were minted when the host prepared this container.
            run_agent_job_in_env(
                &inputs.agent_argv,
                &policy,
                &inputs.prompt,
                workdir,
                &identity,
                AgentRunTimeout::JobDeadline(timeout),
                Some(env.clone()),
                // Servers only: the HOST mounted this container, so there is nothing to mount here.
                JobAttachments { mcp_servers: inputs.mcp_servers.clone(), extra_mounts: Vec::new() },
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
                });
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
/// Before the first attempt the workdir passes the layout gate and its config is REPLACED
/// ([`crate::seller_git::neutralize_push_config`]); an unexpected layout (a gitfile or symlinked
/// `.git`, a `.git/commondir`, a symlinked config) is refused before any network. The caller MUST
/// have reaped the agent's process group already (see that function's docs).
///
/// `gated_oid` (C6) is the commit the gate produced. It is checked and used three ways:
/// 1. Before the push, the local delivery branch must point at it. A survivor that re-pointed the
///    branch between the gate and the push is tamper evidence: [`OrchestratorError::Tampered`], and
///    no push happens.
/// 2. It is the push SOURCE on every attempt (`<gated_oid>:refs/heads/<branch>`), so what the wire
///    carries does not depend on the local branch at push time.
/// 3. After each attempt the remote's advertisement must show the branch at it, and the returned
///    oid is that attested value — the local ref is never re-resolved.
pub fn push_delivery(
    repo_dir: &Path,
    relay_url: &str,
    branch: &str,
    header: Option<String>,
    policy: &PushRetryPolicy,
    gated_oid: &str,
) -> Result<String, OrchestratorError> {
    let tip = local_branch_tip(repo_dir, branch)?;
    if tip != gated_oid {
        return Err(OrchestratorError::Tampered(format!(
            "delivery branch {branch} points at {tip}, not at the gated commit {gated_oid}; \
             refusing to push"
        )));
    }
    seller_git::neutralize_push_config(repo_dir)
        .map_err(|error| OrchestratorError::Io(error.to_string()))?;
    // Every attempt pushes the same gated object: the closure captures `gated_oid` by reference.
    let pushed = push_with_retry(
        policy,
        |_attempt| {
            git_transport::push_branch_with_header(
                repo_dir,
                relay_url,
                branch,
                gated_oid,
                header.clone(),
            )
        },
        default_is_retryable,
        |wait| std::thread::sleep(wait),
        full_jitter,
    )?;
    if pushed != gated_oid {
        return Err(OrchestratorError::Tampered(format!(
            "pushed {pushed} for {branch}, not the gated commit {gated_oid}"
        )));
    }
    Ok(pushed)
}

/// The commit the local delivery branch points at in `repo_dir` (full hex). Opens the workdir
/// through the layout gate ([`crate::seller_git::open_plain_workdir_repo`]) and reads the ref only —
/// no network.
fn local_branch_tip(repo_dir: &Path, branch: &str) -> Result<String, OrchestratorError> {
    let repo = seller_git::open_plain_workdir_repo(repo_dir)
        .map_err(|error| OrchestratorError::Io(format!("open delivery repo: {error}")))?;
    repo.refname_to_id(&git_transport::delivery_ref(branch))
        .map(|oid| oid.to_string())
        .map_err(|error| {
            OrchestratorError::Io(format!("resolve delivery branch {branch}: {error}"))
        })
}

/// Off-runtime [`push_delivery`]: gate the layout, replace `workdir`'s config, and push the gated
/// object from it, on a blocking thread. Returns the attested oid.
///
/// Safe on the host interim path because the container that ran the agent has already exited: no agent
/// process is alive to re-plant the config between the neutralise and the push.
pub async fn push_delivery_off_runtime(
    workdir: PathBuf,
    remote_url: String,
    branch: String,
    header: Option<String>,
    policy: PushRetryPolicy,
    gated_oid: String,
) -> Result<String, OrchestratorError> {
    tokio::task::spawn_blocking(move || {
        push_delivery(
            &workdir,
            &remote_url,
            &branch,
            header,
            &policy,
            &gated_oid,
        )
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
/// The environment variable the HOST sets on the delivery container's `docker run` (`-e`), and the
/// ONE gate that lets `maxplayer __deliver phase1` run at all. [`run_phase1_entry`] reads it once, at
/// startup, before the agent exists; a job process cannot change an environment the orchestrator
/// already read. Nothing on the filesystem decides this: the job can write the filesystem it shares
/// with the orchestrator, and a root job could unlink any sentinel there.
pub const CONTAINER_DELIVERY_ENV: &str = "MAXPLAYER_DELIVER_CONTAINER";
/// The value the host sets [`CONTAINER_DELIVERY_ENV`] to. Any other value, or none, is a refusal.
pub const CONTAINER_DELIVERY_ENV_VALUE: &str = "1";
/// The marker the orchestrator writes after the agent is dead and the delivery commit is gated. Its
/// nonce proves the orchestrator, not a job process, wrote it; its `expected_oid` is the gated commit.
pub const AGENT_DONE_MARKER: &str = "agent-done";
/// The fresh push token the host writes (mode `0600`) after it verified the marker, in the
/// fresh-after-agent mode. Read and DELETED by the orchestrator. It never exists while the agent does.
pub const PUSH_TOKEN_FILE: &str = "push-token";
/// The orchestrator's account of the run, written on every exit ([`Phase1Outcome`]).
pub const OUTCOME_FILE: &str = "outcome.json";
/// Size caps, in bytes, for the files one side reads out of the exchange directory
/// ([`read_exchange_file`]). The host reads the marker WHILE the job agent lives and shares the
/// mount, so the cap is what bounds the host's allocation for a job-controlled file. Each cap is far
/// above the honest size: a marker is about 130 bytes, the oid file is 41.
pub const EXCHANGE_MARKER_MAX_BYTES: usize = 4 * 1024;
/// Cap for [`DELIVERY_OID_FILE`]: 40 hex chars and a newline.
pub const EXCHANGE_OID_MAX_BYTES: usize = 4 * 1024;
/// Cap for [`OUTCOME_FILE`]. The two free-text fields of a [`Phase1Outcome`] are each bounded to
/// [`OUTCOME_TEXT_MAX_BYTES`] before the write, and JSON escaping expands a byte at most six-fold, so
/// an honest outcome always fits.
pub const EXCHANGE_OUTCOME_MAX_BYTES: usize = 64 * 1024;
/// Cap for [`PUSH_TOKEN_FILE`]: one NIP-98 `Authorization` header, about 1 KiB.
pub const EXCHANGE_TOKEN_MAX_BYTES: usize = 64 * 1024;
/// The bound on each free-text field of a [`Phase1Outcome`] (`detail`, `agent.last_agent_message`)
/// as written. The agent authors its last message, so without this bound a hostile agent could make
/// its own honest outcome exceed [`EXCHANGE_OUTCOME_MAX_BYTES`] and be refused.
pub const OUTCOME_TEXT_MAX_BYTES: usize = 4 * 1024;
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
        let mut outcome = match result {
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
        };
        outcome.bound_text();
        outcome
    }

    /// Cap `detail` and `agent.last_agent_message` at [`OUTCOME_TEXT_MAX_BYTES`] each, on a char
    /// boundary, so the written file always fits under [`EXCHANGE_OUTCOME_MAX_BYTES`]. The host
    /// refuses a longer file as planted; an agent that talks too much must not make its own honest
    /// outcome unreadable.
    fn bound_text(&mut self) {
        truncate_to_char_boundary(&mut self.detail, OUTCOME_TEXT_MAX_BYTES);
        if let Some(message) = self
            .agent
            .as_mut()
            .and_then(|agent| agent.last_agent_message.as_mut())
        {
            truncate_to_char_boundary(message, OUTCOME_TEXT_MAX_BYTES);
        }
    }
}

/// Truncate `text` to at most `max_bytes`, cutting back to a char boundary so the result is valid
/// UTF-8. A no-op when the text already fits.
fn truncate_to_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
}

/// Write the outcome file (container side). Replaces any file already there: this is written on
/// every exit, including exits before the reap, and the orchestrator's account must win over
/// anything a job process planted under the same name.
pub fn write_outcome(out_dir: &Path, outcome: &Phase1Outcome) -> Result<(), OrchestratorError> {
    let json = serde_json::to_string(outcome)
        .map_err(|error| OrchestratorError::Io(format!("encode outcome: {error}")))?;
    let path = out_dir.join(OUTCOME_FILE);
    let _ = std::fs::remove_file(&path);
    write_file_atomically(&path, &json, None)
}

/// Remove every hand-off file a job process could have planted in the exchange directory while it
/// lived — a fake marker, a fake token, a fake oid or outcome, and their temp siblings. Called after
/// the reap, when nothing but the orchestrator runs, so what the orchestrator writes next is the only
/// copy. Best effort: a file that cannot be removed makes the following write refuse, which fails
/// the delivery closed rather than silently.
fn remove_planted_exchange_files(out_dir: &Path) {
    for name in [
        AGENT_DONE_MARKER,
        PUSH_TOKEN_FILE,
        DELIVERY_OID_FILE,
        OUTCOME_FILE,
    ] {
        for candidate in [out_dir.join(name), out_dir.join(format!("{name}.tmp"))] {
            match std::fs::remove_file(&candidate) {
                Ok(()) => eprintln!(
                    "sandbox orchestrator: removed a planted file {} before the hand-off",
                    candidate.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => eprintln!(
                    "sandbox orchestrator: could not remove {}: {error}",
                    candidate.display()
                ),
            }
        }
    }
}

/// Read the outcome file (host side). `Ok(None)` when the container never wrote one. Reads through
/// [`read_exchange_file`] under the [`EXCHANGE_OUTCOME_MAX_BYTES`] cap: the file sits on the mount
/// the job could write.
pub fn read_outcome(out_dir: &Path) -> Result<Option<Phase1Outcome>, OrchestratorError> {
    let Some(raw) = read_exchange_file(out_dir, OUTCOME_FILE, EXCHANGE_OUTCOME_MAX_BYTES)? else {
        return Ok(None);
    };
    serde_json::from_slice(&raw).map(Some).map_err(|error| {
        OrchestratorError::Io(format!(
            "parse outcome {}: {error}",
            out_dir.join(OUTCOME_FILE).display()
        ))
    })
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
///
/// The host polls this WHILE the job agent lives, on a mount the agent shares, so the read goes
/// through [`read_exchange_file`]: a FIFO cannot park the poll loop, a symlink is not followed, and
/// no more than [`EXCHANGE_MARKER_MAX_BYTES`] is read — all BEFORE the nonce is looked at.
pub fn read_agent_done_marker(
    out_dir: &Path,
    expected_nonce: &str,
) -> Result<Option<AgentDoneMarker>, OrchestratorError> {
    let Some(raw) = read_exchange_file(out_dir, AGENT_DONE_MARKER, EXCHANGE_MARKER_MAX_BYTES)?
    else {
        return Ok(None);
    };
    let marker: AgentDoneMarker = serde_json::from_slice(&raw).map_err(|error| {
        OrchestratorError::Tampered(format!(
            "marker does not parse ({error}); not minting a token"
        ))
    })?;
    if marker.nonce != expected_nonce {
        return Err(OrchestratorError::Tampered(
            "marker nonce mismatch — a job process may have forged it; not minting a token".into(),
        ));
    }
    if marker.expected_oid.len() != 40
        || !marker.expected_oid.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(OrchestratorError::Tampered(format!(
            "marker expected_oid is not a 40-char hex oid: {:?}",
            marker.expected_oid
        )));
    }
    Ok(Some(marker))
}

/// Wait for the fresh push token (container side, fresh-after-agent mode): poll for
/// [`PUSH_TOKEN_FILE`] up to `wait`, then read it (through [`read_exchange_file`], like every other
/// exchange file) and DELETE it, so the token does not stay on the shared volume. Called only after
/// the agent and every other process are dead. The returned header goes straight into the push and
/// is never logged.
fn await_push_token(out_dir: &Path, wait: Duration) -> Result<String, OrchestratorError> {
    let path = out_dir.join(PUSH_TOKEN_FILE);
    if !wait_for_file(&path, wait, TOKEN_POLL) {
        return Err(OrchestratorError::TokenUnavailable(format!(
            "no push token arrived within {}s of the agent-done marker",
            wait.as_secs()
        )));
    }
    let raw = read_exchange_file(out_dir, PUSH_TOKEN_FILE, EXCHANGE_TOKEN_MAX_BYTES)
        .map_err(|error| OrchestratorError::TokenUnavailable(format!("read token file: {error}")))?
        .ok_or_else(|| {
            OrchestratorError::TokenUnavailable("token file vanished before it was read".into())
        })?;
    if let Err(error) = std::fs::remove_file(&path) {
        eprintln!("sandbox orchestrator: could not delete the consumed token file: {error}");
    }
    let raw = String::from_utf8(raw)
        .map_err(|_| OrchestratorError::TokenUnavailable("token file is not UTF-8".into()))?;
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
            OrchestratorError::Io(format!(
                "remove stale exchange dir {}: {error}",
                dir.display()
            ))
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
/// file. Refuses when ANY entry already exists at `path` — the check does not follow a symlink, so a
/// link a job process planted at the name is a refusal, never a write to wherever it points.
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
    // `create_new` (`O_CREAT | O_EXCL`) refuses any entry already at the temp name, a symlink
    // included. `O_NOFOLLOW` states the same intent on the open itself.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    if let Some(mode) = mode {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
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
        return Err(OrchestratorError::Io(format!(
            "write {}: {error}",
            path.display()
        )));
    }
    Ok(())
}

/// Read `name` out of the exchange directory `dir`: the ONE reader for every file the other side of
/// the mount wrote. `Ok(None)` when the file does not exist yet.
///
/// The host calls this while the job agent lives, and the agent shares the mount, so the read trusts
/// nothing about the entry it finds:
/// - the open does not follow a symlink at the final component (`O_NOFOLLOW`): a link to
///   `/dev/zero`, or to any host file, is refused rather than read;
/// - the open does not block (`O_NONBLOCK`): a FIFO with no writer returns at once instead of
///   parking the host's poll loop past every deadline check;
/// - the OPENED descriptor must be a regular file (`fstat`, not a stat of the path, so a swap between
///   a check and the open cannot slip a FIFO, a device or a directory through);
/// - at most `max_bytes` are read (`Read::take`): a longer file is refused, so the host never
///   allocates more than the cap for a file the job controls.
///
/// A symlink, a non-regular file or an over-cap file is [`OrchestratorError::Tampered`] — only a job
/// process could have planted it. Every other failure is [`OrchestratorError::Io`]. The callers treat
/// both as a refusal; nothing is retried. Off unix the flags cannot be set, so the read is refused.
pub fn read_exchange_file(
    dir: &Path,
    name: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, OrchestratorError> {
    let path = dir.join(name);
    #[cfg(not(unix))]
    {
        let _ = max_bytes;
        Err(OrchestratorError::Io(format!(
            "cannot open {} with O_NOFOLLOW | O_NONBLOCK on this platform; refusing to read it",
            path.display()
        )))
    }
    #[cfg(unix)]
    {
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        let file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err(OrchestratorError::Tampered(format!(
                    "{} is a symlink; only a job process could have planted it; not following it",
                    path.display()
                )));
            }
            Err(error) => {
                return Err(OrchestratorError::Io(format!(
                    "open {}: {error}",
                    path.display()
                )));
            }
        };
        let metadata = file.metadata().map_err(|error| {
            OrchestratorError::Io(format!("stat {}: {error}", path.display()))
        })?;
        if !metadata.file_type().is_file() {
            return Err(OrchestratorError::Tampered(format!(
                "{} is not a regular file ({:?}); only a job process could have planted it; \
                 refusing to read it",
                path.display(),
                metadata.file_type()
            )));
        }
        let limit = max_bytes as u64 + 1;
        let mut bytes = Vec::with_capacity(metadata.len().min(limit) as usize);
        file.take(limit).read_to_end(&mut bytes).map_err(|error| {
            OrchestratorError::Io(format!("read {}: {error}", path.display()))
        })?;
        if bytes.len() > max_bytes {
            return Err(OrchestratorError::Tampered(format!(
                "{} exceeds {max_bytes} bytes; only a job process would write that; refusing to \
                 read it",
                path.display()
            )));
        }
        Ok(Some(bytes))
    }
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
/// is left. MANDATORY: no file on the shared filesystem decides whether it runs. A job process can
/// write that filesystem — `/.dockerenv` once gated this, and a root job could unlink it, double-fork
/// a survivor out of the agent's process group, and have the survivor read the fresh token the host
/// wrote for a marker the orchestrator itself had signed. What keeps a developer's host processes
/// safe is the container gate in [`run_phase1_entry_with`], read from an environment the job never
/// controlled.
///
/// The agent's process group is already gone (the ACP driver reaps it), so what this catches is a
/// double-forked survivor the agent left behind — the one thing that could re-point the delivery
/// branch, plant a config, or read the fresh token between the gate and the push. Fails closed when
/// anything survives, and when `/proc` cannot be listed at all.
///
/// The survivor test is by TASK, not by process ([`other_live_pids`]). A thread group is dead only
/// when every one of its tasks is `Z` or `X`. A group whose leader is a zombie while a sibling thread
/// lives is live: `kill(tgid, SIGKILL)` reaches every thread of the group, and the next listing sees
/// the group dead, or gone. An enumeration error at any level refuses the delivery.
///
/// ⚠ `pub` for ONE caller: `tests/reap_isolated_linux.rs`, which runs this function against a real
/// zombie-leader survivor as the only process tree of a fresh container. Never call it on a host:
/// it SIGKILLs every process but pid 1 and the caller.
#[cfg(all(feature = "wallet", target_os = "linux"))]
pub fn reap_other_processes() -> Result<(), OrchestratorError> {
    reap_with(other_live_pids, kill_pid, std::thread::sleep)
}

/// The production orchestrator is a Linux container. Any other platform has no `/proc` to prove the
/// container empty with, so the reap fails CLOSED: no marker, no token, no push. (The entry refuses
/// before the agent runs on such a build; this is the second lock.)
#[cfg(all(feature = "wallet", not(target_os = "linux")))]
pub fn reap_other_processes() -> Result<(), OrchestratorError> {
    reap_with(
        || {
            Err(OrchestratorError::Io(
                "process reap unavailable on this build/platform: the delivery container is a \
                 Linux build; refusing to push"
                    .into(),
            ))
        },
        |_pid| {},
        |_wait| {},
    )
}

/// The reap over its process-table seams — `live_pids()` lists every other live pid, `kill(pid)`
/// sends SIGKILL — and a `sleep`, so the logic is testable on any platform without a real process
/// table. Kills and re-lists up to 20 times, 100 ms apart. `Ok(())` only when a listing came back
/// empty; a listing that fails is that error (fail closed: nothing proves the container empty), and
/// survivors after the last round are [`OrchestratorError::Tampered`].
#[cfg_attr(not(feature = "wallet"), allow(dead_code))]
fn reap_with(
    mut live_pids: impl FnMut() -> Result<Vec<u32>, OrchestratorError>,
    mut kill: impl FnMut(u32),
    mut sleep: impl FnMut(Duration),
) -> Result<(), OrchestratorError> {
    for _attempt in 0..20 {
        let live = live_pids()?;
        if live.is_empty() {
            return Ok(());
        }
        for pid in &live {
            kill(*pid);
        }
        sleep(Duration::from_millis(100));
    }
    let survivors = live_pids()?;
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

/// SIGKILL `pid`.
#[cfg(all(feature = "wallet", target_os = "linux"))]
fn kill_pid(pid: u32) {
    // SAFETY: `kill` takes a pid and a signal and touches no memory of ours. The pid was read from
    // /proc a moment ago; a stale pid at worst names a process that is gone.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// Every thread group in `/proc` except pid 1 and this process that still has a live task.
///
/// `/proc` lists thread-group ids, and `/proc/<tgid>/status` reports the state of the group LEADER
/// only. Linux keeps an exited leader as a zombie while a sibling thread lives (`exit_notify` reaps
/// the leader only when the thread group is empty). A leader-only test therefore misses a group whose
/// worker thread runs, and that worker shares the orchestrator's uid. The rule here is by task: a
/// group is dead only when every entry of `/proc/<tgid>/task/` is `Z` or `X`. One task in any other
/// state makes the group live. A group whose directory is gone between the listing and the read is
/// dead.
///
/// Fails closed. Each of these is an [`OrchestratorError::Io`], never a guess, because an incomplete
/// enumeration is not proof of emptiness:
/// - `/proc` cannot be listed, or the listing fails part way (no entry is dropped);
/// - a `task/` directory or a `status` file cannot be read for a reason other than "gone";
/// - a `task/` entry is not a tid, or a `status` file has no `State:` line.
#[cfg(all(feature = "wallet", target_os = "linux"))]
fn other_live_pids() -> Result<Vec<u32>, OrchestratorError> {
    let me = std::process::id();
    let entries = std::fs::read_dir("/proc")
        .map_err(|error| OrchestratorError::Io(format!("list /proc: {error}")))?;
    let mut live = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| OrchestratorError::Io(format!("list /proc: {error}")))?;
        // `/proc` also holds `self`, `sys`, `net`, …: only a numeric name is a thread group.
        let Some(tgid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if tgid == 1 || tgid == me {
            continue;
        }
        if thread_group_is_live(tgid)? {
            live.push(tgid);
        }
    }
    Ok(live)
}

/// Whether ANY task of thread group `tgid` is in a state other than `Z` (zombie) or `X` (dead).
/// `Ok(false)` when every task is `Z`/`X`, or when the group is gone. Any other failure is `Err`.
#[cfg(all(feature = "wallet", target_os = "linux"))]
fn thread_group_is_live(tgid: u32) -> Result<bool, OrchestratorError> {
    let task_dir = format!("/proc/{tgid}/task");
    let tasks = match std::fs::read_dir(&task_dir) {
        Ok(tasks) => tasks,
        Err(error) if task_is_gone(&error) => return Ok(false),
        Err(error) => {
            return Err(OrchestratorError::Io(format!("list {task_dir}: {error}")));
        }
    };
    for task in tasks {
        let task =
            task.map_err(|error| OrchestratorError::Io(format!("list {task_dir}: {error}")))?;
        let name = task.file_name();
        let Some(tid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
            return Err(OrchestratorError::Io(format!(
                "unexpected entry {name:?} in {task_dir}; refusing to classify the group"
            )));
        };
        match task_state(&format!("{task_dir}/{tid}/status"))? {
            // The task exited between the listing and the read, or it is a zombie or dead.
            None | Some('Z') | Some('X') => {}
            Some(_) => return Ok(true),
        }
    }
    Ok(false)
}

/// The first character of the `State:` line of a `/proc/…/status` file. `Ok(None)` when the task is
/// gone. `Err` when the file cannot be read for any other reason, or has no `State:` line.
#[cfg(all(feature = "wallet", target_os = "linux"))]
fn task_state(status_path: &str) -> Result<Option<char>, OrchestratorError> {
    let status = match std::fs::read_to_string(status_path) {
        Ok(status) => status,
        Err(error) if task_is_gone(&error) => return Ok(None),
        Err(error) => {
            return Err(OrchestratorError::Io(format!(
                "read {status_path}: {error}"
            )));
        }
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("State:"))
        .and_then(|rest| rest.trim().chars().next())
        .map(Some)
        .ok_or_else(|| {
            OrchestratorError::Io(format!(
                "{status_path} has no State: line; refusing to classify the task"
            ))
        })
}

/// Whether a `/proc` read failed because the task no longer exists. procfs answers `ENOENT` when the
/// tid is not found at lookup, and `ESRCH` when the task exits between the open and the read. Both
/// name a task that is gone, and neither can hide a live one.
#[cfg(all(feature = "wallet", target_os = "linux"))]
fn task_is_gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
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
    fn agent_repo_with_delivery(
        tag: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, String, String) {
        let root =
            std::env::temp_dir().join(format!("maxplayer-orch-{tag}-{}", std::process::id()));
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
            let tree = repo
                .find_tree(index.write_tree().expect("tree"))
                .expect("find tree");
            let sig =
                git2::Signature::new("base", "base@example.invalid", &git2::Time::new(DATE, 0))
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
    // seller_git::neutralize_push_config, which push_delivery reuses. The layout gate has its tests
    // in seller_git; the destination binding in git_transport and tests/push_destination_binding.rs;
    // the object-sourced push and the remote read-back in git_transport.)

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
            agent_env_names: vec![
                "ANTHROPIC_API_KEY".to_owned(),
                "ANTHROPIC_BASE_URL".to_owned(),
            ],
            mcp_servers: Vec::new(),
            relay_url: "ext::sh -c evil".to_owned(),
            push_token,
            handoff_nonce: NONCE.to_owned(),
        }
    }

    fn fresh_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("maxplayer-orch-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("io")).expect("mkdir io");
        root
    }

    /// The environment the host gives the delivery container.
    fn in_container(key: &str) -> Option<String> {
        (key == CONTAINER_DELIVERY_ENV).then(|| CONTAINER_DELIVERY_ENV_VALUE.to_owned())
    }

    /// A reap that found the container empty.
    fn reap_ok() -> Result<(), OrchestratorError> {
        Ok(())
    }

    /// [`run_phase1_entry_with`] under the container's environment and a clean reap: the two seams
    /// the hand-off tests do not vary.
    fn entry(
        inputs_path: &Path,
        run_agent: impl FnOnce(&Phase1Inputs, &Path) -> Result<AgentOutcome, OrchestratorError>,
    ) -> Result<Phase1Output, OrchestratorError> {
        run_phase1_entry_with(inputs_path, in_container, run_agent, reap_ok)
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
        assert_eq!(
            backoff_delay(&policy, 99),
            Duration::from_secs(8),
            "no overflow, capped"
        );
    }

    // Retryable classification: transient/conflict retry, permission/allowlist do not.
    #[test]
    fn retryable_classification() {
        assert!(default_is_retryable(&TransportError::Io("connect".into())));
        assert!(default_is_retryable(&TransportError::Rejected(
            "receive-pack busy".into()
        )));
        assert!(!default_is_retryable(&TransportError::Auth(
            "403 ref scope".into()
        )));
        assert!(!default_is_retryable(&TransportError::Transport(
            "ext:: banned".into()
        )));
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
        assert_eq!(
            sleeps.borrow().as_slice(),
            &[Duration::from_millis(500)],
            "one backoff wait"
        );
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
        assert!(
            matches!(err, OrchestratorError::PushExhausted { attempts: 1, .. }),
            "got {err}"
        );
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
        assert!(
            matches!(err, OrchestratorError::PushExhausted { attempts: 3, .. }),
            "got {err}"
        );
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
        assert_eq!(
            tip, result.delivery_oid,
            "reported oid is the committed workdir tip"
        );
        assert_eq!(
            result.delivery_repo_dir, workdir,
            "phase 2 pushes from the workdir"
        );

        // The OID file the host reads round-trips.
        write_delivery_oid(&out, &result.delivery_oid).expect("write oid");
        assert_eq!(
            read_delivery_oid(&out).expect("read oid"),
            result.delivery_oid
        );
        let _ = fs::remove_dir_all(&root);
    }

    // The gate still fires inside phase 1: an agent that wrote nothing yields Gate(NoExecutionObserved)
    // and no commit — the quota-dead case, unchanged by relocation.
    #[test]
    fn run_phase1_gate_refuses_when_the_agent_wrote_nothing() {
        let root =
            std::env::temp_dir().join(format!("maxplayer-orch-p1empty-{}", std::process::id()));
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
            matches!(
                err,
                OrchestratorError::Gate(SellerGitError::NoExecutionObserved(_))
            ),
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
            assert_eq!(
                mode, 0o600,
                "the inputs file is owner-only from its first byte"
            );
        }
        // Refuses to overwrite: a file already there is stale or planted.
        assert!(
            write_phase1_inputs(&path, &inputs).is_err(),
            "no silent overwrite"
        );
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
        let result = entry(&inputs_path, |inputs, workdir| {
            *saw_inputs_at_agent_time.borrow_mut() =
                Some(root.join("io").join(PHASE1_INPUTS_FILE).exists());
            assert_eq!(
                workdir, inputs.workdir,
                "the agent runs in the provisioned workdir"
            );
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
        assert!(
            matches!(err, OrchestratorError::PushExhausted { attempts: 1, .. }),
            "{err}"
        );
        // …but the gate ran: the marker names the gated commit and echoes the nonce.
        let marker = read_agent_done_marker(&root.join("io"), NONCE)
            .expect("marker parses")
            .expect("marker written after the agent");
        let repo = git2::Repository::open(root.join("work")).expect("open workdir");
        let tip = repo
            .refname_to_id("refs/heads/maxplayer/abc12345")
            .expect("tip")
            .to_string();
        assert_eq!(
            marker.expected_oid, tip,
            "the marker names the gated commit"
        );
        // The outcome file classifies the failure and carries the agent's report, no oid.
        let outcome = read_outcome(&root.join("io"))
            .expect("outcome parses")
            .expect("written");
        assert_eq!(outcome.status, Phase1Status::PushFailed);
        assert_eq!(outcome.delivery_oid, None);
        assert_eq!(
            outcome
                .agent
                .as_ref()
                .and_then(|a| a.last_agent_message.as_deref()),
            Some("done")
        );
        assert!(
            !outcome.detail.contains("long-lived-header"),
            "the outcome never carries the token: {}",
            outcome.detail
        );
        assert!(
            !root.join("io").join(PUSH_TOKEN_FILE).exists(),
            "no token file in this mode"
        );
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
                wait_for_file(
                    &marker_path,
                    Duration::from_secs(10),
                    Duration::from_millis(10)
                ),
                "the marker must appear"
            );
            let marker = read_agent_done_marker(&io, NONCE)
                .expect("valid")
                .expect("present");
            write_secret_file(&io.join(PUSH_TOKEN_FILE), "Nostr fresh-header\n").expect("token");
            marker
        });

        let agent_ran_before_marker = RefCell::new(false);
        let result = entry(&inputs_path, |_inputs, workdir| {
            *agent_ran_before_marker.borrow_mut() =
                !root.join("io").join(AGENT_DONE_MARKER).exists();
            fs::write(workdir.join("answer.txt"), "fresh agent output\n")
                .map_err(|e| OrchestratorError::Io(e.to_string()))?;
            Ok(AgentOutcome::default())
        });
        let marker = host.join().expect("host thread");
        assert!(
            agent_ran_before_marker.into_inner(),
            "the marker is written after the agent"
        );
        // The token reached the push (which then failed on the refused remote), and was consumed.
        let err = result.expect_err("refused remote");
        assert!(
            matches!(err, OrchestratorError::PushExhausted { .. }),
            "{err}"
        );
        assert!(
            !root.join("io").join(PUSH_TOKEN_FILE).exists(),
            "the consumed token file is deleted"
        );
        let outcome = read_outcome(&root.join("io"))
            .expect("parses")
            .expect("written");
        assert_eq!(
            outcome.status,
            Phase1Status::PushFailed,
            "{}",
            outcome.detail
        );
        assert!(
            !outcome.detail.contains("fresh-header"),
            "no token in the outcome"
        );
        assert_eq!(marker.nonce, NONCE);
        let _ = fs::remove_dir_all(&root);
    }

    // Files a job process planted in the exchange directory (a forged marker, a fake token, a fake
    // outcome) are discarded after the reap: the marker the host reads is the orchestrator's, and the
    // fake token never reaches the push.
    #[test]
    fn entry_discards_files_the_agent_planted_in_the_exchange_dir() {
        let root = fresh_root("entry-planted");
        let inputs = inputs_for(&root, PushTokenSource::FreshAfterAgent { wait_secs: 1 });
        let inputs_path = root.join("io").join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");
        let io = root.join("io");
        let err = entry(&inputs_path, |_inputs, workdir| {
            // The "agent" plants a forged marker with a guessed nonce, a fake token and a fake outcome.
            fs::write(
                io.join(AGENT_DONE_MARKER),
                format!(
                    "{{\"nonce\":\"{NONCE}\",\"expected_oid\":\"{}\"}}",
                    "0".repeat(40)
                ),
            )
            .expect("plant");
            fs::write(io.join(PUSH_TOKEN_FILE), "Nostr planted").expect("plant");
            fs::write(io.join(OUTCOME_FILE), "{\"status\":\"delivered\"}").expect("plant");
            fs::write(workdir.join("answer.txt"), "output\n")
                .map_err(|e| OrchestratorError::Io(e.to_string()))?;
            Ok(AgentOutcome::default())
        })
        .expect_err("no real token arrives");
        // The planted token was discarded, so the wait timed out rather than pushing with it.
        assert!(
            matches!(err, OrchestratorError::TokenUnavailable(_)),
            "{err}"
        );
        // The marker on disk is the orchestrator's: it names the real gated commit, not the planted oid.
        let marker = read_agent_done_marker(&io, NONCE)
            .expect("valid")
            .expect("present");
        assert_ne!(marker.expected_oid, "0".repeat(40));
        let repo = git2::Repository::open(root.join("work")).expect("open");
        let tip = repo
            .refname_to_id("refs/heads/maxplayer/abc12345")
            .expect("tip")
            .to_string();
        assert_eq!(marker.expected_oid, tip);
        // The outcome is the orchestrator's account, not the planted "delivered".
        let outcome = read_outcome(&io).expect("parses").expect("present");
        assert_eq!(outcome.status, Phase1Status::TokenUnavailable);
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
        let err = entry(&inputs_path, |_inputs, workdir| {
            fs::write(workdir.join("answer.txt"), "output\n")
                .map_err(|e| OrchestratorError::Io(e.to_string()))?;
            Ok(AgentOutcome::default())
        })
        .expect_err("no token ⇒ no push");
        assert!(
            matches!(err, OrchestratorError::TokenUnavailable(_)),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the wait is bounded"
        );
        let outcome = read_outcome(&root.join("io"))
            .expect("parses")
            .expect("written");
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
        let err = entry(&inputs_path, |_inputs, _workdir| {
            *agent_ran.borrow_mut() = true;
            Ok(AgentOutcome::default())
        })
        .expect_err("an undeletable inputs file refuses the run");
        assert!(matches!(err, OrchestratorError::Io(_)), "{err}");
        assert!(
            err.to_string().contains("refusing to start the agent"),
            "{err}"
        );
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
        let err = entry(&inputs_path, |_inputs, _workdir| Ok(AgentOutcome::default()))
            .expect_err("empty tree");
        assert!(
            matches!(
                err,
                OrchestratorError::Gate(SellerGitError::NoExecutionObserved(_))
            ),
            "{err}"
        );
        assert!(
            !root.join("io").join(AGENT_DONE_MARKER).exists(),
            "no marker without a commit"
        );
        let outcome = read_outcome(&root.join("io"))
            .expect("parses")
            .expect("written");
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
            let gated_commit = repo
                .find_commit(git2::Oid::from_str(&gated).expect("oid"))
                .expect("commit");
            let tree = gated_commit.tree().expect("tree");
            let other = repo
                .commit(None, &sig, &sig, "re-pointed", &tree, &[&gated_commit])
                .expect("commit");
            repo.reference(
                &git_transport::delivery_ref(&branch),
                other,
                true,
                "re-point",
            )
            .expect("re-point");
        }

        let err = push_delivery(
            &workdir,
            "https://relay.example/git/o/r.git", // never contacted: the check comes first
            &branch,
            None,
            &PushRetryPolicy::default(),
            &gated,
        )
        .expect_err("a moved branch is tamper evidence");
        assert!(matches!(err, OrchestratorError::Tampered(_)), "{err}");
        // The config was NOT yet neutralised — nothing ran past the check.
        let config = fs::read_to_string(workdir.join(".git").join("config")).expect("config");
        assert!(
            config.contains("user.name") || config.contains("[user]"),
            "untouched: {config}"
        );
        // Control: with the branch back on the gated commit the check passes and the push proceeds
        // to the transport, which the refused remote then stops.
        let repo = git2::Repository::open(&workdir).expect("open");
        let gated_oid = git2::Oid::from_str(&gated).expect("oid");
        repo.reference(
            &git_transport::delivery_ref(&branch),
            gated_oid,
            true,
            "restore",
        )
        .expect("restore");
        drop(repo);
        let err = push_delivery(
            &workdir,
            "ext::sh -c evil",
            &branch,
            None,
            &PushRetryPolicy::default(),
            &gated,
        )
        .expect_err("refused remote");
        assert!(
            matches!(err, OrchestratorError::PushExhausted { .. }),
            "{err}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    // The marker round-trips, and a forged one (wrong nonce, garbage, bad oid) is tamper evidence
    // rather than a token mint.
    #[test]
    fn marker_round_trips_and_rejects_a_forgery() {
        let root = fresh_root("marker");
        let io = root.join("io");
        assert_eq!(
            read_agent_done_marker(&io, NONCE).expect("absent is fine"),
            None
        );
        write_agent_done_marker(&io, NONCE, &"c".repeat(40)).expect("write");
        let marker = read_agent_done_marker(&io, NONCE)
            .expect("valid")
            .expect("present");
        assert_eq!(marker.expected_oid, "c".repeat(40));
        assert!(
            matches!(
                read_agent_done_marker(&io, "other-nonce"),
                Err(OrchestratorError::Tampered(_))
            ),
            "wrong nonce is tamper evidence"
        );
        assert!(
            write_agent_done_marker(&io, NONCE, &"c".repeat(40)).is_err(),
            "no overwrite"
        );

        fs::remove_file(io.join(AGENT_DONE_MARKER)).expect("rm");
        fs::write(io.join(AGENT_DONE_MARKER), "not json").expect("plant");
        assert!(matches!(
            read_agent_done_marker(&io, NONCE),
            Err(OrchestratorError::Tampered(_))
        ));
        fs::remove_file(io.join(AGENT_DONE_MARKER)).expect("rm");
        write_agent_done_marker(&io, NONCE, "short").expect("write");
        assert!(matches!(
            read_agent_done_marker(&io, NONCE),
            Err(OrchestratorError::Tampered(_))
        ));
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
        assert!(wait_for_file(
            &late,
            Duration::from_secs(5),
            Duration::from_millis(5)
        ));
        writer.join().expect("writer");
        let started = std::time::Instant::now();
        assert!(!wait_for_file(
            &root.join("io").join("never"),
            Duration::from_millis(120),
            Duration::from_millis(10)
        ));
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(120) && waited < Duration::from_secs(2),
            "{waited:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    // §7(b): a long-lived token that would outlive the relay cap is refused at mint time.
    #[test]
    fn long_lived_expiration_refuses_over_cap_and_covers_the_margin() {
        let now = 1_000_000;
        // Deadline one hour out: lifetime = 3600 + margin, under a 6 h cap.
        let expiry = long_lived_expiration(now + 3_600, now, 21_600).expect("under cap");
        assert_eq!(
            expiry,
            (now + 3_600 + PUSH_MARGIN_SECS) as i64,
            "expiry = deadline + margin"
        );
        // Deadline exactly at cap minus margin fits; one second more does not.
        long_lived_expiration(now + 21_600 - PUSH_MARGIN_SECS, now, 21_600).expect("fits exactly");
        let err = long_lived_expiration(now + 21_600 - PUSH_MARGIN_SECS + 1, now, 21_600)
            .expect_err("over cap");
        assert!(
            matches!(err, OrchestratorError::TokenUnavailable(_)),
            "{err}"
        );
        let message = err.to_string();
        assert!(message.contains("relay cap of 21600s"), "{message}");
        assert!(
            message.contains("fresh-after-agent"),
            "names the way out: {message}"
        );
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
        for poison in [
            "MAXPLAYER_PUSH_TOKEN",
            "MAXPLAYER_JOB_HASH",
            "MAXPLAYER_INPUTS",
        ] {
            assert!(
                !keys.contains(&poison),
                "{poison} must not reach the agent: {keys:?}"
            );
        }
        let values = env
            .iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!values.contains("Nostr secret") && !values.contains(JOB_HASH));
        // The delivery identity wins over a same-named host value.
        let author = env
            .iter()
            .find(|(k, _)| k == "GIT_AUTHOR_NAME")
            .expect("identity env");
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
        assert_eq!(
            classify(OrchestratorError::Agent("x".into())),
            Phase1Status::AgentFailed
        );
        assert_eq!(
            classify(OrchestratorError::DeadlineExceeded),
            Phase1Status::DeadlineExceeded
        );
        assert_eq!(
            classify(OrchestratorError::AgentUnavailable("x".into())),
            Phase1Status::AgentUnavailable
        );
        assert_eq!(
            classify(OrchestratorError::Gate(
                SellerGitError::NoExecutionObserved("x".into())
            )),
            Phase1Status::NoSentinel
        );
        assert_eq!(
            classify(OrchestratorError::Gate(SellerGitError::Io("x".into()))),
            Phase1Status::SnapshotFailed
        );
        assert_eq!(
            classify(OrchestratorError::TokenUnavailable("x".into())),
            Phase1Status::TokenUnavailable
        );
        assert_eq!(
            classify(OrchestratorError::PushExhausted {
                attempts: 1,
                last: "x".into()
            }),
            Phase1Status::PushFailed
        );
        assert_eq!(
            classify(OrchestratorError::Tampered("x".into())),
            Phase1Status::Tampered
        );
        assert_eq!(
            classify(OrchestratorError::Io("x".into())),
            Phase1Status::Aborted
        );
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
        assert!(
            !dir.join(PUSH_TOKEN_FILE).exists(),
            "stale contents are gone"
        );
        let mode = fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        write_secret_file(&dir.join(PUSH_TOKEN_FILE), "Nostr x").expect("token");
        let mode = fs::metadata(dir.join(PUSH_TOKEN_FILE))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the token file is owner-only from its first byte"
        );
        assert!(
            !dir.join(format!("{PUSH_TOKEN_FILE}.tmp")).exists(),
            "no temp file left behind"
        );
        let _ = fs::remove_dir_all(&root);
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
        fs::write(
            root.join(DELIVERY_OID_FILE),
            format!("{}\n", "a".repeat(40)),
        )
        .expect("write");
        assert_eq!(read_delivery_oid(&root).expect("valid"), "a".repeat(40));
        let _ = fs::remove_dir_all(&root);
    }

    // ─── F2: the host reads job-writable exchange files through one hardened reader ──────────────

    /// Plant a FIFO with no writer at `path`. A following, blocking reader parks on it forever.
    #[cfg(unix)]
    fn plant_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL in path");
        // SAFETY: `mkfifo` reads a NUL-terminated path and a mode, and touches no memory of ours.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            rc,
            0,
            "mkfifo {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }

    /// The three files the host reads: each with its reader and a VALID content for it, so a
    /// symlink test can point at a valid target and prove the reader refused the LINK, not the
    /// content behind it.
    #[cfg(unix)]
    fn host_readers() -> Vec<(&'static str, fn(&Path) -> Result<(), OrchestratorError>, String)> {
        let valid_marker = serde_json::to_string(&AgentDoneMarker {
            nonce: NONCE.to_owned(),
            expected_oid: "c".repeat(40),
        })
        .expect("encode marker");
        let valid_outcome = serde_json::to_string(&Phase1Outcome {
            status: Phase1Status::Delivered,
            detail: String::new(),
            delivery_oid: Some("c".repeat(40)),
            agent: None,
            wall_time_ms: 1,
        })
        .expect("encode outcome");
        vec![
            (
                AGENT_DONE_MARKER,
                |io| read_agent_done_marker(io, NONCE).map(|_| ()),
                valid_marker,
            ),
            (
                DELIVERY_OID_FILE,
                |io| read_delivery_oid(io).map(|_| ()),
                format!("{}\n", "c".repeat(40)),
            ),
            (OUTCOME_FILE, |io| read_outcome(io).map(|_| ()), valid_outcome),
        ]
    }

    // Attack A on the exchange channel: while it is alive, the job plants a FIFO with no writer
    // under the marker's name. A following, blocking read parks the host's poll loop on it forever,
    // past every deadline check. The hardened reader opens non-blocking, sees a FIFO on the OPENED
    // descriptor, and refuses at once. The oid and outcome readers get the same attack.
    #[cfg(unix)]
    #[test]
    fn readers_refuse_a_planted_fifo_without_blocking() {
        let root = fresh_root("fifo");
        let io = root.join("io");
        for (name, read, _) in host_readers() {
            plant_fifo(&io.join(name));
            let started = std::time::Instant::now();
            let err = read(&io).expect_err("a FIFO is never read");
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_secs(1),
                "{name}: the read took {elapsed:?}"
            );
            assert!(matches!(err, OrchestratorError::Tampered(_)), "{name}: {err}");
            assert!(
                err.to_string().contains("not a regular file"),
                "{name}: {err}"
            );
            fs::remove_file(io.join(name)).expect("rm fifo");
        }
        let _ = fs::remove_dir_all(&root);
    }

    // Attack B: the job plants a symlink under the marker's name. The target here is a VALID file
    // of the right shape, so a reader that followed the link would return Ok — the refusal proves
    // the link itself was refused, whatever it points at (`/dev/zero` in the real attack, which a
    // following reader consumes until allocation fails). The same content in a regular file reads.
    #[cfg(unix)]
    #[test]
    fn readers_refuse_a_planted_symlink_without_following_it() {
        let root = fresh_root("symlink");
        let io = root.join("io");
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("mkdir");
        for (name, read, valid) in host_readers() {
            let target = elsewhere.join(name);
            fs::write(&target, &valid).expect("write target");
            std::os::unix::fs::symlink(&target, io.join(name)).expect("symlink");
            let started = std::time::Instant::now();
            let err = read(&io).expect_err("a symlink is never followed");
            assert!(started.elapsed() < Duration::from_secs(1), "{name}");
            assert!(matches!(err, OrchestratorError::Tampered(_)), "{name}: {err}");
            assert!(err.to_string().contains("symlink"), "{name}: {err}");
            fs::remove_file(io.join(name)).expect("rm link");
        }
        for (name, read, valid) in host_readers() {
            fs::write(io.join(name), &valid).expect("write regular");
            read(&io).unwrap_or_else(|err| panic!("{name}: a valid regular file reads: {err}"));
            fs::remove_file(io.join(name)).expect("rm");
        }
        let _ = fs::remove_dir_all(&root);
    }

    // A regular file over its cap is refused before it is parsed, so the host never allocates more
    // than the cap for a file the job wrote. One cap per file.
    #[cfg(unix)]
    #[test]
    fn readers_refuse_an_oversized_regular_file() {
        let root = fresh_root("oversize");
        let io = root.join("io");
        let caps = [
            (AGENT_DONE_MARKER, EXCHANGE_MARKER_MAX_BYTES),
            (DELIVERY_OID_FILE, EXCHANGE_OID_MAX_BYTES),
            (OUTCOME_FILE, EXCHANGE_OUTCOME_MAX_BYTES),
        ];
        for (name, read, _) in host_readers() {
            let cap = caps.iter().find(|(n, _)| *n == name).expect("a cap per file").1;
            fs::write(io.join(name), "a".repeat(cap + 1)).expect("write");
            let err = read(&io).expect_err("over the cap");
            assert!(matches!(err, OrchestratorError::Tampered(_)), "{name}: {err}");
            assert!(err.to_string().contains("exceeds"), "{name}: {err}");
            fs::remove_file(io.join(name)).expect("rm");
        }
        let _ = fs::remove_dir_all(&root);
    }

    // The reader's own contract: absent is `None`, exactly the cap reads, one byte more is refused,
    // and a directory under the name is not a regular file.
    #[cfg(unix)]
    #[test]
    fn exchange_reader_caps_at_exactly_max_bytes() {
        let root = fresh_root("reader");
        let io = root.join("io");
        assert_eq!(read_exchange_file(&io, "f", 8).expect("absent"), None);
        fs::write(io.join("f"), [b'x'; 8]).expect("write");
        assert_eq!(
            read_exchange_file(&io, "f", 8)
                .expect("fits")
                .expect("present")
                .len(),
            8
        );
        fs::write(io.join("f"), [b'x'; 9]).expect("write");
        assert!(matches!(
            read_exchange_file(&io, "f", 8),
            Err(OrchestratorError::Tampered(_))
        ));
        fs::remove_file(io.join("f")).expect("rm");
        fs::create_dir(io.join("f")).expect("mkdir");
        let err = read_exchange_file(&io, "f", 8).expect_err("a directory is refused");
        assert!(matches!(err, OrchestratorError::Tampered(_)), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    // The host's token write never follows a link the job planted: a symlink at the token's name is
    // a refusal, and so is one at the temp name the write goes through. Nothing is written to the
    // link target, and no token file appears.
    #[cfg(unix)]
    #[test]
    fn token_write_refuses_a_planted_symlink_at_either_name() {
        let root = fresh_root("token-link");
        let io = root.join("io");
        let victim = root.join("victim");
        fs::write(&victim, "untouched").expect("write victim");
        for planted in [PUSH_TOKEN_FILE.to_owned(), format!("{PUSH_TOKEN_FILE}.tmp")] {
            std::os::unix::fs::symlink(&victim, io.join(&planted)).expect("symlink");
            let err = write_secret_file(&io.join(PUSH_TOKEN_FILE), "Nostr secret")
                .expect_err("a planted link refuses the write");
            assert!(matches!(err, OrchestratorError::Io(_)), "{planted}: {err}");
            assert_eq!(
                fs::read_to_string(&victim).expect("read victim"),
                "untouched",
                "{planted}: the link target must not be written through"
            );
            let at_name = io.join(PUSH_TOKEN_FILE).symlink_metadata();
            assert!(
                at_name.map(|m| m.file_type().is_symlink()).unwrap_or(true),
                "{planted}: no token file may appear at the token's name"
            );
            // The writer's own cleanup may already have unlinked the link at the temp name (an
            // `unlink`, which never follows it); the target above is the proof that matters.
            let _ = fs::remove_file(io.join(&planted));
        }
        let _ = fs::remove_dir_all(&root);
    }

    // The outcome's free text is bounded at the write, on a char boundary, so an agent that says
    // too much cannot make its own honest outcome exceed the host's read cap.
    #[test]
    fn outcome_text_is_bounded_so_the_file_fits_the_read_cap() {
        let root = fresh_root("outcome-bound");
        let io = root.join("io");
        let agent = Some(AgentOutcome {
            usage: None,
            last_agent_message: Some("é".repeat(EXCHANGE_OUTCOME_MAX_BYTES)),
        });
        let failed: Result<Phase1Output, OrchestratorError> =
            Err(OrchestratorError::Agent("x".repeat(EXCHANGE_OUTCOME_MAX_BYTES)));
        let outcome = Phase1Outcome::from_result(&failed, agent, Duration::from_secs(1));
        assert!(outcome.detail.len() <= OUTCOME_TEXT_MAX_BYTES);
        let message = outcome
            .agent
            .as_ref()
            .and_then(|a| a.last_agent_message.as_deref())
            .expect("the message is kept, shortened");
        assert!(message.len() <= OUTCOME_TEXT_MAX_BYTES);
        assert!(!message.is_empty());
        assert!(message.chars().all(|c| c == 'é'), "cut on a char boundary");
        write_outcome(&io, &outcome).expect("write");
        assert_eq!(
            read_outcome(&io).expect("under the cap").as_ref(),
            Some(&outcome)
        );
        let _ = fs::remove_dir_all(&root);
    }

    // ─── F3: the reap is mandatory, and the gate that protects a developer is not a file ───────────

    // Without the host's environment variable — absent, or set to anything but its value — the
    // entry refuses before it reads the inputs, deletes them, or starts the agent. A developer who
    // runs `__deliver phase1` on a host by hand hits this, and their processes are never reaped.
    #[test]
    fn entry_refuses_outside_the_delivery_container_before_touching_anything() {
        let root = fresh_root("entry-gate");
        let inputs = inputs_for(&root, PushTokenSource::None);
        let inputs_path = root.join("io").join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");
        let absent: fn(&str) -> Option<String> = |_| None;
        let zero: fn(&str) -> Option<String> =
            |key| (key == CONTAINER_DELIVERY_ENV).then(|| "0".to_owned());
        let word: fn(&str) -> Option<String> =
            |key| (key == CONTAINER_DELIVERY_ENV).then(|| "true".to_owned());
        for env in [absent, zero, word] {
            let agent_ran = RefCell::new(false);
            let reaped = RefCell::new(false);
            let err = run_phase1_entry_with(
                &inputs_path,
                env,
                |_, _| {
                    *agent_ran.borrow_mut() = true;
                    Ok(AgentOutcome::default())
                },
                || {
                    *reaped.borrow_mut() = true;
                    Ok(())
                },
            )
            .expect_err("refused outside the container");
            assert!(
                matches!(err, OrchestratorError::AgentUnavailable(_)),
                "{err}"
            );
            assert!(err.to_string().contains(CONTAINER_DELIVERY_ENV), "{err}");
            assert!(!*agent_ran.borrow(), "the agent must not start");
            assert!(!*reaped.borrow(), "nothing is reaped outside the container");
            assert!(inputs_path.exists(), "the inputs are not even read");
            assert!(
                !root.join("io").join(OUTCOME_FILE).exists(),
                "no outcome is written"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    // The reap runs after the gate and BEFORE the marker exists, and a reap that fails — `/proc`
    // unreadable, or a survivor that will not die — fails the delivery closed: no marker, so no
    // token is ever minted, no push, and the outcome says why.
    #[test]
    fn entry_fails_closed_when_the_reap_cannot_prove_the_container_empty() {
        let root = fresh_root("entry-reap");
        let inputs = inputs_for(&root, PushTokenSource::FreshAfterAgent { wait_secs: 10 });
        let inputs_path = root.join("io").join(PHASE1_INPUTS_FILE);
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");
        let io = root.join("io");
        let marker_seen_at_reap = RefCell::new(None);
        let started = std::time::Instant::now();
        let err = run_phase1_entry_with(
            &inputs_path,
            in_container,
            |_, workdir| {
                fs::write(workdir.join("answer.txt"), "output\n")
                    .map_err(|e| OrchestratorError::Io(e.to_string()))?;
                Ok(AgentOutcome::default())
            },
            || {
                *marker_seen_at_reap.borrow_mut() = Some(io.join(AGENT_DONE_MARKER).exists());
                Err(OrchestratorError::Tampered(
                    "1 process(es) survived the agent (pids [42]); refusing to push".into(),
                ))
            },
        )
        .expect_err("a failed reap fails the delivery");
        assert!(matches!(err, OrchestratorError::Tampered(_)), "{err}");
        assert_eq!(
            marker_seen_at_reap.into_inner(),
            Some(false),
            "the reap runs before the marker is written"
        );
        assert!(
            !io.join(AGENT_DONE_MARKER).exists(),
            "no marker invites a token after a failed reap"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no token wait happened"
        );
        let outcome = read_outcome(&io).expect("parses").expect("written");
        assert_eq!(outcome.status, Phase1Status::Tampered);
        let _ = fs::remove_dir_all(&root);
    }

    // The reap logic over an injected process table: empty ⇒ Ok with nothing killed; a listing that
    // fails ⇒ that error (fail closed: nothing proves the container empty); survivors are killed and
    // re-listed until gone; a survivor that will not die is tamper evidence after a bounded number
    // of rounds.
    #[test]
    fn reap_kills_and_relists_and_fails_closed_when_it_cannot_prove_the_container_empty() {
        let killed = RefCell::new(Vec::new());
        reap_with(|| Ok(vec![]), |pid| killed.borrow_mut().push(pid), |_| {})
            .expect("an empty container is fine");
        assert!(killed.borrow().is_empty());

        let err = reap_with(
            || Err(OrchestratorError::Io("list /proc: denied".into())),
            |pid| killed.borrow_mut().push(pid),
            |_| {},
        )
        .expect_err("an unlistable process table fails closed");
        assert!(matches!(err, OrchestratorError::Io(_)), "{err}");
        assert!(killed.borrow().is_empty(), "nothing is killed blind");

        let listings = RefCell::new(vec![vec![], vec![42, 43]]);
        let slept = RefCell::new(0u32);
        reap_with(
            || Ok(listings.borrow_mut().pop().unwrap_or_default()),
            |pid| killed.borrow_mut().push(pid),
            |_| *slept.borrow_mut() += 1,
        )
        .expect("the survivors died");
        assert_eq!(*killed.borrow(), vec![42, 43]);
        assert_eq!(*slept.borrow(), 1);

        killed.borrow_mut().clear();
        let err = reap_with(|| Ok(vec![7]), |pid| killed.borrow_mut().push(pid), |_| {})
            .expect_err("an immortal survivor is tamper evidence");
        assert!(matches!(err, OrchestratorError::Tampered(_)), "{err}");
        assert!(err.to_string().contains("[7]"), "{err}");
        assert_eq!(killed.borrow().len(), 20, "one kill per round, bounded");
    }

    // Off Linux the production reap has no process table to prove the container empty with, so it
    // fails closed rather than silently skipping. RED ON REVERT: the old stub returned `Ok(())`.
    #[cfg(all(feature = "wallet", not(target_os = "linux")))]
    #[test]
    fn production_reap_fails_closed_off_linux() {
        let err = reap_other_processes().expect_err("no /proc here");
        assert!(matches!(err, OrchestratorError::Io(_)), "{err}");
        assert!(err.to_string().contains("unavailable"), "{err}");
    }

    // No file on the shared filesystem decides the reap. The job can write that filesystem, and a
    // root job can unlink any sentinel on it, so the code must not name one. RED ON REVERT: the reap
    // once returned early when `/.dockerenv` was absent.
    #[test]
    fn the_reap_consults_no_sentinel_file() {
        let source = include_str!("delivery_orchestrator.rs");
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        let sentinel = concat!("docker", "env");
        assert!(
            !code.contains(sentinel),
            "the reap must not be gated on a file a job could unlink"
        );
    }

    /// The environment variable that turns a re-executed copy of this test binary into the
    /// zombie-leader helper. See [`zombie_leader_helper_entry`].
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    const ZOMBIE_HELPER_ENV: &str = "MAXPLAYER_TEST_ZOMBIE_HELPER";

    /// The libtest name of [`zombie_leader_helper_entry`] in this binary.
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    const ZOMBIE_HELPER_TEST: &str = "delivery_orchestrator::tests::zombie_leader_helper_entry";

    // NOT a test of anything. This is the body of the helper process that
    // `a_zombie_leader_with_a_live_sibling_thread_is_reported_live` re-executes this binary into.
    // Without the environment variable it returns at once and passes, so an ordinary run is not
    // disturbed. With it, it never returns; the parent test kills the process.
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    #[test]
    fn zombie_leader_helper_entry() {
        if std::env::var(ZOMBIE_HELPER_ENV).as_deref() != Ok("1") {
            return;
        }
        become_zombie_leader_with_live_sibling();
    }

    /// Make this process a thread group whose LEADER is a zombie while another thread lives: the
    /// shape a job can leave behind (a detached helper whose initial thread calls `pthread_exit`).
    ///
    /// libtest runs every test body on a spawned thread, so the initial thread of the process — the
    /// group leader — is libtest's, parked on a channel. This thread installs a `SIGUSR1` handler
    /// that exits only the thread it runs on (`exit`, not `exit_group`), and sends the signal to the
    /// leader with `tgkill`. The kernel keeps the leader as a zombie because the group is not empty,
    /// so `/proc/<pid>/status` reads `Z` while this thread and a worker thread sleep. Never returns.
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    fn become_zombie_leader_with_live_sibling() -> ! {
        extern "C" fn exit_this_thread_only(_signal: libc::c_int) {
            // SAFETY: a raw `exit` syscall ends only the calling thread and touches no memory of
            // ours. It is async-signal-safe.
            unsafe {
                libc::syscall(libc::SYS_exit, 0);
            }
        }
        // A second live thread besides this one, so "a sibling lives" does not rest on libtest.
        std::thread::spawn(|| std::thread::sleep(Duration::from_secs(120)));
        // SAFETY: plain libc calls with valid arguments. The handler has the signature `signal`
        // expects, and `tgkill` targets the leader of this process (tid == tgid).
        unsafe {
            let handler: extern "C" fn(libc::c_int) = exit_this_thread_only;
            let handler = handler as libc::sighandler_t;
            assert_ne!(libc::signal(libc::SIGUSR1, handler), libc::SIG_ERR);
            let tgid = libc::getpid();
            assert_eq!(
                libc::syscall(libc::SYS_tgkill, tgid, tgid, libc::SIGUSR1),
                0
            );
        }
        loop {
            std::thread::sleep(Duration::from_secs(120));
        }
    }

    /// The `State:` character of one `/proc/…/status` file, read by the test itself and not by the
    /// code under test. `None` when the file is gone or has no `State:` line.
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    fn observed_state(status_path: &str) -> Option<char> {
        std::fs::read_to_string(status_path)
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .and_then(|rest| rest.trim().chars().next())
    }

    /// `(tid, state)` of every task of `tgid`, read by the test itself.
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    fn observed_tasks(tgid: u32) -> Vec<(u32, char)> {
        let Ok(entries) = std::fs::read_dir(format!("/proc/{tgid}/task")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|tid| {
                let state = observed_state(&format!("/proc/{tgid}/task/{tid}/status"))?;
                Some((tid, state))
            })
            .collect()
    }

    /// The helper process of the zombie-leader test: a re-execution of this test binary. Killed and
    /// reaped on drop, so a failed assertion leaves nothing behind.
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    struct ZombieHelper(Option<std::process::Child>);

    #[cfg(all(feature = "wallet", target_os = "linux"))]
    impl ZombieHelper {
        fn spawn() -> Self {
            let exe = std::env::current_exe().expect("this test binary has a path");
            let child = std::process::Command::new(exe)
                .args([ZOMBIE_HELPER_TEST, "--exact", "--test-threads=1"])
                .env(ZOMBIE_HELPER_ENV, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("re-execute this test binary as the helper");
            Self(Some(child))
        }

        fn tgid(&self) -> u32 {
            self.0.as_ref().expect("the helper is live").id()
        }

        /// Wait until the leader is `Z` and another task is not `Z`/`X`. The test reads both facts
        /// itself. Returns the observed leader state and task list. Panics with the helper's output
        /// when the scenario does not appear in time.
        fn await_zombie_leader_with_live_sibling(
            &mut self,
            timeout: Duration,
        ) -> (char, Vec<(u32, char)>) {
            let tgid = self.tgid();
            let started = std::time::Instant::now();
            loop {
                let leader = observed_state(&format!("/proc/{tgid}/status"));
                let tasks = observed_tasks(tgid);
                let sibling_lives = tasks
                    .iter()
                    .any(|(tid, state)| *tid != tgid && !matches!(state, 'Z' | 'X'));
                if leader == Some('Z') && sibling_lives {
                    return ('Z', tasks);
                }
                if started.elapsed() > timeout {
                    let output = self.kill_and_reap();
                    panic!(
                        "the helper did not become a zombie leader with a live sibling within \
                         {timeout:?}: leader {leader:?}, tasks {tasks:?}; helper stdout {:?}, \
                         stderr {:?}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// SIGKILL the whole group and reap it.
        fn kill_and_reap(&mut self) -> std::process::Output {
            let mut child = self.0.take().expect("the helper is live");
            let _ = child.kill();
            child.wait_with_output().expect("reap the helper")
        }
    }

    #[cfg(all(feature = "wallet", target_os = "linux"))]
    impl Drop for ZombieHelper {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    // The attack from the security re-review, reproduced: a thread group whose leader exited while
    // a sibling thread lives. Linux lists the group in `/proc` under its tgid and reports the
    // LEADER's state, `Z`, in `/proc/<tgid>/status`. A leader-only reading skips the group, so the
    // live thread survives the reap and can read the token as the orchestrator's uid. The production
    // enumerator must report the group live. Read-only: it kills nothing but its own helper.
    // RED ON REVERT to the leader-only `State:` test: the tgid is absent from the list.
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    #[test]
    fn a_zombie_leader_with_a_live_sibling_thread_is_reported_live() {
        let mut helper = ZombieHelper::spawn();
        let tgid = helper.tgid();
        let (leader, tasks) = helper.await_zombie_leader_with_live_sibling(Duration::from_secs(5));
        // The scenario is real before anything is asserted about the code under test.
        assert_eq!(leader, 'Z', "the helper's leader is a zombie");
        assert!(
            tasks
                .iter()
                .any(|(tid, state)| *tid != tgid && !matches!(state, 'Z' | 'X')),
            "a sibling thread of the helper lives: {tasks:?}"
        );

        let live = other_live_pids().expect("the process table of this host is readable");
        assert!(
            live.contains(&tgid),
            "the helper's thread group {tgid} is absent from other_live_pids(): \
             /proc/{tgid}/status State: {leader}, tasks {tasks:?}; a leader-only reading hides a \
             live thread"
        );

        helper.kill_and_reap();
        assert!(
            !Path::new(&format!("/proc/{tgid}")).exists(),
            "SIGKILL to the tgid reached every thread, and the parent reaped the group"
        );
    }

    // The edges of the task classifier, on this host: a group that is gone is dead, a status file
    // that is gone is a task that is gone, a status file with no `State:` line is an error and not
    // a guess, and this process is live.
    #[cfg(all(feature = "wallet", target_os = "linux"))]
    #[test]
    fn the_task_classifier_fails_closed_on_an_unreadable_state() {
        // No pid is this large (`pid_max` tops out at 2^22), so the group is gone.
        assert!(!thread_group_is_live(u32::MAX).expect("a gone group is dead"));
        assert_eq!(
            task_state("/proc/4294967295/task/4294967295/status").expect("a gone task"),
            None
        );
        let root = fresh_root("task-state");
        let path = root.join("status");
        fs::write(&path, "Name:\tx\nPid:\t1\n").expect("write");
        let err = task_state(path.to_str().expect("utf-8")).expect_err("no State: line");
        assert!(matches!(err, OrchestratorError::Io(_)), "{err}");
        assert!(err.to_string().contains("no State: line"), "{err}");
        // The calling thread of this process runs, so its group is live.
        assert!(thread_group_is_live(std::process::id()).expect("readable"));
        let _ = fs::remove_dir_all(&root);
    }
}
