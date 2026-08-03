//! Agent-run + delivery-shaping helpers, neutral to any single seller lifecycle.
//!
//! Running the awarded agent (ACP driver), composing its delivery-instruction prompt, deriving the
//! delivery discriminator, and shaping the PUBLIC seller-claimed exec-metadata block are the same on
//! the legacy in-memory daemon and the durable node. This module owns them so neither lifecycle
//! depends on the other's error type: the helpers raise a neutral [`ExecError`] that each consumer
//! maps into its own (`DaemonError`, `NodeError`, …) — the same decoupling pattern as
//! [`crate::relay_auth`].

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(feature = "acp")]
use sha2::{Digest, Sha256};

use crate::driver::UsageMetadata;
use crate::gateway::TagSpec;
use crate::home::MobeeHome;
use crate::seller_git::DeliveryAgentIdentity;

/// A neutral agent-run / delivery-shaping failure. Distinct from any consumer's error type so no
/// lifecycle's error leaks here; callers map it into their own (`DaemonError`, `NodeError`, …).
#[derive(Debug, Clone)]
pub enum ExecError {
    /// Misconfiguration surfaced before the run (e.g. empty agent command).
    Config(String),
    /// The agent process failed, timed out, or ended non-terminal.
    Agent(String),
    /// A delivery-shaping policy refusal (e.g. an un-typeable delivery oid).
    Policy(String),
    /// The binary was built without the `acp` feature, so no agent can run.
    AcpRequired,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(message) => write!(f, "seller config: {message}"),
            Self::Agent(message) => write!(f, "seller agent error: {message}"),
            Self::Policy(message) => write!(f, "seller policy: {message}"),
            Self::AcpRequired => write!(
                f,
                "seller agent-run requires rebuilding with the acp feature: \
                 cargo run -p mobee --features acp -- sell run"
            ),
        }
    }
}

impl std::error::Error for ExecError {}

/// How the awarded agent command is launched: either directly (pass-through) or inside a launcher
/// (e.g. `bwrap …`, `systemd-nspawn …`) the command runs under. The wrap is a pure argv transform,
/// so the run/exec path stays launcher-agnostic — the launcher is the only thing a future OS
/// sandbox changes here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// The launcher argv prepended to the agent command. Empty ⇒ pass-through.
    launcher: Vec<String>,
}

impl SandboxPolicy {
    /// A pass-through policy: the agent command runs exactly as configured.
    pub fn passthrough() -> Self {
        Self {
            launcher: Vec::new(),
        }
    }

    /// A policy that runs the agent command inside `launcher` (its argv is prepended). An empty
    /// launcher is a pass-through.
    pub fn wrapped(launcher: Vec<String>) -> Self {
        Self { launcher }
    }

    /// Resolve the policy from the optional `[sandbox]` config: present ⇒ its launcher, absent ⇒
    /// pass-through.
    pub fn from_config(config: Option<&crate::home::SandboxConfig>) -> Self {
        match config {
            Some(config) => Self::wrapped(config.launcher.clone()),
            None => Self::passthrough(),
        }
    }

    /// Whether this policy launches the command directly (no launcher).
    pub fn is_passthrough(&self) -> bool {
        self.launcher.is_empty()
    }

    /// The full argv to spawn: the agent command unchanged under a pass-through policy, otherwise
    /// the launcher argv followed by the agent command.
    pub fn wrap(&self, agent_command: &[String]) -> Vec<String> {
        if self.launcher.is_empty() {
            return agent_command.to_vec();
        }
        let mut argv = Vec::with_capacity(self.launcher.len() + agent_command.len());
        argv.extend_from_slice(&self.launcher);
        argv.extend_from_slice(agent_command);
        argv
    }
}

/// The `(program, args)` the ACP driver actually spawns for `agent_command` under `policy`: wrap
/// the command in the policy's launcher, then split argv0 from the rest. Fails closed when the
/// agent command is empty (a launcher alone is not a runnable command).
///
/// Gated to match its only production caller, `run_acp_job`: without `acp` there is no spawn path
/// to build argv for, and the wrap/refuse behaviour is still covered by the tests below.
#[cfg(any(feature = "acp", test))]
fn launch_argv(
    policy: &SandboxPolicy,
    agent_command: &[String],
) -> Result<(String, Vec<String>), ExecError> {
    if agent_command.is_empty() {
        return Err(ExecError::Config("agent_command empty".into()));
    }
    let mut argv = policy.wrap(agent_command).into_iter();
    let program = argv
        .next()
        .expect("wrap of a non-empty command yields a non-empty argv");
    Ok((program, argv.collect()))
}

/// The per-job working directory under the home (`$MOBEE_HOME/seller-jobs/<job_id>`).
pub fn job_workdir(home: &MobeeHome, job_id: &str) -> PathBuf {
    home.root.join("seller-jobs").join(job_id)
}

/// The ONE coherent job timeout. The ACP driver's idle/response timeout is derived from the job's
/// own deadline (`--job-timeout-secs` → offer deadline → default, via [`crate::seller::job_deadline_unix`])
/// so a job has a single predictable deadline. Saturating: a non-positive remaining window yields
/// `Duration::ZERO`, which fails the run cleanly at the deadline rather than hanging.
pub fn unified_job_timeout(deadline_unix: u64, now_unix: u64) -> Duration {
    Duration::from_secs(deadline_unix.saturating_sub(now_unix))
}

/// Run the agent with bounded retries that stay WITHIN the job deadline.
///
/// A transient agent error is retried until either the attempt budget (`max_attempts`) is spent OR
/// the deadline (`deadline_unix`, checked against injected `now`) passes. The error is surfaced to
/// the caller — which then publishes the feedback-kind error exactly once — ONLY after one of those
/// limits is reached. This stops a transient failure from immediately burning the claim while the
/// deadline still has room. `run` is invoked with the 1-based attempt number and awaited to
/// completion before any retry, so attempts never overlap.
pub async fn run_agent_with_retry<F, Fut>(
    deadline_unix: u64,
    max_attempts: u32,
    now: impl Fn() -> u64,
    mut run: F,
) -> Result<Option<UsageMetadata>, ExecError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<Option<UsageMetadata>, ExecError>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match run(attempt).await {
            Ok(usage) => return Ok(usage),
            // Retry only while BOTH an attempt and the deadline remain; otherwise surface the error
            // so the caller publishes feedback-kind exactly once (past deadline / exhausted).
            Err(_) if attempt < max_attempts && now() < deadline_unix => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Daemon/node-owned delivery. The seller appends explicit, secret-free delivery instructions to the
/// agent's task prompt so the agent delivers by committing its work to the git repository in its
/// working directory — rather than guessing a delivery channel. The seller performs the
/// authenticated push of the committed branch to the bound remote (NIP-98; the agent is never handed
/// a key), so this text carries NO secret — it is public prompt text built only from the task and the
/// (public) remote URL.
pub fn compose_agent_prompt(task: &str, git_remote: &str, memory_section: Option<&str>) -> String {
    let base = format!(
        "{task}\n\n\
         ---\n\
         DELIVERY (required). Your deliverable is the FINAL STATE OF YOUR CURRENT WORKING \
         DIRECTORY:\n\
         - Leave your work as files on disk there. The daemon snapshots that directory into one \
         commit and pushes it to the bound git remote ({git_remote}) on your behalf.\n\
         - You do NOT need to commit or push, and you are NOT handed any credentials. Committing \
         is harmless, but it is the directory CONTENTS that are delivered, not your commits.\n\
         - Files excluded by .gitignore are NOT delivered, so never ignore your own deliverable.\n\
         Anything you only print to the console is not delivered."
    );
    // Read-on-start: when memory is enabled the rendered index section is appended. When `None`
    // (memory_enabled=false, or no non-empty index) the output is byte-IDENTICAL to the
    // memory-disabled prompt (golden invariant).
    match memory_section {
        Some(section) => format!("{base}\n\n{section}"),
        None => base,
    }
}

/// A concise, single-line delivery-commit message derived from the offer task: the first non-empty
/// line, whitespace-collapsed and length-capped. Falls back to a fixed label for an empty task.
pub fn delivery_message(task: &str) -> String {
    let summary: String = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if summary.is_empty() {
        return "mobee delivery".to_owned();
    }
    let capped: String = summary.chars().take(72).collect();
    format!("mobee delivery: {capped}")
}

/// Delivery discriminator for the seller's commit/fork delivery, derived from the SAME typed
/// [`GitDelivery`](crate::delivery::GitDelivery) the buyer's pay path uses — NOT a hardcoded label —
/// so buyer and seller derive it from one abstraction (`"fork"`). Fails closed if the just-pushed
/// fields somehow do not type (impossible on the success path — a git push returns a canonical oid);
/// never silently relabels or emits a bogus kind.
pub fn seller_delivery_kind(
    git_remote: &str,
    branch: &str,
    commit_oid: &str,
) -> Result<crate::receipt::DeliveryKind, ExecError> {
    let delivery = crate::delivery::GitDelivery::new(
        git_remote.to_owned(),
        branch.to_owned(),
        crate::delivery::CommitOid::parse(commit_oid.to_owned())
            .map_err(|error| ExecError::Policy(format!("delivery oid: {error}")))?,
    )
    .map_err(|error| ExecError::Policy(format!("delivery typing: {error}")))?;
    Ok(delivery.delivery_kind())
}

/// Build the seller-claimed PUBLIC usage block for a result-kind result.
///
/// This block is PUBLIC and harness-generic. It is **opportunistic**: emit only fields the seller can
/// source. `harness` is resolved from the configured preset label (else the agent command),
/// `wall_time` is measured, and `metadata_trust=seller-claimed` is required whenever any field is
/// present (anchor rule).
///
/// `usage_transport` is the harness/adapter's declared capture axis (`acp-native` for the codex
/// adapter, `side-channel` otherwise), resolved from the configured harness identity.
///
/// Token / model / cost tags are appended **only where the driver surfaced them** (absent-stays-absent,
/// never zero-filled — a fabricated `0` is worse than a rendered dash). `total` = `input + output +
/// reasoning` (locked rule); cache siblings are evidence and are NEVER summed into `total`. When
/// `usage` is `None` the block is exactly the four base tags — no-capture trades stay honestly dashed.
pub fn seller_exec_metadata(
    agent_command: &[String],
    agent_preset: Option<&str>,
    wall_time_ms: u64,
    usage: Option<&UsageMetadata>,
) -> Vec<TagSpec> {
    let (harness, transport) = harness_and_transport(agent_command, agent_preset);
    let wall = wall_time_ms.to_string();

    let mut tags = vec![
        TagSpec::new(["harness", harness.as_str()]),
        TagSpec::new(["usage_transport", transport]),
        TagSpec::new(["metadata_trust", "seller-claimed"]),
        TagSpec::new(["wall_time", wall.as_str(), "ms"]),
    ];

    if let Some(u) = usage {
        if let Some(model) = &u.model {
            tags.push(TagSpec::new(["model", model.as_str()]));
        }
        // Own the string renders so the borrows outlive each `TagSpec::new` call.
        let total = u.total_tokens().map(|n| n.to_string());
        let input = u.input_tokens.map(|n| n.to_string());
        let output = u.output_tokens.map(|n| n.to_string());
        let reasoning = u.reasoning_tokens.map(|n| n.to_string());
        let cache_read = u.cache_read_tokens.map(|n| n.to_string());
        let cache_write = u.cache_write_tokens.map(|n| n.to_string());
        if let Some(v) = &total {
            tags.push(TagSpec::new(["tokens", v.as_str(), "total"]));
        }
        if let Some(v) = &input {
            tags.push(TagSpec::new(["tokens", v.as_str(), "input"]));
        }
        if let Some(v) = &output {
            tags.push(TagSpec::new(["tokens", v.as_str(), "output"]));
        }
        if let Some(v) = &reasoning {
            tags.push(TagSpec::new(["tokens", v.as_str(), "reasoning"]));
        }
        if let Some(v) = &cache_read {
            tags.push(TagSpec::new(["tokens", v.as_str(), "cache_read"]));
        }
        if let Some(v) = &cache_write {
            tags.push(TagSpec::new(["tokens", v.as_str(), "cache_write"]));
        }
        if let Some(cost) = &u.cost {
            tags.push(TagSpec::new([
                "cost",
                cost.amount.as_str(),
                "usd",
                cost.basis.as_str(),
            ]));
        }
    }

    tags
}

/// Best-effort harness id + usage transport.
///
/// The configured **preset label** (`claude`|`cursor`|`codex`, [`crate::home::SellerConfig::agent`])
/// is the authoritative harness/adapter identity and is preferred over argv inspection: presets
/// launch the ACP adapter via `npx <adapter-package>` (argv0 = `npx`), so an argv0-naive id emitted
/// `npx` — which a downstream harness-family classifier maps to `harness_family="other"`, hiding real
/// claude/codex/cursor jobs. When no preset label is present (raw `--agent-argv` power-user hatch)
/// fall back to scanning the FULL adapter argv (not just argv0): the adapter package name (e.g.
/// `@agentclientprotocol/claude-agent-acp`) still carries the family. Unknown ⇒ the command basename
/// + the conservative `side-channel`.
pub fn harness_and_transport(
    agent_command: &[String],
    agent_preset: Option<&str>,
) -> (String, &'static str) {
    // Preset label is authoritative — resolve from the adapter identity, never argv0. A non-built-in
    // label is a config-defined `[agents]` preset: the preset name IS the harness identity
    // (conservative `side-channel` transport — nothing is known about it).
    if let Some(preset) = agent_preset {
        match preset.trim().to_ascii_lowercase().as_str() {
            "claude" => return ("claude-agent-acp".to_owned(), "side-channel"),
            "codex" => return ("codex-acp-ng".to_owned(), "acp-native"),
            "cursor" => return ("cursor-agent".to_owned(), "side-channel"),
            "" => {}
            _ => return (preset.trim().to_owned(), "side-channel"),
        }
    }
    // Hatch fallback: scan the FULL argv (adapter identity), not just argv0.
    let joined = agent_command.join(" ").to_ascii_lowercase();
    if joined.contains("codex") {
        ("codex-acp-ng".to_owned(), "acp-native")
    } else if joined.contains("cursor") {
        ("cursor-agent".to_owned(), "side-channel")
    } else if joined.contains("claude") {
        ("claude-agent-acp".to_owned(), "side-channel")
    } else {
        let program = agent_command.first().map(String::as_str).unwrap_or("");
        let basename = program.rsplit('/').next().unwrap_or(program);
        let harness = if basename.is_empty() {
            "unknown".to_owned()
        } else {
            basename.to_owned()
        };
        (harness, "side-channel")
    }
}

/// Run the awarded agent under the ACP driver: one session in `workdir`, seeded with `prompt`, with
/// the delivery `identity`'s git env, bounded by `timeout` (the unified job timeout). The agent
/// command is launched through `policy` — directly under a pass-through policy, or inside the
/// policy's launcher. Returns the ACP usage the driver surfaced (`None` when the harness exposed
/// nothing).
#[cfg(feature = "acp")]
pub async fn run_agent_job(
    agent_command: &[String],
    policy: &SandboxPolicy,
    prompt: &str,
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
    timeout: Duration,
) -> Result<Option<UsageMetadata>, ExecError> {
    use crate::driver::{AcpDriver, AgentCommand, ContentBlock, PromptTurn, SessionConfig};
    use crate::engine::{run_job, RunParams};
    use crate::event::JobId;
    use crate::log::EventLog;

    let (program, args) = launch_argv(policy, agent_command)?;
    // The ACP idle/response timeout IS the unified job timeout — never a hardcoded 300s that could
    // override or conflict with `--job-timeout-secs`.
    let mut driver = AcpDriver::new(
        AgentCommand::new(program, args),
        crate::driver::PermissionOutcome::Allow,
        timeout,
    );
    let log_path = workdir.join("seller-run.jsonl");
    let mut log = EventLog::open(&log_path).map_err(|error| ExecError::Agent(error.to_string()))?;
    let params = RunParams {
        session_config: SessionConfig {
            cwd: workdir.to_path_buf(),
            mcp_servers: Vec::new(),
            env: identity.git_env(),
        },
        prompt: PromptTurn {
            input: vec![ContentBlock::Text {
                text: prompt.to_owned(),
            }],
        },
    };
    let outcome = run_job(
        &mut driver,
        &mut log,
        &JobId(format!("seller-{}", short_hash(prompt))),
        params,
        &mut |_| {},
    )
    .await
    .map_err(|error| ExecError::Agent(error.to_string()))?;
    match outcome.terminal {
        crate::event::JobExecutionStatus::Completed => Ok(outcome.usage),
        other => Err(ExecError::Agent(format!("agent terminal {other:?}"))),
    }
}

/// Without the `acp` feature there is no agent runtime — fail closed with the rebuild hint.
#[cfg(not(feature = "acp"))]
pub async fn run_agent_job(
    _agent_command: &[String],
    _policy: &SandboxPolicy,
    _prompt: &str,
    _workdir: &Path,
    _identity: &DeliveryAgentIdentity,
    _timeout: Duration,
) -> Result<Option<UsageMetadata>, ExecError> {
    Err(ExecError::AcpRequired)
}

#[cfg(feature = "acp")]
fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    // The default (pass-through) policy launches the agent command exactly as configured: the
    // spawned `(program, args)` reconstruct the configured argv byte-for-byte, with no launcher.
    #[test]
    fn passthrough_policy_launches_the_configured_command_byte_identical() {
        let agent_command: Vec<String> = ["claude", "--print", "--flag=a b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let policy = SandboxPolicy::passthrough();
        assert!(policy.is_passthrough());
        // The argv `wrap` hands the driver is the configured command, unchanged.
        assert_eq!(policy.wrap(&agent_command), agent_command);
        // Split into what the ACP driver spawns: program = argv0, args = the rest — reconstructing
        // the configured command exactly (byte-identical to before the seam existed).
        let (program, args) = launch_argv(&policy, &agent_command).expect("non-empty command");
        assert_eq!(program, agent_command[0]);
        assert_eq!(args, agent_command[1..]);
        assert_eq!(
            std::iter::once(program).chain(args).collect::<Vec<_>>(),
            agent_command
        );
        // The default policy is the pass-through policy.
        assert_eq!(SandboxPolicy::default(), policy);
    }

    // A launcher policy runs the agent command INSIDE the launcher: argv0 becomes the launcher, the
    // configured command follows unchanged.
    #[test]
    fn launcher_policy_makes_the_launcher_argv0() {
        let agent_command: Vec<String> =
            ["claude", "--print"].iter().map(|s| s.to_string()).collect();
        let launcher: Vec<String> = ["bwrap", "--unshare-all", "--"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let policy = SandboxPolicy::wrapped(launcher.clone());
        assert!(!policy.is_passthrough());
        let (program, args) = launch_argv(&policy, &agent_command).expect("non-empty command");
        assert_eq!(program, launcher[0]);
        // Full spawned argv is launcher then the agent command, in order.
        let spawned: Vec<String> = std::iter::once(program).chain(args).collect();
        let expected: Vec<String> = launcher.iter().chain(agent_command.iter()).cloned().collect();
        assert_eq!(spawned, expected);
    }

    // A launcher with no agent command is a misconfig — a launcher alone is not a runnable command.
    #[test]
    fn empty_agent_command_fails_closed_even_with_a_launcher() {
        let policy = SandboxPolicy::wrapped(vec!["bwrap".into()]);
        let err = launch_argv(&policy, &[]).expect_err("empty command refused");
        assert!(matches!(err, ExecError::Config(_)));
    }

    // The seller-side receipt-preimage delivery discriminator is DERIVED from the typed
    // `GitDelivery` ("fork"), not a hardcoded label — buyer and seller agree by construction.
    #[test]
    fn seller_delivery_kind_derives_fork_from_typed_delivery() {
        let kind = seller_delivery_kind(
            "https://relay.example/git/job.git",
            "mobee/abcd1234",
            &"a".repeat(40),
        )
        .expect("commit delivery types");
        assert_eq!(kind, crate::receipt::DeliveryKind::Fork);
        assert_eq!(kind.as_str(), "fork");
    }

    // The ACP timeout is unified with `--job-timeout-secs` — one deadline.
    #[test]
    fn unified_job_timeout_is_the_remaining_deadline_not_a_hardcoded_constant() {
        // The effective timeout is strictly the remaining window to the job's deadline.
        assert_eq!(unified_job_timeout(1_000, 940), Duration::from_secs(60));
        assert_eq!(unified_job_timeout(1_000, 100), Duration::from_secs(900));
        // Two different deadlines ⇒ two different timeouts — proves it is DERIVED from the
        // deadline, not a fixed 300s that could override or conflict with `--job-timeout-secs`.
        assert_ne!(
            unified_job_timeout(1_000, 940),
            unified_job_timeout(1_000, 100)
        );
        assert_ne!(unified_job_timeout(1_000, 940), Duration::from_secs(300));
        // At/past the deadline ⇒ ZERO (fail cleanly at the deadline, never hang, never wrap).
        assert_eq!(unified_job_timeout(1_000, 1_000), Duration::ZERO);
        assert_eq!(unified_job_timeout(1_000, 5_000), Duration::ZERO);
    }

    // A transient agent error is retried WITHIN the deadline; feedback-kind is published only after
    // the attempt budget or the deadline is spent.
    #[tokio::test]
    async fn retry_recovers_from_a_transient_error_within_the_deadline() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // Deadline far away ⇒ never the limiter; a transient first error must be retried, NOT burn
        // the claim (publish feedback) while the deadline still has room.
        let out = run_agent_with_retry(u64::MAX, 3, || 0, |attempt| {
            attempts.set(attempt);
            async move {
                if attempt < 2 {
                    Err(ExecError::Agent("transient".into()))
                } else {
                    Ok::<Option<UsageMetadata>, ExecError>(None)
                }
            }
        })
        .await;
        assert!(out.is_ok(), "transient error retried within deadline, not fatal: {out:?}");
        assert_eq!(attempts.get(), 2, "retried once, then succeeded");
    }

    #[tokio::test]
    async fn retry_exhausts_bounded_attempts_then_surfaces_the_error() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // Deadline never the limiter (u64::MAX) — only the attempt budget stops the loop.
        let out = run_agent_with_retry(u64::MAX, 3, || 0, |attempt| {
            attempts.set(attempt);
            async move { Err::<Option<UsageMetadata>, ExecError>(ExecError::Agent("always".into())) }
        })
        .await;
        assert!(out.is_err(), "exhausted retries ⇒ error so caller publishes feedback-kind");
        assert_eq!(attempts.get(), 3, "bounded to the attempt budget");
    }

    #[tokio::test]
    async fn retry_past_deadline_makes_one_attempt_then_surfaces_the_error() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        // `now` (5_000) is already past the deadline (1_000) ⇒ no retry budget at all: one attempt,
        // then the error surfaces so the caller publishes feedback-kind.
        let out = run_agent_with_retry(1_000, 3, || 5_000, |attempt| {
            attempts.set(attempt);
            async move { Err::<Option<UsageMetadata>, ExecError>(ExecError::Agent("late".into())) }
        })
        .await;
        assert!(out.is_err(), "past deadline ⇒ error (caller publishes feedback-kind)");
        assert_eq!(attempts.get(), 1, "no retry once the deadline has passed");
    }

    // The seller appends explicit, secret-free delivery instructions.
    #[test]
    fn composed_prompt_carries_task_and_owned_delivery_instructions() {
        let remote = "https://relay.example/git/abc.git";
        let prompt = compose_agent_prompt("build a widget", remote, None);
        // The original task stays up front.
        assert!(prompt.starts_with("build a widget"), "task preserved: {prompt}");
        // Explicit, seller-owned delivery instructions are appended.
        assert!(prompt.contains("DELIVERY"), "has a delivery section: {prompt}");
        assert!(prompt.contains("git"), "delivery is via git: {prompt}");

        // The instructions must describe what `snapshot_delivery_at` ACTUALLY delivers: the final
        // worktree, staged with `add_all`. Telling an agent its commits are the deliverable is
        // false — the snapshot takes `base_oid = None`, so the agent's commits are orphaned by the
        // delivery commit and never pushed.
        assert!(
            prompt.contains("FINAL STATE OF YOUR CURRENT WORKING DIRECTORY"),
            "names the worktree as the deliverable: {prompt}"
        );
        assert!(
            !prompt.contains("Anything not committed to git will not be delivered"),
            "must NOT claim uncommitted work is undelivered — `add_all` delivers it: {prompt}"
        );
        // `add_all` uses IndexAddOption::DEFAULT, which honours .gitignore, so an agent that
        // ignores its own output loses it silently. That has to be said where the agent can read it.
        assert!(
            prompt.contains(".gitignore"),
            "warns that ignored files are not delivered: {prompt}"
        );
        assert!(
            prompt.contains(remote),
            "names the bound remote so delivery is not guessed: {prompt}"
        );
        // Public prompt text — never embeds a secret.
        let lower = prompt.to_lowercase();
        assert!(!prompt.contains("nsec"), "no nostr secret key");
        assert!(!lower.contains("private key"), "no private key");
        assert!(!lower.contains("secret"), "no secret material");
    }

    #[test]
    fn seller_exec_metadata_is_harness_generic_public_and_absent_stays_absent() {
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // claude ⇒ side-channel; codex ⇒ acp-native; unknown ⇒ basename + side-channel. `None`
        // usage: the pre-capture block — token/model/cost stay absent.
        let claude = seller_exec_metadata(&["claude".into(), "--print".into()], None, 1234, None);
        assert_eq!(value(&claude, "harness").as_deref(), Some("claude-agent-acp"));
        assert_eq!(value(&claude, "usage_transport").as_deref(), Some("side-channel"));
        // Anchor rule: metadata_trust present whenever any field is present.
        assert_eq!(value(&claude, "metadata_trust").as_deref(), Some("seller-claimed"));
        assert_eq!(value(&claude, "wall_time").as_deref(), Some("1234"));
        // Absent-stays-absent: no zero-filled token/model/cost fields (not sourced this run).
        assert!(value(&claude, "tokens").is_none());
        assert!(value(&claude, "model").is_none());
        assert!(value(&claude, "cost").is_none());

        let codex = seller_exec_metadata(&["/nix/store/x/bin/codex-acp".into()], None, 5, None);
        assert_eq!(value(&codex, "harness").as_deref(), Some("codex-acp-ng"));
        assert_eq!(value(&codex, "usage_transport").as_deref(), Some("acp-native"));

        let unknown = seller_exec_metadata(&["/opt/tools/mytool".into()], None, 5, None);
        assert_eq!(value(&unknown, "harness").as_deref(), Some("mytool"));
        assert_eq!(value(&unknown, "usage_transport").as_deref(), Some("side-channel"));
    }

    #[test]
    fn claude_preset_resolves_harness_family_claude_despite_npx_argv0() {
        // Mirror the downstream harness-family classifier: a family substring wins;
        // present-but-unrecognized (e.g. "npx") → "other".
        fn harness_family(id: &str) -> &'static str {
            let s = id.to_ascii_lowercase();
            if s.contains("claude") {
                "claude"
            } else if s.contains("cursor") {
                "cursor"
            } else if s.contains("codex") {
                "codex"
            } else {
                "other"
            }
        }
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // The `claude` preset launches the ACP adapter via `npx` (argv0 = "npx"). An argv0-naive id
        // emits "npx" → harness_family "other" (the dashboard bug). The preset label must drive
        // resolution to "claude-agent-acp" → family "claude".
        let npx_claude = vec![
            "/usr/bin/npx".to_string(),
            "-y".to_string(),
            "@agentclientprotocol/claude-agent-acp".to_string(),
        ];
        let tags = seller_exec_metadata(&npx_claude, Some("claude"), 100, None);
        let harness = value(&tags, "harness").expect("harness tag");
        assert_eq!(harness, "claude-agent-acp");
        assert_eq!(
            harness_family(&harness),
            "claude",
            "claude preset must map to harness_family 'claude', not 'other'"
        );

        // Preset label is authoritative even when the argv carries no family hint at all.
        let opaque = vec![
            "/usr/bin/npx".to_string(),
            "-y".to_string(),
            "@acp/opaque-adapter".to_string(),
        ];
        let opaque_tags = seller_exec_metadata(&opaque, Some("claude"), 100, None);
        assert_eq!(
            harness_family(&value(&opaque_tags, "harness").expect("harness")),
            "claude"
        );

        // Regression guard: bare argv0 = "npx" with NO preset label used to yield "other"; the
        // full-argv fallback now recovers "claude" from the adapter package name.
        let hatch = seller_exec_metadata(&npx_claude, None, 100, None);
        assert_eq!(
            harness_family(&value(&hatch, "harness").expect("harness")),
            "claude"
        );
    }

    #[test]
    fn custom_preset_label_is_the_reported_harness_identity() {
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        // A config-defined `[agents]` preset (non-built-in label): the preset name IS the harness id
        // — never argv0, never a family guess from the launch command.
        let argv = vec!["/opt/adapters/grok-acp".to_string(), "stdio".to_string()];
        let tags = seller_exec_metadata(&argv, Some("grok"), 42, None);
        assert_eq!(value(&tags, "harness").as_deref(), Some("grok"));
        assert_eq!(value(&tags, "usage_transport").as_deref(), Some("side-channel"));

        // Built-in labels keep their adapter identities (custom seam must not regress them).
        let builtin = seller_exec_metadata(&argv, Some("codex"), 42, None);
        assert_eq!(value(&builtin, "harness").as_deref(), Some("codex-acp-ng"));
    }

    #[test]
    fn seller_exec_metadata_emits_captured_usage_into_result_tags() {
        use crate::driver::{UsageCost, UsageMetadata};

        // A tag qualified by cell index 1 (value) + cell 2 (qualifier), e.g. ["tokens","140","total"].
        let qualified = |tags: &[TagSpec], name: &str, qualifier: &str| -> Option<String> {
            tags.iter()
                .find(|tag| {
                    tag.first() == Some(name) && tag.0.get(2).map(String::as_str) == Some(qualifier)
                })
                .and_then(|tag| tag.value().map(str::to_owned))
        };
        let value = |tags: &[TagSpec], name: &str| -> Option<String> {
            tags.iter()
                .find(|tag| tag.first() == Some(name))
                .and_then(|tag| tag.value().map(str::to_owned))
        };

        let usage = UsageMetadata {
            model: Some("claude-opus-4-8".into()),
            input_tokens: Some(100),
            output_tokens: Some(40),
            reasoning_tokens: None,
            cache_read_tokens: Some(4096),
            cache_write_tokens: Some(512),
            cost: Some(UsageCost {
                amount: "0.0123".into(),
                basis: "harness-reported-usd".into(),
            }),
        };
        // usage_transport is the harness's declared axis: a claude command is side-channel.
        let tags = seller_exec_metadata(&["claude".into()], None, 4321, Some(&usage));

        assert_eq!(value(&tags, "usage_transport").as_deref(), Some("side-channel"));
        assert_eq!(value(&tags, "model").as_deref(), Some("claude-opus-4-8"));
        // total = input + output (reasoning absent = unknown, not zero); cache NOT folded in.
        assert_eq!(qualified(&tags, "tokens", "total").as_deref(), Some("140"));
        assert_eq!(qualified(&tags, "tokens", "input").as_deref(), Some("100"));
        assert_eq!(qualified(&tags, "tokens", "output").as_deref(), Some("40"));
        assert_eq!(qualified(&tags, "tokens", "reasoning"), None);
        assert_eq!(qualified(&tags, "tokens", "cache_read").as_deref(), Some("4096"));
        assert_eq!(qualified(&tags, "tokens", "cache_write").as_deref(), Some("512"));
        // cost tag: ["cost","<amount>","usd","<basis>"].
        let cost = tags
            .iter()
            .find(|t| t.first() == Some("cost"))
            .expect("cost tag");
        assert_eq!(cost.0, vec!["cost", "0.0123", "usd", "harness-reported-usd"]);

        // Partial capture (output only) → NO total tag (a partial never masquerades as complete).
        let partial = UsageMetadata {
            output_tokens: Some(40),
            ..UsageMetadata::default()
        };
        let partial_tags = seller_exec_metadata(&["claude".into()], None, 1, Some(&partial));
        assert_eq!(qualified(&partial_tags, "tokens", "total"), None);
        assert_eq!(qualified(&partial_tags, "tokens", "output").as_deref(), Some("40"));
    }

    #[test]
    fn delivery_message_summarizes_the_task() {
        assert_eq!(
            delivery_message("Fix the parser\n\nmore detail"),
            "mobee delivery: Fix the parser"
        );
        // Leading blank lines skipped; whitespace collapsed.
        assert_eq!(
            delivery_message("\n\n   add   retry   logic  "),
            "mobee delivery: add retry logic"
        );
        // Empty task falls back to a fixed label.
        assert_eq!(delivery_message("   \n  "), "mobee delivery");
        // Long first line is capped.
        let long = "x".repeat(200);
        let msg = delivery_message(&long);
        assert!(msg.starts_with("mobee delivery: "));
        assert_eq!(msg.len(), "mobee delivery: ".len() + 72);
    }

    #[cfg(not(feature = "acp"))]
    #[tokio::test]
    async fn agent_run_fail_closed_without_acp_feature() {
        let identity = DeliveryAgentIdentity::for_seller(&"aa".repeat(32));
        let err = run_agent_job(
            &["echo".into()],
            &SandboxPolicy::passthrough(),
            "task",
            Path::new("."),
            &identity,
            Duration::from_secs(1),
        )
        .await
        .expect_err("acp required");
        assert!(matches!(err, ExecError::AcpRequired));
        assert!(err.to_string().contains("acp"));
    }
}
