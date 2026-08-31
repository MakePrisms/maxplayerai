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

/// Everything the phase-1 container orchestrator needs, handed to it through a file the host writes
/// and the orchestrator reads. The orchestrator DELETES this file before it starts the agent, and
/// `job_hash` then lives only in the orchestrator's memory — the agent runs as a child and cannot
/// ptrace its parent, so a same-uid agent cannot recover it (B-2). `job_hash` is never placed in the
/// agent's env or argv.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Phase1Inputs {
    pub job_hash: String,
    pub seller_pubkey_hex: String,
    pub base: Option<Phase1BaseOwned>,
    pub delivery_branch: String,
    pub message: String,
    pub author_date_unix: i64,
    /// The ACP harness command spawned as the orchestrator's child (inherits the container stdin/
    /// stdout so the host keeps driving ACP). Carries NO secret.
    pub agent_argv: Vec<String>,
    pub workdir: PathBuf,
    pub out_dir: PathBuf,
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

/// Serialise `inputs` to `path` as JSON (the host side of the input channel). The caller restricts
/// the file's permissions to the orchestrator before the agent can run.
pub fn write_phase1_inputs(path: &Path, inputs: &Phase1Inputs) -> Result<(), OrchestratorError> {
    let json = serde_json::to_string(inputs)
        .map_err(|error| OrchestratorError::Io(format!("encode phase1 inputs: {error}")))?;
    std::fs::write(path, json)
        .map_err(|error| OrchestratorError::Io(format!("write phase1 inputs: {error}")))
}

/// Serialise `inputs` to `path` as JSON (phase 2).
pub fn write_phase2_inputs(path: &Path, inputs: &Phase2Inputs) -> Result<(), OrchestratorError> {
    let json = serde_json::to_string(inputs)
        .map_err(|error| OrchestratorError::Io(format!("encode phase2 inputs: {error}")))?;
    std::fs::write(path, json)
        .map_err(|error| OrchestratorError::Io(format!("write phase2 inputs: {error}")))
}

/// Spawn the ACP agent as a child that INHERITS the container's stdin/stdout — so the host keeps
/// driving the ACP session over `docker run -i` exactly as before — with stderr inherited for the
/// agent's own logs, at `workdir`. Blocks until the agent exits. NO `job_hash` or token is placed in
/// the agent's env or argv.
///
/// A non-zero exit is NOT by itself fatal: the completion gate decides delivery by the delivered
/// TREE, not the exit code (a quota-dead agent exits `0` having written nothing; a crashed agent may
/// exit non-zero after doing real work). The exit is logged to stderr and the gate rules.
fn spawn_agent_child(agent_argv: &[String], workdir: &Path) -> Result<(), OrchestratorError> {
    let (program, args) = agent_argv
        .split_first()
        .ok_or_else(|| OrchestratorError::Io("agent argv is empty".to_owned()))?;
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(workdir)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|error| OrchestratorError::Io(format!("spawn agent {program}: {error}")))?;
    if !status.success() {
        eprintln!("sandbox orchestrator: agent exited with {status}; the gate decides by the tree");
    }
    Ok(())
}

/// Phase-1 container entrypoint. Reads `inputs_path`, DELETES it immediately (so `job_hash` no longer
/// sits on a disk the agent shares — B-2), then runs [`run_phase1`] with the real agent child and
/// writes the OID file the host reads. `job_hash` exists only in this process's memory during the
/// agent run.
pub fn run_phase1_entry(inputs_path: &Path) -> Result<Phase1Output, OrchestratorError> {
    let inputs = read_json::<Phase1Inputs>(inputs_path)?;
    // Delete the sensitive inputs file up front; from here job_hash is only in memory.
    let _ = std::fs::remove_file(inputs_path);

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
        |workdir| spawn_agent_child(&inputs.agent_argv, workdir),
    )?;
    write_delivery_oid(&inputs.out_dir, &output.delivery_oid)?;
    Ok(output)
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
pub fn push_delivery(
    repo_dir: &Path,
    relay_url: &str,
    branch: &str,
    header: Option<String>,
    policy: &PushRetryPolicy,
) -> Result<String, OrchestratorError> {
    seller_git::neutralize_push_config(repo_dir)
        .map_err(|error| OrchestratorError::Io(error.to_string()))?;
    push_with_retry(
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
    )
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
) -> Result<String, OrchestratorError> {
    tokio::task::spawn_blocking(move || {
        push_delivery(&workdir, &remote_url, &branch, header, &policy)
    })
    .await
    .map_err(|error| {
        OrchestratorError::Io(format!("blocking delivery task did not complete: {error}"))
    })?
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

    // The phase-1 inputs round-trip through the on-disk channel unchanged.
    #[test]
    fn phase1_inputs_round_trip() {
        let root = std::env::temp_dir().join(format!("maxplayer-orch-in1-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        let inputs = Phase1Inputs {
            job_hash: JOB_HASH.to_owned(),
            seller_pubkey_hex: "e".repeat(64),
            base: Some(Phase1BaseOwned {
                clone_url: "https://relay.example/git/o/r.git".to_owned(),
                branch: "main".to_owned(),
                oid: "b".repeat(40),
            }),
            delivery_branch: "maxplayer/abc12345".to_owned(),
            message: "delivery".to_owned(),
            author_date_unix: DATE,
            agent_argv: vec!["claude-agent-acp".to_owned()],
            workdir: PathBuf::from("/work"),
            out_dir: PathBuf::from("/out"),
        };
        let path = root.join("in.json");
        write_phase1_inputs(&path, &inputs).expect("write");
        let back: Phase1Inputs = read_json(&path).expect("read");
        assert_eq!(back.job_hash, inputs.job_hash);
        assert_eq!(back.agent_argv, inputs.agent_argv);
        assert_eq!(back.base.expect("base").oid, "b".repeat(40));
        let _ = fs::remove_dir_all(&root);
    }

    // The phase-1 entrypoint runs a real child agent (a shell that writes a deliverable), gates it,
    // commits it, writes the OID file — and DELETES the inputs file before the agent runs (B-2).
    #[test]
    fn run_phase1_entry_runs_child_gates_and_deletes_inputs() {
        let root = std::env::temp_dir().join(format!("maxplayer-orch-entry1-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workdir = root.join("work");
        let out = root.join("out");
        fs::create_dir_all(&out).expect("mkdir out");
        let inputs = Phase1Inputs {
            job_hash: JOB_HASH.to_owned(),
            seller_pubkey_hex: "e".repeat(64),
            base: None, // from-scratch (local-testable; a clone base would need the allowlist)
            delivery_branch: "maxplayer/abc12345".to_owned(),
            message: "delivery".to_owned(),
            author_date_unix: DATE,
            // The "agent": a shell that writes a deliverable into its cwd (the provisioned workdir).
            agent_argv: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf 'entry agent output' > answer.txt".to_owned(),
            ],
            workdir: workdir.clone(),
            out_dir: out.clone(),
        };
        let inputs_path = root.join("in.json");
        write_phase1_inputs(&inputs_path, &inputs).expect("write inputs");

        let output = run_phase1_entry(&inputs_path).expect("phase1 entry");

        assert!(!inputs_path.exists(), "inputs file must be deleted before the agent runs (B-2)");
        assert_eq!(read_delivery_oid(&out).expect("oid file"), output.delivery_oid);
        assert_eq!(output.delivery_repo_dir, workdir, "phase 2 pushes from the workdir");
        // The committed workdir repo holds the reported commit on the delivery branch.
        let repo = git2::Repository::open(&workdir).expect("open workdir");
        assert_eq!(
            repo.refname_to_id("refs/heads/maxplayer/abc12345").expect("tip").to_string(),
            output.delivery_oid,
        );
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
