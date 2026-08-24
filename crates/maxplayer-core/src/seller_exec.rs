//! Agent-run + delivery-shaping helpers, neutral to any single seller lifecycle.
//!
//! Running the awarded agent (ACP driver), composing its delivery-instruction prompt, deriving the
//! delivery discriminator, and shaping the PUBLIC seller-claimed exec-metadata block are the same on
//! the legacy in-memory daemon and the durable node. This module owns them so neither lifecycle
//! depends on the other's error type: the helpers raise a neutral [`ExecError`] that each consumer
//! maps into its own (`DaemonError`, `NodeError`, …) — the same decoupling pattern as
//! [`crate::relay_auth`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(feature = "acp")]
use sha2::{Digest, Sha256};

use crate::driver::UsageMetadata;
use crate::gateway::TagSpec;
use crate::home::MaxplayerHome;
use crate::seller_git::DeliveryAgentIdentity;

/// A neutral agent-run / delivery-shaping failure. Distinct from any consumer's error type so no
/// lifecycle's error leaks here; callers map it into their own (`DaemonError`, `NodeError`, …).
#[derive(Debug, Clone)]
pub enum ExecError {
    /// Misconfiguration surfaced before the run (e.g. empty agent command).
    Config(String),
    /// The agent process failed, timed out, or ended non-terminal.
    Agent(String),
    /// The deadline-derived unified job timer expired. This says nothing about harness health.
    DeadlineExceeded,
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
            Self::DeadlineExceeded => {
                write!(f, "job deadline reached while the agent was still running")
            }
            Self::Policy(message) => write!(f, "seller policy: {message}"),
            Self::AcpRequired => write!(
                f,
                "seller agent-run requires rebuilding with the acp feature: \
                 cargo run -p maxplayer --features acp -- seller run"
            ),
        }
    }
}

impl std::error::Error for ExecError {}

/// A capability probe could not be MEASURED — as distinct from a probe that ran and found the binary
/// absent (#784).
///
/// The two must never be conflated. An absent binary is the ordinary "this seat cannot do that" and
/// simply omits the token; an unmeasurable probe means boot has no honest answer for that token and
/// must fail LOUDLY rather than silently publish a shorter capability set. A buyer commits sats on
/// this field, so "we could not check" is not allowed to look like "checked, and no".
#[derive(Debug)]
pub enum ProbeRunError {
    /// The launcher process could not be spawned. Only raised under an executor whose launcher is a
    /// separate program from the probe target (docker): a missing `docker` means the probe never ran.
    /// Under a pass-through policy the probe program IS the target, so its absence is a clean "not
    /// proven", not this error.
    LauncherUnspawnable(std::io::Error),
    /// The probe was still running at its wall-clock deadline and was killed. On the pre-advertise
    /// path an unbounded probe (a stuck `--version`, a docker pull with no registry answer) would
    /// hang the seller before it ever serves, so a timeout is a hard failure, not a "no".
    TimedOut { after: Duration },
    /// Waiting on the probe process itself failed, so its outcome is unknown.
    Wait(std::io::Error),
}

impl std::fmt::Display for ProbeRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LauncherUnspawnable(error) => {
                write!(f, "capability probe launcher could not be spawned: {error}")
            }
            Self::TimedOut { after } => write!(
                f,
                "capability probe exceeded its {}s bound and was killed",
                after.as_secs()
            ),
            Self::Wait(error) => write!(f, "capability probe could not be waited on: {error}"),
        }
    }
}

impl std::error::Error for ProbeRunError {}

/// What supplied the ACP response timer for an agent run.
///
/// Both real jobs and self-probes use the same ACP driver, but only the former inherits its timer
/// from the job deadline. Keeping the source typed lets a real job expiry bypass harness strikes
/// while a probe that cannot answer inside its own health-check limit still fails the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRunTimeout {
    JobDeadline(Duration),
    HarnessProbe(Duration),
}

impl AgentRunTimeout {
    #[cfg(feature = "acp")]
    fn duration(self) -> Duration {
        match self {
            Self::JobDeadline(duration) | Self::HarnessProbe(duration) => duration,
        }
    }
}

/// The in-container mount point for the per-job workdir under `docker` mode. The agent works here
/// (its ACP session cwd), the host workdir is bind-mounted here read-write, and NOTHING ELSE of the
/// host is mounted — so `$MAXPLAYER_HOME` (wallet/keys/journal) is absent from the container by
/// construction.
const CONTAINER_WORKDIR: &str = "/work";

/// How the awarded agent command is launched. Pass-through and launcher runs stay on the host; a
/// docker run puts the command inside a container that mounts only the per-job workdir. The launch
/// is a pure transform over `(agent_command, JobLaunch)`, so the run/exec path stays executor-
/// agnostic — swapping executors is the only thing that changes here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPolicy {
    kind: PolicyKind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum PolicyKind {
    /// Spawn the agent command exactly as configured, on the host.
    #[default]
    Passthrough,
    /// Prepend a launcher argv (e.g. `bwrap …`) the command runs under, on the host.
    Launcher(Vec<String>),
    /// Run the command inside a container that mounts only the per-job workdir.
    Docker(DockerPolicy),
}

/// The default `docker` sandbox image a seat runs jobs in when `[sandbox] image` is unset. The
/// binary OWNS this ref: it pins the version to this build's `CARGO_PKG_VERSION`, so a seller who
/// installs the npm package and does nothing gets the sandbox image published for exactly that
/// release (`.github/workflows/publish-sandbox-image.yml` pushes `:v<release-tag>`, matching this
/// crate's version). The `[sandbox] image` config field remains ONLY for a future fully-custom
/// image — it is deliberately NOT a version selector; sellers never pin a version by hand.
pub const DEFAULT_SANDBOX_IMAGE: &str =
    concat!("ghcr.io/makeprisms/maxplayer-sandbox:v", env!("CARGO_PKG_VERSION"));

/// A resolved `docker` executor: a validated image, plus any operator-named extra environment to
/// carry in. Built from [`crate::home::SandboxConfig`] via [`SandboxPolicy::from_config`], which
/// defaults the image to [`DEFAULT_SANDBOX_IMAGE`] when the operator names none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerPolicy {
    image: String,
    forward_env: Vec<String>,
    /// The container runtime (`docker run --runtime <name>`), `None` ⇒ the daemon default (`runc`).
    /// The v1 posture sets this to `runsc` (gVisor) on Linux, where it is the primary boundary; a Mac
    /// seat leaves it unset and leans on the platform VM plus the hardening flags. Resolved from
    /// [`crate::home::SandboxConfig::runtime`].
    runtime: Option<String>,
    /// The dedicated docker network the container joins (`docker run --network <name>`), `None` ⇒
    /// the daemon default bridge. Resolved from [`crate::home::SandboxConfig::network`]. The network
    /// is what gives the #797 egress rules a stable interface to scope to — see
    /// [`crate::sandbox_net`].
    network: Option<String>,
    /// The port range the per-job credential proxy binds inside, `None` ⇒ the shipped ephemeral-port
    /// behaviour. Resolved from [`crate::home::SandboxConfig::proxy_port_range`]. Carried on the
    /// policy because the proxy is started by the same launch path that builds the argv, and the
    /// firewall pinhole and the bind must name the same ports or the job cannot reach its model.
    proxy_ports: Option<crate::sandbox_net::PortRange>,
    /// Credentials the proxy sources from a host FILE instead of the daemon's environment (#852).
    /// Resolved from [`crate::home::SandboxConfig::file_credentials`]. Carried on the policy for the
    /// same reason as `proxy_ports`: the containment path that reads them is the one that builds the
    /// launch, so both come from one config value rather than being written down twice.
    file_credentials: Vec<crate::home::FileCredential>,
}

/// The agent-auth environment carried from the daemon into the container.
///
/// A host executor inherits the daemon's environment, so a signed-in CLI simply works; a container
/// inherits NOTHING, and without this every docker job dies on an auth error the operator can only
/// fix by baking a credential into an image layer. Forwarding at run time keeps the secret out of a
/// distributable artifact and lets it be rotated by restarting the daemon.
///
/// An ALLOWLIST, never the whole environment: the container runs a stranger's code, so it receives
/// the named variables and nothing else. The names come from the auth prerequisites the presets
/// already state (`agent_presets::preset_prerequisite`) — nothing here is invented, and a preset
/// whose CLI reads something else is served by `[sandbox] forward_env`.
///
/// The base-URL pair rides along deliberately. An operator who points the daemon at a gateway and
/// gets the key forwarded WITHOUT the endpoint would have the credential sent to the default
/// provider instead — a worse failure than not forwarding at all.
pub const FORWARDED_AGENT_ENV: &[&str] = &[
    // claude — `preset_prerequisite("claude")` names ANTHROPIC_API_KEY; the OAuth pair is what
    // `claude /login` actually leaves behind.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    // codex — `preset_prerequisite("codex")` names OPENAI_API_KEY.
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
];

/// The per-job facts a launch needs beyond the agent command: where the job's workdir is on the
/// host (the docker bind-mount source), the delivery-identity env to carry into the run, and the
/// host uid/gid to run the container as (so bind-mounted output is owned by the seller and the
/// snapshot can read it).
pub struct JobLaunch<'a> {
    pub workdir: &'a Path,
    pub env: &'a [(String, String)],
    pub uid: u32,
    pub gid: u32,
    /// The name of the holder container whose network namespace this job joins, when egress
    /// containment has been established for it (#797, [`crate::sandbox_netns`]). `None` ⇒ the job gets
    /// the configured network, i.e. the behaviour before containment existed.
    ///
    /// This is the difference between "a network was configured" and "the policy is in force": the
    /// rules live in the namespace this names, and they were installed before this job's process
    /// existed. A `Some` here is therefore a containment claim, not a networking preference.
    pub netns: Option<&'a str>,
}

/// What the ACP driver spawns: the process `program` + `args`, and the `cwd` the ACP session runs
/// in. `cwd` is the host workdir for a host launch, and the in-container mount point for a docker
/// launch (the host path does not exist inside the container).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunch {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl SandboxPolicy {
    /// A pass-through policy: the agent command runs exactly as configured.
    pub fn passthrough() -> Self {
        Self {
            kind: PolicyKind::Passthrough,
        }
    }

    /// A policy that runs the agent command inside `launcher` (its argv is prepended). An empty
    /// launcher is a pass-through.
    pub fn wrapped(launcher: Vec<String>) -> Self {
        let kind = if launcher.is_empty() {
            PolicyKind::Passthrough
        } else {
            PolicyKind::Launcher(launcher)
        };
        Self { kind }
    }

    /// A policy that runs the agent command inside a container.
    pub fn docker(policy: DockerPolicy) -> Self {
        Self {
            kind: PolicyKind::Docker(policy),
        }
    }

    /// Resolve the policy from the optional `[sandbox]` config. Absent ⇒ pass-through. Under docker
    /// mode a missing (or blank) `image` DEFAULTS to [`DEFAULT_SANDBOX_IMAGE`] — the binary supplies
    /// the version-pinned GHCR ref — so a fresh seller who sets only `mode = "docker"` gets a working
    /// container without naming an image.
    pub fn from_config(config: Option<&crate::home::SandboxConfig>) -> Result<Self, ExecError> {
        use crate::home::SandboxMode;
        let Some(config) = config else {
            return Ok(Self::passthrough());
        };
        match config.mode {
            SandboxMode::Launcher => Ok(Self::wrapped(config.launcher.clone())),
            SandboxMode::Docker => {
                let image = config
                    .image
                    .clone()
                    .filter(|image| !image.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string());
                let runtime = config
                    .runtime
                    .clone()
                    .filter(|runtime| !runtime.trim().is_empty());
                let network = config
                    .network
                    .clone()
                    .filter(|network| !network.trim().is_empty());
                // A malformed range is refused HERE, at config resolution, not at job time. The
                // alternative — falling back to an ephemeral port — would silently move the proxy
                // outside the range the firewall pinhole names, and the seat would look contained
                // while every job failed to reach its model for a reason no message explains.
                let proxy_ports = config
                    .proxy_port_range
                    .as_deref()
                    .map(str::trim)
                    .filter(|range| !range.is_empty())
                    .map(crate::sandbox_net::PortRange::parse)
                    .transpose()
                    .map_err(|error| {
                        ExecError::Config(format!("[sandbox] proxy_port_range: {error}"))
                    })?;
                // Refused HERE, at config resolution, for the same reason as a malformed port range:
                // a relative path would otherwise resolve against whatever cwd the daemon happens to
                // have, and the job would fail to authenticate with nothing naming the path as the
                // cause.
                for cred in &config.file_credentials {
                    if !cred.path.is_absolute() {
                        return Err(ExecError::Config(format!(
                            "[sandbox] file_credentials: path must be absolute, got {}",
                            cred.path.display()
                        )));
                    }
                    for (label, value) in [
                        ("field", &cred.field),
                        ("env", &cred.env),
                        ("upstream", &cred.upstream),
                    ] {
                        if value.trim().is_empty() {
                            return Err(ExecError::Config(format!(
                                "[sandbox] file_credentials: {label} must not be empty (path {})",
                                cred.path.display()
                            )));
                        }
                    }
                    // Checked here as well as in the deserializer because `FileCredential` is also
                    // constructed in code, which never goes through serde. An empty list would leave
                    // the client talking to the vendor with the placeholder.
                    if cred.endpoint_args.is_empty() {
                        return Err(ExecError::Config(format!(
                            "[sandbox] file_credentials: endpoint_args must name at least one flag \
                             (path {})",
                            cred.path.display()
                        )));
                    }
                    for flag in &cred.endpoint_args {
                        if flag.trim().is_empty() {
                            return Err(ExecError::Config(format!(
                                "[sandbox] file_credentials: endpoint_args must not contain an empty \
                                 flag (path {})",
                                cred.path.display()
                            )));
                        }
                        // The same no-whitespace invariant the deserializer enforces. Repeated here
                        // because a code-constructed `FileCredential` never passes through serde, so
                        // the deserializer's guard is not a guard over this path.
                        if flag.chars().any(char::is_whitespace) {
                            return Err(ExecError::Config(format!(
                                "[sandbox] file_credentials: endpoint_args flag {flag:?} contains \
                                 whitespace; give one flag per entry, unpadded (path {})",
                                cred.path.display()
                            )));
                        }
                    }
                    if crate::credential_proxy::authority_of(&cred.upstream).is_none() {
                        return Err(ExecError::Config(format!(
                            "[sandbox] file_credentials: upstream {} is not a valid URL",
                            cred.upstream
                        )));
                    }
                    // Forwarding the SAME variable the placeholder occupies is refused rather than
                    // merged. Both would reach the launch as `-e NAME=…` and only one can win, so the
                    // seat's behaviour would rest on argument order; and if the daemon's copy differs
                    // from the file's (a stale export beside a re-logged-in file) it is not in the
                    // substitution set and would cross RAW. A config error is the only outcome here
                    // that cannot leak.
                    let env_name = cred.env.trim();
                    if config.forward_env.iter().any(|name| name.trim() == env_name)
                        || FORWARDED_AGENT_ENV.contains(&env_name)
                    {
                        return Err(ExecError::Config(format!(
                            "[sandbox] file_credentials: {env_name} is also forwarded as an \
                             environment variable — remove it from forward_env, because the \
                             placeholder and the daemon's own copy cannot both occupy it"
                        )));
                    }
                    // And the same name claimed by two entries. Docker keeps the LAST `-e NAME=…`,
                    // so the shadowed entry's placeholder never reaches the container and its
                    // upstream rejects every job — a per-job failure nobody can attribute to a
                    // config typo. Neither value is real, so this is fail-visible rather than a
                    // leak; it is refused here because boot is where the operator is still looking.
                    if config
                        .file_credentials
                        .iter()
                        .filter(|other| other.env.trim() == env_name)
                        .count()
                        > 1
                    {
                        return Err(ExecError::Config(format!(
                            "[sandbox] file_credentials: {env_name} is claimed by two entries — \
                             docker keeps the last one, so the other's placeholder never reaches \
                             the container and its upstream rejects every job"
                        )));
                    }
                }
                Ok(Self::docker(DockerPolicy {
                    image,
                    forward_env: config.forward_env.clone(),
                    runtime,
                    network,
                    proxy_ports,
                    file_credentials: config.file_credentials.clone(),
                }))
            }
        }
    }

    /// Whether this policy launches the command directly on the host with no wrapping.
    pub fn is_passthrough(&self) -> bool {
        matches!(self.kind, PolicyKind::Passthrough)
    }

    /// The launcher argv this policy prepends, empty under a pass-through or a docker policy. The
    /// seller boot gate's doctor check reads argv0 from here to verify the launcher resolves BEFORE
    /// it can break every job at spawn — the same argv [`Self::launch`] prepends, so the check tests
    /// exactly what the exec path runs. Under `docker` there is no launcher argv to resolve; the
    /// executor is named by [`Self::docker_image`] instead.
    pub fn launcher(&self) -> &[String] {
        match &self.kind {
            PolicyKind::Launcher(launcher) => launcher,
            PolicyKind::Passthrough | PolicyKind::Docker(_) => &[],
        }
    }

    /// The operator's extra `[sandbox] forward_env` names, `None` under a host policy — which needs
    /// no forwarding at all, having inherited the daemon's environment. The `None`/`Some(&[])`
    /// distinction is load-bearing: it separates "nothing to forward" from "forward the built-in
    /// set and nothing more".
    pub fn forward_env(&self) -> Option<&[String]> {
        match &self.kind {
            PolicyKind::Docker(policy) => Some(&policy.forward_env),
            PolicyKind::Passthrough | PolicyKind::Launcher(_) => None,
        }
    }

    /// The image a docker policy runs the command in, `None` under a host policy. The doctor checks
    /// read it to name the executor the seat is actually configured for: `launcher()` is empty under
    /// docker too, and reporting that as "no launcher" would read as "unsandboxed".
    pub fn docker_image(&self) -> Option<&str> {
        match &self.kind {
            PolicyKind::Docker(policy) => Some(policy.image.as_str()),
            PolicyKind::Passthrough | PolicyKind::Launcher(_) => None,
        }
    }

    /// The dedicated sandbox network a docker policy joins its containers to, `None` under a host
    /// policy or when the operator has not configured one.
    pub fn sandbox_network(&self) -> Option<&str> {
        match &self.kind {
            PolicyKind::Docker(policy) => policy.network.as_deref(),
            PolicyKind::Passthrough | PolicyKind::Launcher(_) => None,
        }
    }

    /// The port range the credential proxy must bind inside for this seat, `None` ⇒ an ephemeral
    /// port. Read by the containment path so the proxy's bind and the firewall's pinhole name the
    /// same ports — two artifacts that must agree, derived from one config value rather than
    /// written down twice.
    pub fn proxy_ports(&self) -> Option<crate::sandbox_net::PortRange> {
        match &self.kind {
            PolicyKind::Docker(policy) => policy.proxy_ports,
            PolicyKind::Passthrough | PolicyKind::Launcher(_) => None,
        }
    }

    /// The file-sourced credentials this policy contains (#852), empty under a host policy — which
    /// needs no containment at all, having inherited the daemon's environment and filesystem.
    pub fn file_credentials(&self) -> &[crate::home::FileCredential] {
        match &self.kind {
            PolicyKind::Docker(policy) => &policy.file_credentials,
            PolicyKind::Passthrough | PolicyKind::Launcher(_) => &[],
        }
    }

    /// The host argv for `agent_command` under this policy — the command unchanged under
    /// pass-through, the launcher argv followed by the command under a launcher — or `None` under a
    /// docker policy, whose launch is not expressible as a bare host argv (it needs the per-job
    /// mount, uid and env; see [`Self::launch`]). The containment probe, which measures a policy
    /// with no job to launch, is the only caller.
    pub fn wrap(&self, agent_command: &[String]) -> Option<Vec<String>> {
        match &self.kind {
            PolicyKind::Passthrough => Some(agent_command.to_vec()),
            PolicyKind::Launcher(launcher) => {
                let mut argv = Vec::with_capacity(launcher.len() + agent_command.len());
                argv.extend_from_slice(launcher);
                argv.extend_from_slice(agent_command);
                Some(argv)
            }
            PolicyKind::Docker(_) => None,
        }
    }

    /// Build what the ACP driver spawns for `agent_command` under this policy and the per-job
    /// `job`. Fails closed when the agent command is empty (a wrapper alone is not a runnable
    /// command).
    pub fn launch(
        &self,
        agent_command: &[String],
        job: &JobLaunch<'_>,
    ) -> Result<AgentLaunch, ExecError> {
        if agent_command.is_empty() {
            return Err(ExecError::Config("agent_command empty".into()));
        }
        let argv = match &self.kind {
            PolicyKind::Passthrough => agent_command.to_vec(),
            PolicyKind::Launcher(launcher) => {
                let mut argv = Vec::with_capacity(launcher.len() + agent_command.len());
                argv.extend_from_slice(launcher);
                argv.extend_from_slice(agent_command);
                argv
            }
            PolicyKind::Docker(policy) => {
                // A placeholder shares the container environment with the delivery identity, the
                // forwarded allowlist, and any base-URL override. Docker keeps the LAST
                // `-e NAME=…`, so a name set twice silences one claimant: its placeholder never
                // arrives and its upstream rejects every job. `contain_env_values` cannot cause
                // this — it resolves an override by mutating the pair it finds — so the appended
                // file-credential pairs are the only path that can produce a duplicate.
                //
                // Scoped to file-credential names deliberately. A seat with no `file_credentials`
                // iterates nothing here, so this guard cannot change any launch that works today.
                for cred in &policy.file_credentials {
                    let name = cred.env.trim();
                    if job.env.iter().filter(|(key, _)| key.trim() == name).count() > 1 {
                        return Err(ExecError::Config(format!(
                            "the container environment sets {name} twice — docker keeps the last \
                             value, so the placeholder for {} would be dropped and every job to \
                             it would fail to authenticate",
                            cred.upstream
                        )));
                    }
                }
                let argv = policy.run_argv(agent_command, job);
                // The ACP session runs at the in-container mount point, not the host path.
                return Ok(split_argv(argv, PathBuf::from(CONTAINER_WORKDIR)));
            }
        };
        Ok(split_argv(argv, job.workdir.to_path_buf()))
    }
}

impl DockerPolicy {
    /// The `docker run …` argv that launches `agent_command` in the container. It mounts ONLY the
    /// per-job workdir (read-write, at [`CONTAINER_WORKDIR`]) so the host `$MAXPLAYER_HOME` is absent by
    /// construction, drops to the seller's uid/gid so the mounted output is owned by the seller,
    /// and carries the delivery-identity env. ACP stdio survives through
    /// `docker run -i` (stdin/stdout piped; no tty).
    ///
    /// Hardening, because the job is a stranger's code and the docker defaults are wrong for it:
    ///
    /// `--cap-drop ALL` — a non-root `--user` already leaves the process with an EMPTY effective
    /// capability set, so this removes no capability the job could use today. It pins the BOUNDING
    /// set to empty, so no capability can be regained (e.g. via a setuid-root binary), and states the
    /// zero-capability posture explicitly rather than leaning on the uid to imply it. Cheap and safe
    /// — dev toolchains (`npm`/`pip`/`cargo` installing under $HOME as the same uid) need no
    /// capability. Paired with `no-new-privileges` below, the two close the setuid → container-root
    /// route from both ends.
    ///
    /// `--security-opt no-new-privileges` — the setuid-root binaries a base image ships
    /// (`node:22-bookworm-slim` carries 8, `su`/`mount`/`umount` among them) are the other half.
    /// Without the flag `NoNewPrivs` is 0 and a job can try to become container-root; with it that
    /// route is closed. The host tree is unreachable either way — this narrows what a job can do
    /// inside its own container, not what it can reach outside.
    ///
    /// `--init` — otherwise PID 1 is the ACP adapter, a node process that does not reap. An agent that
    /// spawns subprocesses accumulates zombies for the life of the container. `--init` makes PID 1 a
    /// real reaper instead.
    ///
    /// `--runtime <name>` (optional) — the container runtime. Unset ⇒ the daemon default (`runc`,
    /// which shares the host kernel). The v1 posture sets `runsc` (gVisor) on Linux so the payload
    /// runs against a userspace kernel, not the host one; a Mac seat leaves it unset (the platform VM
    /// is the boundary there). The named runtime must be registered with the daemon, or the run fails
    /// at spawn — a fail-closed the seller boot doctor is meant to catch first.
    fn run_argv(&self, agent_command: &[String], job: &JobLaunch<'_>) -> Vec<String> {
        let mut argv: Vec<String> = vec!["docker".into(), "run".into(), "-i".into()];
        // Runtime first, so it is unambiguously a `docker run` flag and not read as the image.
        if let Some(runtime) = &self.runtime {
            argv.push("--runtime".into());
            argv.push(runtime.clone());
        }
        // ⛔ There is deliberately NO `--rm`, and adding one back deletes the evidence this path
        // exists to keep. `--rm` removes on container EXIT — and a job that SUCCEEDS exits promptly,
        // so the container is gone before `capture_then_remove` can read it. Capture then fails,
        // removal reports success, and that pair is by definition `evidence_lost`: on the happy path,
        // every time. `capture_then_remove` states that evidence comes first and the order is the
        // whole point; `--rm` is the one flag that makes that order unenforceable.
        //
        // `--rm` was never the cleanup story either. The driver's shutdown signals `child.id()` — the
        // outer `docker run` CLIENT, not the container (`driver/acp_driver.rs`, `shutdown`) — so on a
        // timeout or an abort the client dies, the container keeps running, and `--rm` never fires
        // anyway. Removal is owned by `capture_then_remove`, with `JobContainer`'s `Drop` as the
        // fallback ([`DropFallback::ReportSkippedThenRemove`]). Both remove BY NAME, and both capture
        // first. What survives is a hard kill of the seller between container exit and cleanup, which
        // leaves a named container a later attempt collides on loudly rather than a silent leak.
        //
        // A deterministic name is what makes the container addressable at all. This run is `-i`, not
        // `-d`, so no id is printed on stdout to read back, and without `--name`/`--label`/`--cidfile`
        // nothing downstream can inspect, log or remove THIS container rather than some container.
        // Derived from the job id (never random) exactly as `sandbox_netns::holder_name` is, for the
        // reason stated there: a stale container can then be attributed to the job that leaked it, and
        // a second attempt for the same job collides loudly instead of quietly leaking the first.
        let job_id = job_id_of(job.workdir);
        argv.push("--name".into());
        argv.push(job_container_name(&job_id));
        argv.push("--label".into());
        argv.push(format!("{JOB_LABEL}={job_id}"));
        argv.extend(
            [
                "--init",
                "--security-opt",
                "no-new-privileges",
                "--cap-drop",
                "ALL",
            ]
            .into_iter()
            .map(String::from),
        );
        argv.extend([
            "--user".into(),
            format!("{}:{}", job.uid, job.gid),
            "-v".into(),
            format!("{}:{CONTAINER_WORKDIR}", job.workdir.display()),
            "-w".into(),
            CONTAINER_WORKDIR.into(),
        ]);
        // Egress containment (#797): join the namespace a holder container already owns, where the
        // rendered policy is in force BEFORE this process exists — the rules are not applied to the
        // job, the job is started into them. `crate::sandbox_netns` establishes that; `None` here
        // means it was not established, and the job falls back to the configured network (or, unset,
        // to the daemon default — exactly the behaviour before any of this existed).
        //
        // Name resolution survives the swap: a container joining a namespace still gets its own
        // /etc/resolv.conf pointing at docker's embedded resolver on 127.0.0.11 (measured), which is
        // why `sandbox_net` must never deny loopback.
        match job.netns {
            Some(holder) => {
                argv.push("--network".into());
                argv.push(format!("container:{holder}"));
            }
            None => {
                if let Some(network) = &self.network {
                    argv.push("--network".into());
                    argv.push(network.clone());
                }
            }
        }
        // Credential containment (#647): when the env points the agent at the host-side proxy
        // (`…=http://host.docker.internal:<port>`), the container must be able to resolve that alias.
        // On Linux docker does not provide it by default; `--add-host …:host-gateway` maps it to the
        // host. This is the single pinhole to the proxy's host:port that #797's host-services deny must
        // preserve. Added only when something actually references the alias, so a docker run with no
        // containment carries no inert flag.
        //
        // ⛔ NEVER under namespace containment: the daemon REFUSES the combination outright —
        // `conflicting options: custom host-to-IP mapping and the network mode` (measured, rc 125) —
        // so adding it there does not weaken containment, it prevents the job from starting at all.
        // Such a job needs no alias: `sandbox_netns` measures the address and puts the literal in the
        // env, which is also what the firewall pinhole names.
        if job.netns.is_none()
            && job
                .env
                .iter()
                .any(|(_, value)| value.contains(crate::credential_proxy::PROXY_HOST_ALIAS))
        {
            argv.push("--add-host".into());
            argv.push(format!("{}:host-gateway", crate::credential_proxy::PROXY_HOST_ALIAS));
        }
        for (key, value) in job.env {
            argv.push("-e".into());
            argv.push(format!("{key}={value}"));
        }
        argv.push(self.image.clone());
        argv.extend_from_slice(agent_command);
        argv
    }
}

/// The agent-auth environment to carry into a container launch, read from the daemon's own
/// environment: the [`FORWARDED_AGENT_ENV`] allowlist plus anything `[sandbox] forward_env` adds.
///
/// Empty for a host executor — it already inherits the daemon's environment, so forwarding would be
/// a no-op that only made the argv longer. Only variables actually SET are returned, so an unset
/// name never becomes an empty `-e FOO=` that would override a value baked into the image.
pub fn forwarded_agent_env(policy: &SandboxPolicy) -> Vec<(String, String)> {
    forwarded_agent_env_from(policy, |key| std::env::var(key).ok())
}

/// [`forwarded_agent_env`] over an injected environment, so the allowlist can be tested without
/// mutating the process environment out from under other tests.
fn forwarded_agent_env_from(
    policy: &SandboxPolicy,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let Some(extra) = policy.forward_env() else {
        return Vec::new();
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for key in FORWARDED_AGENT_ENV.iter().copied().chain(extra.iter().map(String::as_str)) {
        let key = key.trim();
        // An operator repeating a built-in name must not produce a doubled `-e`.
        if key.is_empty() || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if let Some(value) = lookup(key) {
            out.push((key.to_owned(), value));
        }
    }
    out
}

/// Split a non-empty argv into `(program, args)` and pair it with the session `cwd`.
fn split_argv(argv: Vec<String>, cwd: PathBuf) -> AgentLaunch {
    let mut argv = argv.into_iter();
    let program = argv
        .next()
        .expect("split_argv is only called with a non-empty argv");
    AgentLaunch {
        program,
        args: argv.collect(),
        cwd,
    }
}

/// The uid/gid an awarded job's process runs as: this daemon's own identity.
///
/// ONE expression, called by both the awarded-job path ([`run_agent_job`]) and the capability probe
/// ([`probe_launch_argv`]), so "the probe runs as the uid a job gets" is a shared call rather than a
/// comment claiming two separate reads agree. Under docker this is what reaches `docker run --user`;
/// the host executors inherit the daemon's identity anyway and ignore it.
///
/// ⚠ Deliberately NOT the owner of any directory. A directory's gid is inherited from its parent
/// when the parent is setgid, so a workdir's group can differ from the creating process's — and a
/// probe run under a gid no job ever gets would answer for an identity that does not exist.
pub fn job_identity() -> (u32, u32) {
    // SAFETY: `getuid`/`getgid` take no arguments, cannot fail, and only read the calling process's
    // own credentials.
    unsafe { (libc::getuid(), libc::getgid()) }
}

/// The argv that runs `probe_command` in the JOB execution environment under `policy` (#802).
///
/// **This is [`SandboxPolicy::launch`], never [`SandboxPolicy::wrap`], and that is the whole point.**
/// `wrap` yields no argv under docker, because a container launch is not expressible as a bare host
/// argv — it needs the per-job mount, uid and env. A probe built on `wrap` therefore cannot reach a
/// docker seat at all, and the ONLY safe reading of that absence is "not proven": running the bare
/// command instead would execute it on the HOST while jobs run inside a container, advertising a
/// capability the job will not have. `launch` is total over all three executors, so the probe reaches
/// every one of them without a fallback existing to be taken.
///
/// ⚠ **This function constructs no argv of its own.** It supplies a [`JobLaunch`] and returns what
/// the awarded-job path would be given for the same inputs. That is deliberate and load-bearing: a
/// second container-argv builder is how the two drift, and a bad launcher argv does not fail one
/// probe — it fails every job the seat is offered (#357). There is one builder, and the probe is
/// merely another caller of it.
///
/// `workdir` is a throwaway directory the caller creates and removes, and a missing one is REFUSED
/// rather than passed through. Measured: with a non-existent bind source, docker creates it as
/// `uid=0 gid=0 mode=755`, and the container — running as the job's uid — then cannot write its own
/// workdir. That failure is silent for the `--version` probes this renders, because a version check
/// writes nothing and still exits 0, so the probe would answer correctly while standing in an
/// environment no job would ever get. The guard costs one stat and removes the whole class.
///
/// The probe carries **no environment** — not the delivery identity, not the forwarded credential
/// allowlist. It asks whether a binary resolves, which needs no secret, so none is put where a
/// container could read it. It also claims **no egress containment** (`netns: None`), because none
/// was established for it.
///
/// ⚠ What a proven token means is bounded by exactly this: the command resolved in this environment
/// at this moment. It does not mean a build will succeed, and the environment can change before a
/// job arrives — the advertisement is a claim bounded by the probe's cadence, never a guarantee.
pub fn probe_launch_argv(
    policy: &SandboxPolicy,
    probe_command: &[String],
    workdir: &Path,
) -> Result<Vec<String>, ExecError> {
    if !workdir.is_dir() {
        return Err(ExecError::Config(format!(
            "the probe workdir {} does not exist — docker would create the bind source as root and \
             the probe would run in a workdir the job's uid cannot write",
            workdir.display()
        )));
    }
    let (uid, gid) = job_identity();
    let job = JobLaunch {
        workdir,
        env: &[],
        uid,
        gid,
        netns: None,
    };
    let launch = policy.launch(probe_command, &job)?;
    let mut argv = Vec::with_capacity(launch.args.len() + 1);
    argv.push(launch.program);
    argv.extend(launch.args);
    Ok(argv)
}

/// A wall-clock bound generous enough for a `--version` or a cold container start, short enough that a
/// stuck probe cannot hold the pre-advertise path open.
pub const CAPABILITY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the best-effort container removal after a docker probe gets before it too is abandoned.
const PROBE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Run one already-rendered probe argv and report its outcome (#784).
///
/// The spawn half of the capability probe, kept here with the rest of the process machinery. `argv`
/// is ALREADY rendered for the job environment by [`probe_launch_argv`] — this function must never
/// render, because doing it in two places is how one of them ends up not doing it.
///
/// Return values carry the distinction the whole field rests on:
/// - `Ok(true)` — the command ran and exited 0: the capability is proven.
/// - `Ok(false)` — the command ran and exited non-zero, or (under a pass-through policy) the target
///   binary was absent: the capability is simply not present. Omit the token.
/// - `Err(_)` — the probe could not be measured at all (launcher missing, timeout). The caller must
///   treat this as a boot failure, never as a silent "no", because a buyer commits sats on this field.
///
/// Output is discarded (`--version` text is not the evidence, the exit status is) and never inherited,
/// so a probe cannot scribble on the operator's console.
///
/// `host_cwd` is the directory the spawned child runs in, and it is the HOST-side probe workdir —
/// the same path [`probe_launch_argv`] validated and handed to the policy. A job's agent session
/// starts in its own workdir, so a probe that runs from wherever the daemon happens to sit answers
/// for a directory no job ever gets: a cwd-sensitive wrapper or a toolchain that reads a local
/// config file resolves differently, and the token it produces describes the daemon's environment
/// rather than the job's.
///
/// ⚠ **NOT [`AgentLaunch::cwd`], and the difference is not cosmetic.** For a docker policy that
/// field is [`CONTAINER_WORKDIR`] — an IN-CONTAINER path that does not exist on this host, because
/// the container reaches its workdir through the bind mount and `-w`. The child spawned here is
/// `docker` ITSELF, running on the host, so giving it the container's path fails to spawn and every
/// docker probe becomes an error. The host path is correct for all three executors at once: it is
/// where a pass-through or launcher probe genuinely runs, and for docker it is the CLI's own cwd,
/// which the container never sees.
///
/// `timeout` bounds the run; on expiry the child is killed and reaped. `docker_container` is
/// `Some(name)` when the policy is docker — the deterministic container the render named — so that:
///   1. a launcher (`docker`) that cannot even spawn is an error, not a false "not proven"; and
///   2. the container is force-removed after the run, success or timeout alike. `--rm` is deliberately
///      absent from the job container ([`SandboxPolicy::run_argv`]), so without this the probe would
///      leak a container and the next token's identically-named run would collide.
///
/// ⚠ This is a MEASUREMENT, not a gate: it answers only "did this command run cleanly HERE, NOW".
pub fn probe_command_outcome(
    argv: &[String],
    host_cwd: &Path,
    timeout: Duration,
    docker_container: Option<&str>,
) -> Result<bool, ProbeRunError> {
    let Some((program, args)) = argv.split_first() else {
        return Ok(false);
    };
    let spawned = std::process::Command::new(program)
        .args(args)
        .current_dir(host_cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        // Under docker the program is `docker` itself; if it cannot spawn, the probe never ran and
        // there is nothing to clean up. Under a pass-through policy the program IS the probe target,
        // so its absence is the honest "not proven".
        Err(error) => {
            return match docker_container {
                Some(_) => Err(ProbeRunError::LauncherUnspawnable(error)),
                None => Ok(false),
            };
        }
    };

    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.success()),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(ProbeRunError::TimedOut { after: timeout });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => break Err(ProbeRunError::Wait(error)),
        }
    };

    // The job container has no `--rm`, so a docker probe leaves a stopped (or, on timeout, running)
    // container behind. Remove it by its deterministic name before returning, so the next token's run
    // does not collide and no residue survives the probe. Best-effort and itself bounded.
    if let Some(name) = docker_container {
        let _ = run_bounded_nulled(&force_remove_argv(name), PROBE_CLEANUP_TIMEOUT);
    }

    outcome
}

/// Spawn `argv` with every stdio nulled and wait at most `timeout`, killing and reaping on expiry.
/// Used for the probe's own container cleanup, so even teardown cannot hang the pre-advertise path.
fn run_bounded_nulled(argv: &[String], timeout: Duration) -> Result<bool, ProbeRunError> {
    let Some((program, args)) = argv.split_first() else {
        return Ok(false);
    };
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(ProbeRunError::LauncherUnspawnable)?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProbeRunError::TimedOut { after: timeout });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(ProbeRunError::Wait(error)),
        }
    }
}

/// The deterministic container name a docker probe in `workdir` will create, or `None` when the policy
/// is not docker. The same value [`probe_launch_argv`] embeds as `--name`, exposed so a caller can
/// force-remove that exact container after the probe. See [`probe_command_outcome`].
pub fn probe_container_name(policy: &SandboxPolicy, workdir: &Path) -> Option<String> {
    policy
        .docker_image()
        .map(|_| job_container_name(&job_id_of(workdir)))
}

/// The per-job working directory under the home (`$MAXPLAYER_HOME/seller-jobs/<job_id>`).
pub fn job_workdir(home: &MaxplayerHome, job_id: &str) -> PathBuf {
    home.root.join("seller-jobs").join(job_id)
}

/// An owned throwaway workdir for the boot capability probe, removed when this value drops (#784).
///
/// It lives under the seller-jobs root — the same place a real job's workdir lives — so the probe runs
/// where jobs run, not beside the seller process. The leaf is unique across SEATS and BOOTS
/// (`capability-probe-<pid>-<nanos>`): two seats on one host differ by pid, and one seat across two
/// boots differs by the nanosecond stamp. That uniqueness is load-bearing beyond tidiness, because the
/// leaf also feeds docker's deterministic container name ([`probe_container_name`]) — two seats sharing
/// a leaf would collide on that name, and a boot that reused a prior boot's leaf could adopt a stale
/// container.
///
/// RAII rather than a caller-remembered cleanup: the probe runs on the pre-advertise path, which has
/// several early-return and `?` points, and a leaked probe dir under seller-jobs is indistinguishable
/// from a real job's. `Drop` removes the tree on every exit, including an unwind.
pub struct ProbeWorkdir {
    path: PathBuf,
}

impl ProbeWorkdir {
    /// Create the unique probe workdir under `$MAXPLAYER_HOME/seller-jobs/`. Fails loudly: a workdir
    /// that cannot be created means the probe cannot run in the environment a job would, and the caller
    /// must refuse to advertise rather than probe somewhere a job never stands.
    pub fn create(home: &MaxplayerHome) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let leaf = format!("capability-probe-{}-{}", std::process::id(), nanos);
        let path = home.root.join("seller-jobs").join(leaf);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The directory to hand [`probe_launch_argv`] as the probe workdir.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProbeWorkdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The job id a workdir belongs to — the inverse of [`job_workdir`]'s last component.
///
/// Used to name per-job docker objects after the job that owns them, so a leaked one can be
/// attributed instead of merely noticed. Sanitised to what docker accepts in a container name
/// (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`), and never empty: a workdir with no usable final component would
/// otherwise produce a name docker rejects at the moment containment is being established.
fn job_id_of(workdir: &Path) -> String {
    let raw = workdir.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' { c } else { '-' })
        .collect();
    let cleaned = cleaned.trim_start_matches(['.', '-', '_']).to_owned();
    if cleaned.is_empty() {
        "unattributed".to_owned()
    } else {
        cleaned
    }
}

/// The label a job container carries, so a leaked one can be attributed and listed.
///
/// Mirrors [`crate::sandbox_netns::HOLDER_LABEL`] deliberately: the holder and the job container are
/// two per-job docker objects with ONE naming story, not two.
pub const JOB_LABEL: &str = "ai.maxplayer.job";

/// The job container's name for `job_id`.
///
/// Derived from the job id rather than random, for the reasons stated on
/// [`crate::sandbox_netns::holder_name`]: a stale container can be attributed to the job that leaked
/// it, and a second attempt for the same job collides loudly instead of quietly leaking the first.
pub fn job_container_name(job_id: &str) -> String {
    format!("maxplayer-job-{job_id}")
}

/// The directory a job's captured diagnostics are written to: `$MAXPLAYER_HOME/seller-diagnostics/<job_id>`.
///
/// ⛔ **Deliberately NOT inside the job workdir, and this is a containment property rather than
/// tidiness.** The workdir is the ONE host path bind-mounted into the container
/// (`-v <workdir>:<CONTAINER_WORKDIR>`, read-write), so a stranger's job can read and overwrite
/// anything there. Evidence about a job must not be writable by that job — capture written into the
/// mount could be edited by the very run it indicts.
///
/// ⚠ **And the file mode does NOT substitute for this.** The container runs `--user` with
/// [`job_identity`] — the daemon's OWN uid — so inside the mount the job is the same uid that wrote
/// the capture, and `0600` grants it full access. The owner-only mode below protects these files from
/// OTHER users on a shared host; only being outside the mount protects them from the job itself. Two
/// different threats, and only one of them is answered by a permission bit.
///
/// Derived by inverting [`job_workdir`] rather than taking a new parameter, and the inversion is
/// CHECKED: the middle component must be `seller-jobs`, or this is not a path `job_workdir` built and
/// `None` is returned instead of a guess. A wrong directory here would scatter diagnostics somewhere
/// nobody looks, which reads exactly like the capture never ran.
fn job_diagnostics_dir(workdir: &Path) -> Option<PathBuf> {
    let jobs_dir = workdir.parent()?;
    if jobs_dir.file_name()? != "seller-jobs" {
        return None;
    }
    let job_id = job_id_of(workdir);
    Some(jobs_dir.parent()?.join("seller-diagnostics").join(job_id))
}

/// `docker inspect` argv for the job container, **field-selected**.
///
/// ⛔ **Never a bare `docker inspect`.** Its JSON embeds `Config.Env`, and on a run WITHOUT credential
/// containment that carries the real `ANTHROPIC_API_KEY` (the `-e` pairs this module builds). A
/// capture file is written to disk and read later by a human, so a bare inspect would persist a live
/// credential to a file forever. Naming the fields makes the credential absent BY CONSTRUCTION rather
/// than scrubbed afterwards — the difference between a guarantee and a filter that has to be right.
///
/// The fields are the ones that answer *why did this job fail*: terminal state and exit code,
/// OOM-kill (the failure that looks like a silent hang), docker's own error string, the start/finish
/// instants, and the network mode (which names the holder it was joined to).
pub fn inspect_argv(name: &str) -> Vec<String> {
    [
        "docker",
        "inspect",
        "--format",
        "status={{.State.Status}} exit_code={{.State.ExitCode}} oom_killed={{.State.OOMKilled}} \
         error={{printf \"%q\" .State.Error}} started_at={{.State.StartedAt}} \
         finished_at={{.State.FinishedAt}} network_mode={{.HostConfig.NetworkMode}} \
         image={{.Config.Image}}",
        name,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker logs` argv for the job container — the agent's own stdout/stderr.
///
/// `--timestamps` because the question a capture answers is usually *when did it stop*, and the
/// container's clock is the only one that saw it.
pub fn logs_argv(name: &str) -> Vec<String> {
    ["docker", "logs", "--timestamps", name]
        .into_iter()
        .map(String::from)
        .collect()
}

/// `docker rm` argv that removes the job container by exact name.
///
/// `--force` because the container this exists for is one that is still RUNNING: the client died and
/// `--rm` never fired, so a plain `rm` would refuse. `--volumes` matches the holder's teardown.
pub fn force_remove_argv(name: &str) -> Vec<String> {
    ["docker", "rm", "--force", "--volumes", name]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Header names whose VALUE is a credential and must never reach a capture file.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "cookie",
    "set-cookie",
];

/// The marker a redacted span is replaced by. Matches the crate's existing `<redacted…>` convention
/// (see `wallet_ops`), so one grep finds every redaction the daemon has ever written.
const REDACTION_MARKER: &str = "<redacted>";

/// Strip credentials out of captured text before it is written to disk.
///
/// Three passes, because a credential reaches a log by three different routes and each needs its own:
///
/// 1. **Exact values.** `secrets` holds the live credential values this run actually forwarded. Any
///    verbatim occurrence is replaced. This is the only pass that can catch a token in a shape nobody
///    anticipated, which is why the caller passes values rather than relying on patterns.
/// 2. **Header lines.** Everything after the colon on a [`SENSITIVE_HEADERS`] line goes, so an agent
///    that logs its own outgoing request does not persist the header value.
/// 3. **Token shapes.** `sk-`-prefixed runs, which is what every vendor key in
///    [`CONTAINED_CREDENTIALS`] looks like — the backstop for a credential this daemon never held and
///    so could not list in `secrets` (one the job brought itself).
///
/// ⚠ Pass 1 is the load-bearing one and passes 2 and 3 are backstops. A redactor built only on
/// patterns answers "I found no credential" identically whether the text is clean or merely
/// unfamiliar — so the values are supplied, never inferred.
///
/// Short `secrets` entries are ignored: a 3-character "secret" would redact ordinary prose and the
/// resulting file would be useless for the diagnosis it exists to serve.
pub fn redact(text: &str, secrets: &[String]) -> String {
    const MIN_SECRET_LEN: usize = 8;
    let mut out = text.to_owned();
    for secret in secrets {
        let secret = secret.trim();
        if secret.len() >= MIN_SECRET_LEN {
            out = out.replace(secret, REDACTION_MARKER);
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for line in out.lines() {
        match line.split_once(':') {
            Some((head, _)) if SENSITIVE_HEADERS.contains(&head.trim().to_ascii_lowercase().as_str()) => {
                lines.push(format!("{head}: {REDACTION_MARKER}"))
            }
            _ => lines.push(redact_token_shapes(line)),
        }
    }
    lines.join("\n")
}

/// Replace `sk-…` runs in one line. Split out so [`redact`] reads as its three passes.
fn redact_token_shapes(line: &str) -> String {
    /// Shortest `sk-` run treated as a token. Vendor keys are far longer; this only has to be long
    /// enough that a literal "sk-" in prose is left alone.
    const MIN_TOKEN_LEN: usize = 16;
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("sk-") {
        let (before, from_token) = rest.split_at(at);
        out.push_str(before);
        let token_len = from_token
            .char_indices()
            .find(|(index, c)| *index > 0 && !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
            .map_or(from_token.len(), |(index, _)| index);
        if token_len >= MIN_TOKEN_LEN {
            out.push_str(REDACTION_MARKER);
        } else {
            out.push_str(&from_token[..token_len]);
        }
        rest = &from_token[token_len..];
    }
    out.push_str(rest);
    out
}

/// One `docker` invocation, injected so the cleanup ORDER can be observed in a test.
///
/// The effectful implementation is [`RealDockerCli`]. This is a seam rather than a direct
/// `Command::new("docker")` for one reason: the property that matters here is a SEQUENCE — capture
/// strictly before removal — and a sequence cannot be checked by reading the source, only by
/// recording what was called. See `sandbox_netns` for the sibling style where the argv builders are
/// pure and unit-tested; this adds the one seam that style leaves untestable.
pub trait DockerCli {
    /// Run `argv`, returning combined output on success or a message on failure.
    fn run(&mut self, argv: &[String]) -> Result<String, String>;
}

/// The real `docker` runner: a blocking `std::process::Command`.
///
/// Synchronous, like [`crate::sandbox_netns::NetnsHolder`]'s teardown and for the same reason — this
/// runs on paths that include a panicking or aborted job, where a spawned task can be dropped by a
/// shutting-down runtime.
pub struct RealDockerCli;

impl DockerCli for RealDockerCli {
    fn run(&mut self, argv: &[String]) -> Result<String, String> {
        let (program, args) = argv.split_first().ok_or("an empty docker argv")?;
        let done = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| format!("could not run `{program}`: {error}"))?;
        let stdout = String::from_utf8_lossy(&done.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&done.stderr).trim().to_owned();
        match done.status.code() {
            Some(0) => Ok(stdout),
            Some(code) => Err(format!(
                "exit {code}: {}",
                if stderr.is_empty() { &stdout } else { &stderr }
            )),
            None => Err("killed by a signal".to_string()),
        }
    }
}

/// What cleanup did, with capture and removal reported **independently**.
///
/// ⚠ Two `Result`s and not one, deliberately. A single status collapses "evidence saved but the
/// container is still there" and "container gone, evidence lost" into one word — and those want
/// opposite responses. A caller that only ever reads a combined verdict cannot tell an orphan from a
/// blind spot.
#[derive(Debug)]
pub struct CleanupReport {
    /// Where the diagnostics were written, or why they were not.
    pub capture: Result<PathBuf, String>,
    /// Whether the exact job container is gone, or why it is not.
    pub removal: Result<(), String>,
}

impl CleanupReport {
    /// The one state nothing else can recover from: the container is gone AND its evidence was not
    /// saved. Named so a caller can act on it rather than re-deriving it from two fields.
    pub fn evidence_lost(&self) -> bool {
        self.capture.is_err() && self.removal.is_ok()
    }
}

/// Capture the job container's diagnostics, THEN remove it.
///
/// **Evidence first, and the order is the whole point.** The tree already states this rule for the
/// containment sidecar (`sandbox_netns::sidecar_argv`: *"`--rm` is safe here specifically because the
/// caller captures stdout and stderr before the container is removed; the evidence is in hand before
/// the container is gone"*). This is that rule applied to the job container, which is the one place it
/// was missing.
///
/// Writes, owner-only, into `dir`:
/// - `inspect.txt` — the field-selected state ([`inspect_argv`]);
/// - `logs.txt` — the container's own stdout/stderr, redacted ([`logs_argv`], [`redact`]);
/// - `event-log.jsonl` — a copy of the run's maxplayer event log, redacted, so the bundle is
///   self-contained. The original stays in the workdir; this is a copy, not a move, because the
///   workdir's own copy is what the delivery path already excludes and nothing here should change that.
///
/// **A capture failure never silently becomes a clean removal.** Every leg's outcome is recorded, and
/// removal is attempted regardless — a container left running is a resource leak that also blocks the
/// next attempt on the same job id, so refusing to remove it would trade one failure for two. What the
/// caller must not be able to do is read "removed" and infer "captured", which is why
/// [`CleanupReport`] keeps them apart and [`CleanupReport::evidence_lost`] names the bad pair.
pub fn capture_then_remove<D: DockerCli>(
    cli: &mut D,
    name: &str,
    dir: &Path,
    event_log: Option<&Path>,
    secrets: &[String],
) -> CleanupReport {
    let capture = capture_into(cli, name, dir, event_log, secrets);
    // Attempted on BOTH branches. See the doc comment: not removing would leave a running container
    // AND a name collision for the next attempt.
    let removal = cli.run(&force_remove_argv(name)).map(|_| ());
    CleanupReport { capture, removal }
}

/// The capture half of [`capture_then_remove`], split out so the `?` shorthand can be used without
/// letting an early return skip the removal.
fn capture_into<D: DockerCli>(
    cli: &mut D,
    name: &str,
    dir: &Path,
    event_log: Option<&Path>,
    secrets: &[String],
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("could not create the diagnostics directory {}: {error}", dir.display()))?;
    restrict_to_owner(dir, 0o700)?;

    let inspect = cli.run(&inspect_argv(name))?;
    write_owner_only(&dir.join("inspect.txt"), &redact(&inspect, secrets))?;
    // `docker logs` on a container that produced nothing exits 0 with empty output; an error here is a
    // real failure to READ the logs, which is exactly the case that must not be reported as captured.
    let logs = cli.run(&logs_argv(name))?;
    write_owner_only(&dir.join("logs.txt"), &redact(&logs, secrets))?;
    if let Some(path) = event_log {
        // Absent is normal — the run can fail before the driver writes its first event — so a missing
        // event log is not a capture failure. An UNREADABLE one is.
        match std::fs::read_to_string(path) {
            Ok(events) => write_owner_only(&dir.join("event-log.jsonl"), &redact(&events, secrets))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("could not read the event log {}: {error}", path.display()))
            }
        }
    }
    Ok(dir.to_path_buf())
}

/// Write `body` to `path` with mode `0600`, creating it.
///
/// Owner-only because the file holds a stranger's job output on a host that may run more than one
/// seat, and because the redaction above is a backstop rather than a proof: a capture file should not
/// be world-readable even when we believe it is clean.
fn write_owner_only(path: &Path, body: &str) -> Result<(), String> {
    std::fs::write(path, body)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    restrict_to_owner(path, 0o600)
}

/// Set `mode` on `path`. A no-op off unix, where the concept does not apply.
fn restrict_to_owner(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("could not restrict {} to its owner: {error}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// The structured marker the `Drop` fallback emits. A FIXED string, because it is what an operator
/// greps for when a job has no diagnostics — a reworded marker is an unfindable record.
pub const CAPTURE_SKIPPED_MARKER: &str = "capture_skipped=drop_fallback";

/// What [`JobContainer`]'s `Drop` must do, as a VALUE.
///
/// Extracted from `Drop` so the decision is testable: `Drop` itself reaches a real docker daemon and
/// cannot return a result, so a test can observe neither its outcome nor its ordering. The variants
/// are the whole permitted vocabulary — note that **none of them captures**, which is the property
/// this type exists to make structural rather than remembered.
#[derive(Debug, PartialEq, Eq)]
pub enum DropFallback {
    /// An explicit [`capture_then_remove`] already ran; the fallback issues nothing.
    Nothing,
    /// No explicit cleanup ran: emit [`CAPTURE_SKIPPED_MARKER`], THEN remove.
    ReportSkippedThenRemove,
}

/// The fallback for a guard in state `settled`.
///
/// Reporting comes first and removal second, and never the reverse: the marker is the only record
/// that will exist about this container, so it must be written while the claim is still true rather
/// than after a step that can fail or abort.
fn drop_fallback(settled: bool) -> DropFallback {
    if settled {
        DropFallback::Nothing
    } else {
        DropFallback::ReportSkippedThenRemove
    }
}

/// An RAII handle on the job container, adopted the moment the container can exist.
///
/// Mirrors [`crate::sandbox_netns::NetnsHolder`]: the guard exists from the moment the container does,
/// so a `?`, an early return or an unwind cannot leave one behind. The difference is what each guard
/// does — the holder's `Drop` is the whole teardown story, whereas this one is a REMOVE-ONLY fallback
/// for paths that never reached [`Self::settle`].
pub struct JobContainer {
    name: String,
    /// Set by [`Self::settle`] once an explicit [`capture_then_remove`] has run, so `Drop` knows
    /// whether it is the fallback or a no-op.
    settled: bool,
}

impl JobContainer {
    /// Adopt `name` under the guard. Call this where the container becomes possible, not where it
    /// becomes certain — `docker run` can fail after creating it.
    pub fn adopt(name: String) -> Self {
        Self { name, settled: false }
    }

    /// The container's name, for the commands that address it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Record that the explicit evidence-first path has run, so `Drop` does nothing.
    pub fn settle(&mut self) {
        self.settled = true;
    }
}

impl Drop for JobContainer {
    /// Remove the container — and **only** remove it.
    ///
    /// ⛔ **Capture must never move into here, and the reason is in this crate already.**
    /// [`crate::sandbox_netns::NetnsHolder`]'s own `Drop` states it: *"Failure is logged, never
    /// propagated: `Drop` cannot return."* A capture placed in `Drop` is therefore a guard whose
    /// failure mode is SILENCE, sitting directly over the evidence it is meant to save — a capture
    /// that fails here, followed by a removal that succeeds, recreates the exact bug this work exists
    /// to fix, and reports nothing.
    ///
    /// So this path is honest about being second-best: it emits a structured
    /// `capture_skipped=drop_fallback` record BEFORE removing. Without that marker, "no diagnostics
    /// for this job" is ambiguous between *capture ran and there was nothing to find* and *the
    /// fallback removed the container before anything captured it* — two states that are
    /// byte-identical in the record and want different investigations.
    fn drop(&mut self) {
        if drop_fallback(self.settled) == DropFallback::Nothing {
            return;
        }
        // Emitted BEFORE the removal, not after: this line is the only evidence that will exist about
        // this container, so it must be written while the claim is still true rather than depending on
        // reaching the end of a path that is already the abnormal one.
        eprintln!(
            "sandbox: {CAPTURE_SKIPPED_MARKER} container={} reason=no_explicit_cleanup",
            self.name
        );
        match RealDockerCli.run(&force_remove_argv(&self.name)) {
            Ok(_) => {}
            Err(error) => eprintln!(
                "sandbox: {CAPTURE_SKIPPED_MARKER} container={} removal_failed={error}",
                self.name
            ),
        }
    }
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
) -> Result<AgentRunReport, ExecError>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<AgentRunReport, ExecError>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match run(attempt).await {
            Ok(report) => return Ok(report),
            // Retry only while BOTH an attempt and the deadline remain; otherwise surface the error
            // so the caller publishes feedback-kind exactly once (past deadline / exhausted).
            Err(_) if attempt < max_attempts && now() < deadline_unix => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Daemon/node-owned delivery, plus the job agent's PROMPT PREAMBLE — identity, job context,
/// boundaries (#685, #731), and the buyer's declared output type (#686).
///
/// ⚠ It is a PREAMBLE IN THE USER TURN, **not** a protocol-level system prompt, because ACP has no
/// system-prompt surface: [`SessionConfig`](crate::driver::SessionConfig) is `{cwd, mcp_servers,
/// env}` — the whole of `session/new` — and the single `session/prompt` turn carries one text
/// block. Composing it HERE hands every harness the same preamble from one place. The only form
/// that would be a true system prompt is a per-harness launcher flag (`--append-system-prompt` and
/// its equivalents), which has to be repeated per harness — so a newly added harness would
/// silently ship without one.
///
/// The seller appends explicit, secret-free delivery instructions to the agent's task prompt so the
/// agent delivers by committing its work to the git repository in its working directory — rather
/// than guessing a delivery channel. The seller performs the authenticated push of the committed
/// branch to the bound remote (NIP-98; the agent is never handed a key), so this text carries NO
/// secret — it is public prompt text built only from the task, the deadline, and the (public)
/// remote URL.
///
/// Three shape decisions worth keeping:
/// - The task stays FIRST. The buyer's instructions must not be pushed down by our own prose.
/// - `deadline_unix` is stated as an ABSOLUTE epoch second, never as a remaining budget: the prompt
///   is composed once, before [`run_agent_with_retry`], so a relative figure would be a lie on the
///   second attempt.
/// - No `date` invocation is suggested. `date -u -d @…` is GNU-only and sellers run on macOS too.
///
/// It carries NO refusal instruction, deliberately (#685). A refusal today either writes nothing —
/// quarantining the harness — or writes a refusal note that mints a sentinel and gets PAID, so
/// inviting one before the pre-money seam handles it means paying for refusals. The test below
/// asserts that ABSENCE, so it cannot be reintroduced by accident.
pub fn compose_agent_prompt(
    task: &str,
    git_remote: &str,
    deadline_unix: u64,
    declared_output: Option<&str>,
    memory_section: Option<&str>,
) -> String {
    // #686: the buyer's `["output", …]` tag is MANDATORY on ingest and is a MIME / output type
    // (`text/plain`, `application/json`). Stating it here is the only way the hired agent learns what
    // form was asked for — it reads this prompt and nothing else of the offer. `None` ⇒ say nothing:
    // an offer recorded before the column existed has no declared type, and inventing a default would
    // put a fact in the prompt that no buyer stated. Blank is treated as absent for the same reason.
    //
    // It is a STATEMENT, not a gate. Nothing downstream refuses or penalises a delivery whose format
    // does not match — that is a money-path decision with its own blast radius, deliberately not here.
    let output_section = match declared_output.map(str::trim).filter(|value| !value.is_empty()) {
        Some(output) => format!(
            "DECLARED OUTPUT TYPE: {output}. The buyer declared this output type on the offer, so \
             produce the deliverable in that form. The task above wins where the two disagree.\n"
        ),
        None => String::new(),
    };
    let base = format!(
        "{task}\n\n\
         ---\n\
         CONTEXT — from the seller daemon running you, not from the buyer. It applies to how you \
         carry out the task above.\n\
         WHO YOU ARE: an autonomous agent working a PAID job on the maxplayer marketplace. A buyer \
         posted the task above, the seller node running you claimed it on your behalf, and the \
         buyer settles payment when your work is delivered. Treat it as contracted work.\n\
         DEADLINE: {deadline_unix} (Unix epoch seconds, UTC). Work that is not on disk by then may \
         never be delivered, so finish and leave your work in place rather than running long.\n\
         {output_section}\
         BOUNDARIES:\n\
         - Your current working directory is the job directory and the whole of your scope. Work \
         only inside it.\n\
         - Deliver only what the task asks for; do not add unrequested files.\n\
         - Never read, write, or reveal credentials, tokens, key material, or any file outside \
         this directory — not in your deliverable, and not in anything you print.\n\
         - If you wait on a background process, match something your own waiter cannot contain — \
         a PID or pidfile captured at start, or `pgrep -x` against an exact program name — never \
         `pgrep -f` on a substring of your own command line.\n\
         TOOLING: this runtime is deliberately thin, so a compiler, runtime or CLI your task \
         needs may be absent — installing it is expected, and the container is yours alone and \
         is discarded when the job ends. You run as an unprivileged user with no sudo and no \
         capabilities, so a system package manager will fail; install under `$HOME` instead, and \
         never into the job directory, where it would become part of your deliverable. Prefer \
         `nix develop` when the project ships a flake and nix is present, otherwise a user-local \
         installer such as rustup, `pip install --user`, or `npm install -g` with a prefix under \
         `$HOME`. When `$HOME` is not writable, say so in your output and carry on with the \
         rest of the task — a named obstacle is worth more to the buyer than a deadline spent \
         hiding one.\n\
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

/// The fixed delivery-subject prefix. Its 20 bytes are charged against [`SUBJECT_BYTE_BUDGET`].
const SUBJECT_PREFIX: &str = "maxplayer delivery: ";

/// Byte budget for the WHOLE emitted subject line — prefix included, not the summary alone (#637).
///
/// Two deliberate choices, neither inherited:
///
/// * **72 BYTES, not 72 chars.** A git subject is bytes on the wire and 72 is git's own subject
///   width (the 50/72 convention `git log` formats to, and where GitHub truncates a title). The
///   pre-fix `summary.chars().take(72)` bounded Unicode scalar values, which bounds nothing a
///   subject cares about: 72 chars of non-ASCII is up to 288 bytes.
/// * **The budget covers the LINE, so the prefix is charged to it.** What the convention bounds is
///   the rendered line, and the prefix is part of every rendered line. Spending 72 on the summary
///   alone is precisely what put a 92-byte subject on `main` (#635, quoted in #637) — so keeping
///   72-for-the-summary would have preserved the number while leaving the complaint standing.
///   The summary's own budget is therefore what is left over: 72 - 20 = 52 bytes.
const SUBJECT_BYTE_BUDGET: usize = 72;

/// Truncate `summary` to at most `budget` BYTES, cutting only at a word boundary and never inside a
/// UTF-8 character.
///
/// Boundary-safe BY CONSTRUCTION, which is the whole point: `&s[..n]` panics when `n` is not a char
/// boundary, so the index is walked DOWN to the nearest boundary BEFORE any slicing happens. The
/// walk always terminates because index 0 is a boundary. Every slice below is taken at an index that
/// has already been proven to be one, so no input can panic here.
///
/// The head is then trimmed back to the last ASCII space — which is exactly a word boundary, since
/// the caller has already collapsed all whitespace to single spaces. Two edge arms: a cut landing
/// exactly ON a space is already word-complete (trimming would drop a whole word for nothing), and a
/// single word longer than the entire budget has no boundary to trim to, so it is hard-cut at the
/// char boundary rather than truncated away to nothing.
///
/// No ellipsis is appended, consistently, in every arm: a git subject is scarce, every byte spent
/// marking the truncation is a byte not spent saying what was delivered, and the emitted result must
/// never differ from the input for an under-budget summary.
fn cap_summary_bytes(summary: &str, budget: usize) -> &str {
    if summary.len() <= budget {
        return summary;
    }
    let mut end = budget;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    if summary.as_bytes().get(end) == Some(&b' ') {
        return &summary[..end];
    }
    match summary[..end].rfind(' ') {
        Some(space) => &summary[..space],
        None => &summary[..end],
    }
}

/// A concise, single-line delivery-commit message derived from the offer task: the first non-empty
/// line, whitespace-collapsed and capped to [`SUBJECT_BYTE_BUDGET`] bytes at a word boundary. Falls
/// back to a fixed label for an empty task.
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
        return "maxplayer delivery".to_owned();
    }
    let capped = cap_summary_bytes(
        &summary,
        SUBJECT_BYTE_BUDGET.saturating_sub(SUBJECT_PREFIX.len()),
    );
    // Unreachable at the const budget (52 bytes leaves room for at least one char of any summary),
    // but it is what keeps the no-trailing-space invariant true by construction rather than by
    // arithmetic: a prefix with nothing after it is the bare label, never `"…delivery: "`.
    if capped.is_empty() {
        return "maxplayer delivery".to_owned();
    }
    format!("{SUBJECT_PREFIX}{capped}")
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

/// What one agent run reports back: the ACP usage the driver surfaced, and the agent's own last
/// message.
///
/// The message is carried because a COMPLETED turn is not a successful one. An agent whose plan is
/// exhausted, or whose model host is unreachable, ends its turn normally and explains itself in
/// ordinary assistant text — so the turn's shape cannot tell that apart from a model that simply did
/// nothing, and only the text can. Measured 2026-08-21: a contained cursor seat returned
/// `getaddrinfo EAI_AGAIN` for its model host in exactly this field, on the first run, while the
/// caller reported a flaky model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentRunReport {
    /// Usage the driver surfaced for this run. `None` when the harness exposed nothing.
    pub usage: Option<UsageMetadata>,
    /// The agent's last non-empty message this turn, verbatim. `None` when it said nothing at all —
    /// which is itself informative, and distinct from an empty string.
    pub last_agent_message: Option<String>,
}

/// Run the awarded agent under the ACP driver: one session in `workdir`, seeded with `prompt`, with
/// the delivery `identity`'s git env, bounded by `timeout` (the unified job timeout). The agent
/// command is launched through `policy` — directly under a pass-through policy, or inside the
/// policy's launcher.
#[cfg(feature = "acp")]
pub async fn run_agent_job(
    agent_command: &[String],
    policy: &SandboxPolicy,
    prompt: &str,
    workdir: &Path,
    identity: &DeliveryAgentIdentity,
    timeout: AgentRunTimeout,
) -> Result<AgentRunReport, ExecError> {
    use crate::driver::{AcpDriver, AgentCommand, ContentBlock, PromptTurn, SessionConfig};
    use crate::engine::{run_job, RunParams};
    use crate::event::JobId;
    use crate::log::EventLog;

    // Run the container/process as the seller's own uid/gid so a docker bind-mount's output is owned
    // by the seller and the delivery snapshot can read it. Ignored by the host executors.
    //
    // The delivery identity, plus the agent-auth allowlist a container needs because it inherits
    // nothing from the daemon. Empty under a host executor, which already inherits it all.
    let mut env = identity.git_env();
    // The argv actually spawned. Identical to `agent_command` unless containment adds a redirect flag
    // for a file-sourced credential, whose proxy URL is not known until the proxy is bound.
    let mut effective_command = agent_command.to_vec();
    let forwarded = forwarded_agent_env(policy);
    // The credential VALUES this run forwards, held for the capture redactor's exact-value pass.
    // Taken here because `forwarded` is consumed into `env` further down, and taken as values rather
    // than trusting patterns because a pattern-only redactor reports "no credential found" identically
    // for text that is clean and text whose token shape it does not recognise.
    //
    // ⛔ These are never printed, formatted into an error, or written anywhere but through
    // [`redact`], which replaces them. A capture path is exactly where a secret must not leak.
    let forwarded_secrets: Vec<String> = forwarded.iter().map(|(_, value)| value.clone()).collect();
    // Credential containment (#647). Under docker the real model credential must NOT enter the
    // container: a stranger's job can read `-e ANTHROPIC_API_KEY` and exfiltrate a reusable secret.
    // Start a per-job host proxy that holds the real credential, forward a format-plausible
    // placeholder + a base-URL override pointing at the proxy in its place, and keep the proxy alive
    // for the run (dropping `_proxy` at fn end revokes the placeholder). If containment is required
    // but cannot be established, the job FAILS — there is no fallback to putting the real credential
    // in the container.
    let (uid, gid) = job_identity();
    // Egress containment (#797), FIRST, because two things downstream depend on what it measures: the
    // job's `--network` names the holder it creates, and the credential proxy's base URL must carry the
    // address it resolved. Declared before `_proxy` so it is dropped LAST — the namespace has to
    // outlive the job that runs in it.
    //
    // Established only for a docker policy with a configured network. No network ⇒ no containment,
    // which is the behaviour a seat had before any of this existed; it is not silently claimed.
    let _containment;
    let holder = match (policy.docker_image(), policy.sandbox_network()) {
        (Some(image), Some(network)) => {
            let established = crate::sandbox_netns::establish(
                network,
                image,
                crate::sandbox_netns::DEFAULT_NETFILTER_IMAGE,
                crate::credential_proxy::PROXY_HOST_ALIAS,
                &job_id_of(workdir),
                // The holder is stamped with this seat's own key, so the boot reaper of a co-tenant
                // daemon can tell it is not theirs to remove.
                identity.seller_pubkey_hex(),
                uid,
                gid,
                policy.proxy_ports(),
                true,
            )
            .await
            // Fail the job rather than run it uncontained. The whole point of moving containment into
            // the namespace is that "configured but not enforced" stops being representable, and a
            // fallback here would put it straight back.
            .map_err(|error| {
                ExecError::Policy(format!("[sandbox] egress containment not established: {error}"))
            })?;
            let name = established.holder.name().to_owned();
            let host = established.proxy_host.clone();
            _containment = Some(established);
            Some((name, host))
        }
        _ => {
            _containment = None;
            None
        }
    };
    // Credential containment (#647). Under docker the real model credential must NOT enter the
    // container: a stranger's job can read `-e ANTHROPIC_API_KEY` and exfiltrate a reusable secret.
    // Start a per-job host proxy that holds the real credential, forward a format-plausible
    // placeholder + a base-URL override pointing at the proxy in its place, and keep the proxy alive
    // for the run (dropping `_proxy` at fn end revokes the placeholder). If containment is required
    // but cannot be established, the job FAILS — there is no fallback to putting the real credential
    // in the container.
    let _proxy;
    if policy.docker_image().is_some() {
        // A namespace-contained job reaches the proxy at the measured address, not at the docker
        // alias: `--add-host` and `--network=container:…` are mutually exclusive, so the alias would
        // never resolve inside it. The same string is what the firewall pinhole names.
        let proxy_host = holder
            .as_ref()
            .map(|(_, host)| host.as_str())
            .unwrap_or(crate::credential_proxy::PROXY_HOST_ALIAS);
        match start_credential_containment(
            &forwarded,
            policy.file_credentials(),
            policy.proxy_ports(),
            proxy_host,
        )
        .await?
        {
            Some(containment) => {
                // A contained credential the job cannot reach is worse than a loud failure: the run
                // would burn its whole timeout on auth errors. The pinhole comes from `proxy_ports`,
                // so without a range there is no hole for the proxy to be reached through.
                if holder.is_some() && policy.proxy_ports().is_none() {
                    return Err(ExecError::Config(
                        "[sandbox] docker: a contained credential needs [sandbox] proxy_port_range \
                         when egress containment is active — without it the firewall opens no pinhole \
                         and the job cannot reach its model"
                            .into(),
                    ));
                }
                env.extend(containment.env);
                // A file-sourced credential is reached through the client's own flag, not a base-URL
                // variable, so the redirect has to land in the argv the driver spawns.
                effective_command.extend(containment.argv_extra);
                _proxy = Some(containment.proxy);
            }
            None => env.extend(forwarded),
        }
    } else {
        env.extend(forwarded);
    }
    let job = JobLaunch {
        workdir,
        env: &env,
        uid,
        gid,
        netns: holder.as_ref().map(|(name, _)| name.as_str()),
    };
    let launch = policy.launch(&effective_command, &job)?;
    // The ACP idle/response timeout IS the unified job timeout — never a hardcoded 300s that could
    // override or conflict with `--job-timeout-secs`.
    let mut driver = AcpDriver::new(
        AgentCommand::new(launch.program, launch.args),
        crate::driver::PermissionOutcome::Allow,
        timeout.duration(),
    );
    let log_path = workdir.join(crate::seller_git::SELLER_RUN_LOG);
    let mut log = EventLog::open(&log_path).map_err(|error| ExecError::Agent(error.to_string()))?;
    let params = RunParams {
        session_config: SessionConfig {
            cwd: launch.cwd,
            mcp_servers: Vec::new(),
            env: identity.git_env(),
        },
        prompt: PromptTurn {
            input: vec![ContentBlock::Text {
                text: prompt.to_owned(),
            }],
        },
    };
    // The agent's own account of the turn, kept from the sink that was already called for every
    // update. A turn can complete having done nothing and say WHY in its last message — a blocked
    // host, an exhausted plan — and that text was previously dropped here, leaving the caller to
    // guess a cause from the turn's shape alone.
    let mut capture = crate::engine::AgentMessageCapture::default();
    // The job container is addressable by a name derived from the job, so adopt the guard BEFORE the
    // run: from here on every exit — return, `?`, panic, runtime shutdown — has something that will
    // remove it. Only a docker policy has a container at all; a host executor has nothing to guard.
    let mut container = policy
        .docker_image()
        .map(|_| JobContainer::adopt(job_container_name(&job_id_of(workdir))));
    let outcome = run_job(
        &mut driver,
        &mut log,
        &JobId(format!("seller-{}", short_hash(prompt))),
        params,
        &mut |event| capture.observe(event),
    )
    .await;
    // ⛔ The cleanup sits HERE, above the `?`, and moving it below would restore the bug. A timeout or
    // an agent failure returns `Err`, and the error exit is precisely the one that leaves a container
    // running — `AcpDriver::shutdown` kills the `docker run` CLIENT, so `--rm` never fires. A cleanup
    // placed after `map_err(…)?` would therefore run on every exit EXCEPT the ones that need it.
    if let Some(mut container) = container.take() {
        let name = container.name().to_owned();
        let destination = job_diagnostics_dir(workdir);
        let event_log = log_path.clone();
        let secrets = forwarded_secrets;
        // Owned: `spawn_blocking` needs `'static`, and this is only ever used in a message.
        let workdir_shown = workdir.display().to_string();
        // Off the runtime, for the reason `AcpDriver::shutdown` gives for its own blocking work: the
        // seller node runs every awarded job as a `spawn_local` task on ONE LocalSet thread, so
        // blocking docker calls here would stall every sibling job for the duration.
        let reported = tokio::task::spawn_blocking(move || match destination {
            Some(dir) => {
                capture_then_remove(&mut RealDockerCli, &name, &dir, Some(&event_log), &secrets)
            }
            // The workdir is not one `job_workdir` built, so there is nowhere derivable to put the
            // evidence. Still remove the container — a leak would also block the next attempt on this
            // job id — but say plainly that nothing was captured rather than reporting a clean exit.
            None => CleanupReport {
                capture: Err(format!(
                    "no diagnostics directory derivable from the workdir {workdir_shown}"
                )),
                removal: RealDockerCli.run(&force_remove_argv(&name)).map(|_| ()),
            },
        })
        .await;
        // The explicit path ran, so `Drop` must not also fire its fallback and report a
        // `capture_skipped` that did not happen. `settle` records that it RAN, not that it SUCCEEDED —
        // the two legs below carry the outcome.
        container.settle();
        match reported {
            // Reported as two independent facts. A single verdict would collapse "evidence saved, the
            // container is still there" and "container gone, evidence lost" — opposite problems.
            Ok(report) => {
                match &report.capture {
                    Ok(dir) => eprintln!(
                        "sandbox: job_capture=ok container={} dir={}",
                        container.name(),
                        dir.display()
                    ),
                    Err(error) => eprintln!(
                        "sandbox: job_capture=failed container={} error={error}",
                        container.name()
                    ),
                }
                match &report.removal {
                    Ok(()) => {
                        eprintln!("sandbox: job_cleanup=ok container={}", container.name())
                    }
                    Err(error) => eprintln!(
                        "sandbox: job_cleanup=failed container={} error={error}",
                        container.name()
                    ),
                }
                if report.evidence_lost() {
                    eprintln!(
                        "sandbox: job_capture=evidence_lost container={} — the container was removed \
                         and its diagnostics were not saved",
                        container.name()
                    );
                }
            }
            // The blocking task itself died, so neither leg has an answer and the container's state is
            // unknown. Saying so is the point: an unreported orphan is the failure mode this work
            // exists to remove.
            Err(error) => eprintln!(
                "sandbox: job_cleanup=unknown container={} error=cleanup task panicked: {error}",
                container.name()
            ),
        }
    }
    let outcome = outcome.map_err(|error| classify_run_error(error, timeout))?;
    match outcome.terminal {
        crate::event::JobExecutionStatus::Completed => Ok(AgentRunReport {
            usage: outcome.usage,
            last_agent_message: capture.into_last_message(),
        }),
        other => Err(ExecError::Agent(format!("agent terminal {other:?}"))),
    }
}

/// One contained credential variable: the env name that carries the secret, the base-URL variable
/// overridden to route it through the proxy, the vendor's default upstream when the operator set no
/// base URL, and the placeholder shape to mint (vendor prefix + random-tail length).
struct ContainedCred {
    /// The credential env var whose value is replaced by a per-job placeholder.
    env: &'static str,
    /// The base-URL env var pointed at the proxy so the client's request reaches it.
    base_url_env: &'static str,
    /// The vendor default upstream, used when the operator set no `base_url_env`.
    default_upstream: &'static str,
    /// The placeholder's vendor prefix (so a client that shape-validates locally does not refuse).
    placeholder_prefix: &'static str,
    /// Random characters after the prefix, chosen to match the vendor credential's length.
    placeholder_random_len: usize,
}

/// The credential variables the #647 proxy CONTAINS: each is removed from the container (replaced by a
/// per-job placeholder) and substituted for the real value at egress. All four use the identical
/// value-based mechanism; they differ only in placeholder shape and which vendor upstream they route
/// to. Verbatim-travel is PROVEN for `ANTHROPIC_API_KEY` (spike: single `x-api-key` header); the other
/// three are contained on the same mechanism but their verbatim-travel is verified by the red-team /
/// throwaway-login test. Fail-closed: if a token is derived rather than sent verbatim, substitution
/// misses and the job cannot authenticate — a break, never a leak.
const CONTAINED_CREDENTIALS: &[ContainedCred] = &[
    ContainedCred {
        env: "ANTHROPIC_API_KEY",
        base_url_env: "ANTHROPIC_BASE_URL",
        default_upstream: "https://api.anthropic.com",
        placeholder_prefix: "sk-ant-api03-",
        placeholder_random_len: 93,
    },
    ContainedCred {
        env: "ANTHROPIC_AUTH_TOKEN",
        base_url_env: "ANTHROPIC_BASE_URL",
        default_upstream: "https://api.anthropic.com",
        placeholder_prefix: "sk-ant-",
        placeholder_random_len: 96,
    },
    ContainedCred {
        env: "CLAUDE_CODE_OAUTH_TOKEN",
        base_url_env: "ANTHROPIC_BASE_URL",
        default_upstream: "https://api.anthropic.com",
        placeholder_prefix: "sk-ant-oat01-",
        placeholder_random_len: 93,
    },
    ContainedCred {
        env: "OPENAI_API_KEY",
        base_url_env: "OPENAI_BASE_URL",
        default_upstream: "https://api.openai.com",
        placeholder_prefix: "sk-",
        placeholder_random_len: 48,
    },
];

/// Whether `name` is a credential variable the proxy contains, across BOTH registries: the built-in
/// [`CONTAINED_CREDENTIALS`] table and the operator's `[sandbox] file_credentials`.
///
/// Both, because containment is the property being audited and it has two sources. A predicate over
/// only the table would report a file-sourced credential as uncontained, and — the direction that
/// actually costs something — would let the doctor's completeness claim be emitted by a check that
/// cannot see half of what it claims about.
fn is_contained_credential(name: &str, file_creds: &[crate::home::FileCredential]) -> bool {
    CONTAINED_CREDENTIALS.iter().any(|cred| cred.env == name)
        || file_creds.iter().any(|cred| cred.env.trim() == name)
}

/// Forwarded variables that would cross into a docker container UNCONTAINED: the operator-added
/// `[sandbox] forward_env` names (beyond the built-in allowlist) that are SET and are NOT one of the
/// contained credential variables — in EITHER registry (the built-in table or `[sandbox]
/// file_credentials`). What remains is operator-added names the daemon cannot recognize: a
/// `MY_AGENT_TOKEN` may well be a credential, and the daemon has no way to know, so it is flagged
/// rather than silently forwarded raw.
///
/// The scope of that claim is exactly "the two registries", never "every credential that exists".
///
/// Empty for a non-docker policy (a host executor inherits the daemon environment; there is no
/// container to leak into). `lookup` injects the environment so the gap is testable. Drives the loud
/// boot log and the doctor WARN (#647 P2).
pub fn uncontained_forwarded_credentials(
    policy: &SandboxPolicy,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    // `None` ⇒ non-docker (nothing forwarded); `Some(extras)` ⇒ docker, with the operator's extra
    // forward_env names (the built-in allowlist is contained or is a base URL, never raw here).
    let Some(extras) = policy.forward_env() else {
        return Vec::new();
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for name in extras {
        let name = name.trim();
        if name.is_empty()
            || is_contained_credential(name, policy.file_credentials())
            || seen.contains(&name)
        {
            continue;
        }
        seen.push(name);
        if lookup(name).is_some_and(|value| !value.trim().is_empty()) {
            out.push(name.to_owned());
        }
    }
    out
}

/// A per-job credential to substitute at egress, and the container-facing rewrite it implies.
#[cfg(feature = "acp")]
struct MintedCredential {
    real: String,
    placeholder: String,
    upstream: String,
}

/// What containment hands back to the launch: the container-facing environment, any argv the client
/// needs in order to reach the proxy, and the running proxy itself (dropped at job end, which revokes
/// every placeholder).
///
/// `argv_extra` exists because redirection is not uniformly an environment variable. A file-sourced
/// credential names the client's own flag instead — measured necessary for `cursor-agent`, whose env
/// base-URL overrides are ignored for credential-bearing traffic.
#[cfg(feature = "acp")]
struct Containment {
    env: Vec<(String, String)>,
    argv_extra: Vec<String>,
    proxy: crate::credential_proxy::RunningProxy,
}

/// The `type` claim minted into a file-credential placeholder.
///
/// A plausible value, NOT a measured requirement: nothing was observed reading it, and the signature
/// cannot be checked by the bearer in any case. Recorded as a choice so a later reader does not treat
/// it as a constraint discovered from the vendor.
#[cfg(feature = "acp")]
const FILE_CREDENTIAL_CLAIM_TYPE: &str = "session";

/// How long a file-credential placeholder claims to be valid.
///
/// Only has to outlast one job — the placeholder is revoked when the proxy drops at job end — but it
/// is generous because the failure is silent and one-sided: a client that refuses an
/// already-expired-looking token never reaches the wire, and the job dies with an auth error that
/// says nothing about `exp`. Rolling per job, never a fixed timestamp.
#[cfg(feature = "acp")]
const FILE_CREDENTIAL_PLACEHOLDER_LIFETIME: std::time::Duration =
    std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Read one file-sourced credential's real value from the host (#852).
///
/// Called per job rather than cached at daemon start, and that is load-bearing: the operator's
/// standing remediation for an expired session is to log in again, which REWRITES this file. A value
/// cached at startup would survive that re-login and every job would fail to authenticate AFTER being
/// awarded — the most expensive moment to discover a stale credential.
///
/// No error message can carry the file's content: a parse failure reports line and column only, and a
/// missing field names the field, never a value.
#[cfg(feature = "acp")]
fn read_file_credential(cred: &crate::home::FileCredential) -> Result<String, ExecError> {
    let raw = std::fs::read_to_string(&cred.path).map_err(|error| {
        ExecError::Config(format!(
            "[sandbox] file_credentials: cannot read {}: {error}",
            cred.path.display()
        ))
    })?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).map_err(|error| {
        ExecError::Config(format!(
            "[sandbox] file_credentials: {} is not valid JSON (line {}, column {})",
            cred.path.display(),
            error.line(),
            error.column()
        ))
    })?;
    parsed
        .get(&cred.field)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            ExecError::Config(format!(
                "[sandbox] file_credentials: {} has no non-empty string field `{}`",
                cred.path.display(),
                cred.field
            ))
        })
}

/// Establish credential containment for a docker job, or `Ok(None)` when no contained credential is
/// present (nothing to contain). For every contained credential the operator forwards, mint a per-job
/// placeholder, register `(placeholder → real, upstream)` with the proxy, and route the vendor's
/// base URL through the proxy.
///
/// File-sourced credentials (`[sandbox] file_credentials`) are handled alongside, differing in three
/// ways: the real value comes from a file rather than an env pair, the placeholder is a parseable JWT
/// rather than prefix-plus-random, and the client is redirected by an argv flag rather than a base-URL
/// variable.
///
/// `Err` on any failure to stand up the proxy or register a credential: the caller turns that into a
/// failed job. This is the no-fallback invariant — the one failure mode that must never silently leave
/// a real credential in the container.
#[cfg(feature = "acp")]
async fn start_credential_containment(
    forwarded: &[(String, String)],
    file_creds: &[crate::home::FileCredential],
    proxy_ports: Option<crate::sandbox_net::PortRange>,
    proxy_host: &str,
) -> Result<Option<Containment>, ExecError> {
    use crate::credential_proxy as proxy;
    use std::sync::Arc;

    let lookup = |key: &str| {
        forwarded
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.trim().is_empty())
    };

    // Resolve which contained credentials are actually present, and the upstream each routes to (the
    // operator's base URL for that vendor, else the vendor default). Collected before the proxy starts
    // so a bad base URL fails the job without leaving a half-started listener.
    let mut minted: Vec<(&'static ContainedCred, MintedCredential)> = Vec::new();
    let mut upstream_hosts: Vec<String> = Vec::new();
    for cred in CONTAINED_CREDENTIALS {
        let Some(real) = lookup(cred.env) else { continue };
        let upstream = lookup(cred.base_url_env)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| cred.default_upstream.to_owned());
        let host = proxy::authority_of(&upstream).ok_or_else(|| {
            ExecError::Config(format!(
                "[sandbox] docker: {}={upstream} is not a valid URL",
                cred.base_url_env
            ))
        })?;
        if !upstream_hosts.contains(&host) {
            upstream_hosts.push(host);
        }
        minted.push((
            cred,
            MintedCredential {
                real,
                placeholder: proxy::mint_placeholder(cred.placeholder_prefix, cred.placeholder_random_len),
                upstream,
            },
        ));
    }
    // File-sourced credentials (#852). Read and validated BEFORE the proxy starts, same rule as the
    // env-sourced ones above: an unreadable file or a missing field fails the job without leaving a
    // half-started listener behind.
    //
    // The placeholder is a JWT, not prefix-plus-random: the client PARSES this value (a malformed one
    // is refused locally and never reaches the wire, which presents as "containment broke the seat"
    // rather than as a bad placeholder). `exp` is per-job and rolling for the same reason a fixed one
    // would be wrong — it would start being refused at a date nothing in the config explains.
    let mut minted_files: Vec<(&crate::home::FileCredential, MintedCredential)> = Vec::new();
    for cred in file_creds {
        let real = read_file_credential(cred)?;
        let upstream = cred.upstream.trim().to_owned();
        let host = proxy::authority_of(&upstream).ok_or_else(|| {
            ExecError::Config(format!(
                "[sandbox] file_credentials: upstream {upstream} is not a valid URL"
            ))
        })?;
        if !upstream_hosts.contains(&host) {
            upstream_hosts.push(host);
        }
        minted_files.push((
            cred,
            MintedCredential {
                real,
                placeholder: proxy::mint_jwt_placeholder(
                    FILE_CREDENTIAL_CLAIM_TYPE,
                    FILE_CREDENTIAL_PLACEHOLDER_LIFETIME,
                ),
                upstream,
            },
        ));
    }

    if minted.is_empty() && minted_files.is_empty() {
        return Ok(None);
    }

    let engine = Arc::new(proxy::ProxyEngine::new(upstream_hosts));
    // The proxy is header-agnostic by design: it forwards whatever the container sent, and the
    // forwarded agent credential rides `x-api-key`. reqwest's default redirect policy is
    // `Policy::limited(10)`, and its cross-host scrub covers only AUTHORIZATION, COOKIE, cookie2,
    // PROXY_AUTHORIZATION and WWW_AUTHENTICATE — `x-api-key` is in none of them. So a 3xx from an
    // allowlisted host would carry the credential onward to a host the allowlist never approved:
    // the destination is decided BEFORE the redirect moves it.
    //
    // So a redirect is followed only while it stays on the upstream the credential in flight was
    // registered for. That upstream is not the allowlist: `authorize` picks the destination from the
    // credential itself, and the allowlist is the UNION of every present credential's upstream. A
    // union check would approve a 3xx from one registered vendor to ANOTHER — handing an Anthropic key
    // to OpenAI's host — because both are on it. A refused attempt is `stop()`, which returns the 3xx
    // for the proxy to relay to the container unchanged.
    //
    // The pairing is available without per-request state, which is why this stays one shared client:
    // `Policy::custom` closes over the ENGINE and never a request, so the credential cannot be reached
    // from here — but the attempt carries its own chain, and its FIRST entry is the original request
    // URL, which `relay` built from that credential's upstream.
    //
    // EVERY HOP, not just the first, and each judged against the ORIGINAL rather than its predecessor:
    // judging hop-against-predecessor would let a chain walk one authority at a time to anywhere.
    // Verified in reqwest 0.12.28 rather than assumed — `TowerRedirectPolicy::redirect`
    // (`src/redirect.rs:306`) pushes the previous URL onto an accumulating chain (`:315`) and then
    // calls this policy with THAT hop's target (`:317`), so `previous()[0]` is the original request URL
    // on every hop. An empty chain yields no original and is refused rather than followed.
    let forwarding_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let original = attempt.previous().first().map(|url| url.as_str()).unwrap_or("");
            if proxy::allows_paired_redirect(original, attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|error| ExecError::Agent(format!("credential proxy client: {error}")))?;
    let running = proxy::start(Arc::clone(&engine), forwarding_client, proxy_ports)
        .await
        .map_err(|error| ExecError::Agent(format!("credential proxy failed to start: {error}")))?;

    // `proxy_host` is the docker alias for an uncontained job and a measured literal address for a
    // namespace-contained one, which cannot resolve the alias at all. Passed in rather than decided
    // here so exactly one value reaches both this URL and the firewall's pinhole.
    let base_url = running.container_base_url_via(proxy_host);
    let mut substitutions: Vec<(String, String)> = Vec::with_capacity(minted.len());
    let mut base_url_overrides: Vec<&'static str> = Vec::new();
    for (cred, m) in &minted {
        engine
            .register(proxy::JobCredential {
                placeholder: m.placeholder.clone(),
                real: m.real.clone(),
                upstream: m.upstream.clone(),
            })
            .map_err(|refusal| {
                ExecError::Agent(format!("credential proxy registration refused: {refusal}"))
            })?;
        substitutions.push((m.real.clone(), m.placeholder.clone()));
        if !base_url_overrides.contains(&cred.base_url_env) {
            base_url_overrides.push(cred.base_url_env);
        }
    }

    // File-sourced credentials: register the swap, hand the container the PLACEHOLDER in the variable
    // the operator named, and append the client's own redirect flag to the argv.
    //
    // The real values join `substitutions` BEFORE the env rewrite below, so a forwarded variable that
    // happens to carry the same secret is scrubbed too — the same value-based defence the env-sourced
    // entries get, not a weaker path for arriving from a file.
    let mut placed: Vec<(&crate::home::FileCredential, String)> = Vec::new();
    for (cred, m) in &minted_files {
        engine
            .register(proxy::JobCredential {
                placeholder: m.placeholder.clone(),
                real: m.real.clone(),
                upstream: m.upstream.clone(),
            })
            .map_err(|refusal| {
                ExecError::Agent(format!("credential proxy registration refused: {refusal}"))
            })?;
        substitutions.push((m.real.clone(), m.placeholder.clone()));
        placed.push((cred, m.placeholder.clone()));
    }
    let (file_env, argv_extra) = file_credential_launch_additions(&placed, &base_url);

    let mut contained = contain_env_values(forwarded, &substitutions, &base_url_overrides, &base_url);
    // Appended AFTER the rewrite: these pairs carry placeholders, which have nothing to scrub.
    contained.extend(file_env);
    Ok(Some(Containment {
        env: contained,
        argv_extra,
        proxy: running,
    }))
}

/// Rewrite the forwarded env into what the container actually receives: every occurrence of each real
/// credential is replaced by its per-job placeholder (value-based, so an operator-added `[sandbox]
/// forward_env` variable carrying the same secret is scrubbed too — #647 acceptance #2), and each
/// vendor base URL in `base_url_overrides` is pointed at the proxy so the client's request reaches it.
/// No real credential value appears in any returned pair.
///
/// A pure transform so the red-prove test can assert every real credential is absent from the
/// container view without a container, a network, or a real key.
#[cfg(feature = "acp")]
pub fn contain_env_values(
    forwarded: &[(String, String)],
    substitutions: &[(String, String)],
    base_url_overrides: &[&str],
    base_url: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = forwarded
        .iter()
        .map(|(key, value)| {
            let mut value = value.clone();
            for (real, placeholder) in substitutions {
                if !real.is_empty() {
                    value = value.replace(real, placeholder);
                }
            }
            (key.clone(), value)
        })
        .collect();
    for name in base_url_overrides {
        match out.iter_mut().find(|(key, _)| key == name) {
            Some(entry) => entry.1 = base_url.to_owned(),
            None => out.push(((*name).to_owned(), base_url.to_owned())),
        }
    }
    out
}

/// What a set of placed file-credential placeholders adds to the launch: the container environment
/// pairs carrying each placeholder, and the argv fragment redirecting the client at the proxy.
///
/// A pure transform, deliberately, so the red-prove can assert the real credential is absent and the
/// redirect present without a container, a network, a proxy or a real key — the same reason
/// [`contain_env_values`] is pure.
///
/// **Every flag in `endpoint_args` gets its own `<flag> <base_url>` pair.** Emitting only the first
/// would contain only the endpoints we happened to name first and leave the rest reaching the vendor
/// with the placeholder — which authenticates nothing and fails the job while the proxy log stays
/// clean, because traffic that never arrives leaves no trace in it.
#[cfg(feature = "acp")]
fn file_credential_launch_additions(
    placed: &[(&crate::home::FileCredential, String)],
    base_url: &str,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut env = Vec::with_capacity(placed.len());
    let mut argv = Vec::new();
    for (cred, placeholder) in placed {
        env.push((cred.env.clone(), placeholder.clone()));
        for flag in &cred.endpoint_args {
            argv.push(flag.clone());
            argv.push(base_url.to_owned());
        }
    }
    (env, argv)
}

#[cfg(feature = "acp")]
fn classify_run_error(error: crate::engine::EngineError, timeout: AgentRunTimeout) -> ExecError {
    match (error, timeout) {
        (
            crate::engine::EngineError::Driver(crate::driver::DriverError::ResponseTimeout { .. }),
            AgentRunTimeout::JobDeadline(_),
        ) => ExecError::DeadlineExceeded,
        (error, _) => ExecError::Agent(error.to_string()),
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
    _timeout: AgentRunTimeout,
) -> Result<AgentRunReport, ExecError> {
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

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn job<'a>(workdir: &'a Path, env: &'a [(String, String)]) -> JobLaunch<'a> {
        JobLaunch {
            workdir,
            env,
            uid: 1000,
            gid: 1000,
            netns: None,
        }
    }

    /// Mirror of the downstream DASHBOARD harness-family classifier: a family substring wins;
    /// present-but-unrecognised (e.g. `npx`) falls through to `other`.
    ///
    /// ⚠ THIS IS NOT THE WIRE VOCABULARY. #784's `crate::agent_presets::HARNESS_FAMILIES` is a
    /// closed enum matched EXACTLY, with no catch-all, and it spells the Claude family
    /// `claude-code`. The two share a name and overlap in most tokens — see
    /// [`the_dashboard_family_vocabulary_is_not_the_wire_family_vocabulary`], which asserts exactly
    /// where they agree and where they diverge so the difference cannot be erased silently.
    ///
    /// ONE mirror, shared by both tests that need it. Two copies of a mirror can drift apart, and a
    /// drifted mirror would quietly stop reflecting the thing it exists to track.
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

    // The default (pass-through) policy launches the agent command exactly as configured: the
    // spawned `(program, args)` reconstruct the configured argv byte-for-byte, at the host workdir,
    // with no wrapper.
    #[test]
    fn passthrough_policy_launches_the_configured_command_byte_identical() {
        let agent_command = argv(&["claude", "--print", "--flag=a b"]);
        let policy = SandboxPolicy::passthrough();
        assert!(policy.is_passthrough());
        assert_eq!(SandboxPolicy::default(), policy);
        let workdir = Path::new("/srv/jobs/j1");
        let launch = policy.launch(&agent_command, &job(workdir, &[])).expect("non-empty command");
        // program = argv0, args = the rest — reconstructing the configured command exactly
        // (byte-identical to before the seam existed), run at the host workdir.
        assert_eq!(launch.program, agent_command[0]);
        assert_eq!(launch.args, agent_command[1..]);
        assert_eq!(
            std::iter::once(launch.program).chain(launch.args).collect::<Vec<_>>(),
            agent_command
        );
        assert_eq!(launch.cwd, workdir);
    }

    // A launcher policy runs the agent command INSIDE the launcher: argv0 becomes the launcher, the
    // configured command follows unchanged.
    #[test]
    fn launcher_policy_makes_the_launcher_argv0() {
        let agent_command = argv(&["claude", "--print"]);
        let launcher = argv(&["bwrap", "--unshare-all", "--"]);
        let policy = SandboxPolicy::wrapped(launcher.clone());
        assert!(!policy.is_passthrough());
        let launch = policy.launch(&agent_command, &job(Path::new("/w"), &[])).expect("command");
        assert_eq!(launch.program, launcher[0]);
        let spawned: Vec<String> = std::iter::once(launch.program).chain(launch.args).collect();
        let expected: Vec<String> = launcher.iter().chain(agent_command.iter()).cloned().collect();
        assert_eq!(spawned, expected);
    }

    // An empty agent command is a misconfig under every policy — a wrapper alone is not runnable.
    #[test]
    fn empty_agent_command_fails_closed() {
        let policy = SandboxPolicy::wrapped(argv(&["bwrap"]));
        let err = policy.launch(&[], &job(Path::new("/w"), &[])).expect_err("refused");
        assert!(matches!(err, ExecError::Config(_)));
    }

    /// Whether `arg` names the seller home DIRECTORY — `.maxplayer` occurring as a whole path
    /// component, under either anchoring: absolute (`/home/seller/.maxplayer/…`) or relative
    /// (`.maxplayer/…` at the start of the value).
    ///
    /// A component match and not a substring match, because the project's docker label namespace
    /// (`ai.maxplayer.job`) contains the same letters while naming no path at all. Terminators are
    /// `/`, `:` (a `-v src:dst` value ends the source there) and end-of-string.
    fn names_home_dir(arg: &str) -> bool {
        const HOME_COMPONENT: &str = ".maxplayer";
        let bytes = arg.as_bytes();
        let mut from = 0usize;
        while let Some(offset) = arg[from..].find(HOME_COMPONENT) {
            let at = from + offset;
            let end = at + HOME_COMPONENT.len();
            let starts_component = at == 0 || bytes[at - 1] == b'/';
            let ends_component =
                end == bytes.len() || bytes[end] == b'/' || bytes[end] == b':';
            if starts_component && ends_component {
                return true;
            }
            from = at + 1;
        }
        false
    }

    // A docker policy mounts ONLY the per-job workdir at the container mount point, so no host path
    // outside the workdir — $MAXPLAYER_HOME included — is reachable in the container by construction.
    #[test]
    fn docker_policy_mounts_only_the_job_workdir() {
        let agent_command = argv(&["claude-agent-acp"]);
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let env = vec![("GIT_AUTHOR_NAME".to_string(), "maxplayer-seller-abcd".to_string())];
        let workdir = Path::new("/home/seller/.maxplayer/seller-jobs/job1");
        let launch = policy.launch(&agent_command, &job(workdir, &env)).expect("command");

        assert_eq!(launch.program, "docker");
        // The ACP session runs at the in-container mount point, never the host path.
        assert_eq!(launch.cwd, Path::new(CONTAINER_WORKDIR));
        // Exactly one bind mount, and it is the job workdir → the container mount point.
        let mounts: Vec<&String> = launch
            .args
            .iter()
            .zip(launch.args.iter().skip(1))
            .filter(|(flag, _)| flag.as_str() == "-v")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(mounts, vec![&format!("{}:{CONTAINER_WORKDIR}", workdir.display())]);
        // The seller's home path never appears anywhere in the argv — not as a mount, not elsewhere.
        //
        // The needle tests `.maxplayer` as a PATH COMPONENT rather than as a substring, because a
        // path is the property and a substring is only a token that usually accompanies it. A bare
        // `contains(".maxplayer")` also matches this project's docker LABEL namespace
        // (`ai.maxplayer.…`, carried by both `JOB_LABEL` and `sandbox_netns::HOLDER_LABEL`), which
        // reaches no host filesystem; and anchoring it to `/.maxplayer` instead would trade that
        // false positive for a false NEGATIVE, silently dropping any leak that arrives as a relative
        // path. Component-matching keeps both anchorings and excludes the label by structure.
        let leaks = |args: &[String]| {
            args.iter().any(|a: &String| names_home_dir(a) && !a.contains("seller-jobs/job1"))
        };
        assert!(
            !leaks(&launch.args),
            "no host $MAXPLAYER_HOME path leaks into the container argv: {:?}",
            launch.args
        );
        // Controls in BOTH directions. The assertion above passes by matching nothing, which is also
        // exactly what a broken needle does — so the same predicate is shown firing on real leaks and
        // staying silent on the label that is not one.
        assert!(
            leaks(&[format!("/home/seller/.maxplayer:{CONTAINER_WORKDIR}")]),
            "an absolute host-home mount must be detected"
        );
        assert!(
            leaks(&[format!(".maxplayer/id_ed25519:{CONTAINER_WORKDIR}")]),
            "a RELATIVE host-home path must be detected too — anchoring the needle to a leading \
             slash would have missed this one"
        );
        assert!(
            !leaks(&[format!("{JOB_LABEL}=job1")]),
            "the docker label namespace is not a host path and must not be reported as a leak"
        );
        // Runs as the seller uid/gid and carries the delivery-identity env.
        assert!(windowed(&launch.args, &["--user", "1000:1000"]));
        assert!(windowed(&launch.args, &["-e", "GIT_AUTHOR_NAME=maxplayer-seller-abcd"]));
        // The image precedes the agent command, which is the final argv segment.
        assert_eq!(launch.args.last().map(String::as_str), Some("claude-agent-acp"));
    }

    fn docker_policy_for_probe() -> SandboxPolicy {
        SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:probe".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        })
    }

    /// A REAL throwaway directory, because `probe_launch_argv` refuses one that does not exist — and
    /// it refuses it for a measured reason (docker would create the bind source root-owned). A test
    /// passing a convenient `/w/probe` would be asserting against the refusal arm without noticing.
    struct ProbeDir(PathBuf);

    impl ProbeDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("maxplayer-probe-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&path).expect("lay out the throwaway probe workdir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ProbeDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // The guard on the workdir, both directions. Measured with a live daemon: a non-existent bind
    // source is created by docker as `uid=0 gid=0 mode=755`, and the container — running as the job's
    // uid — then cannot write its own workdir. A `--version` probe writes nothing and still exits 0,
    // so that misconfiguration is INVISIBLE in the result: the probe answers correctly while standing
    // in an environment no job would ever get.
    //
    // Both arms asserted, because a guard that only ever refuses is indistinguishable from a broken
    // renderer.
    #[test]
    fn a_probe_workdir_that_does_not_exist_is_refused_and_a_real_one_renders() {
        let policy = docker_policy_for_probe();
        let probe_command = argv(&["cargo", "--version"]);

        let missing = std::env::temp_dir()
            .join(format!("maxplayer-probe-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let refused = probe_launch_argv(&policy, &probe_command, &missing);
        assert!(
            matches!(refused, Err(ExecError::Config(_))),
            "a missing bind source must be refused, not passed to docker: {refused:?}"
        );

        // The positive control: the same call with a real directory renders.
        let dir = ProbeDir::new("guard");
        assert!(
            probe_launch_argv(&policy, &probe_command, dir.path()).is_ok(),
            "an existing workdir must render — otherwise the refusal above proves nothing"
        );
    }

    // #802, and the reason this seam exists: the two renderers disagree about docker, and only one of
    // them can reach a docker seat.
    //
    // TWO OPPOSING SIDES IN ONE TEST, deliberately. Asserting only that `probe_launch_argv` returns
    // an argv would pass just as well if `wrap` had quietly started returning one too — and then the
    // probe would have a host-argv fallback available again, which is the exact mistake this closes.
    // So the refusal is asserted beside the reach: `wrap` still yields NOTHING under docker, and the
    // probe path yields a real `docker run`.
    #[test]
    fn a_docker_policy_yields_no_host_argv_but_does_yield_a_probe_launch() {
        let policy = docker_policy_for_probe();
        let probe_command = argv(&["cargo", "--version"]);

        // The old seam. `None` is correct here and must stay correct: a caller with no job to launch
        // has no mount, uid or env to build a container from, so there is nothing safe to return.
        assert!(
            policy.wrap(&probe_command).is_none(),
            "wrap must keep refusing docker — a host argv here is the wrong-environment probe"
        );

        // The new seam, total over the executor the primary one actually is.
        let dir = ProbeDir::new("reach");
        let rendered = probe_launch_argv(&policy, &probe_command, dir.path())
            .expect("a docker policy renders a probe launch");
        assert_eq!(rendered.first().map(String::as_str), Some("docker"));
        assert!(windowed(
            &rendered,
            &["-v", &format!("{}:{CONTAINER_WORKDIR}", dir.path().display())]
        ));
        // The probe command is the trailing segment, after the image — so what runs inside the
        // container is the probe, not something the renderer appended.
        assert_eq!(
            rendered[rendered.len() - 2..],
            probe_command[..],
            "the probe command is the final argv segment: {rendered:?}"
        );
        assert!(
            rendered.contains(&"maxplayer-sandbox:probe".to_owned()),
            "the operator's real image, never a stand-in: {rendered:?}"
        );
    }

    // The probe carries a `--user`, and it is the awarded-job path's OWN identity expression rather
    // than a number written down twice.
    //
    // The sandbox image creates no user and sets no `USER`, so a `docker run` WITHOUT `--user` is
    // root — and a root probe can see and execute what a job cannot, advertising a capability the job
    // does not have. That failure passes every test someone would think to run and shows up only when
    // a buyer's sats are on it, so the flag's presence is asserted directly, not inferred.
    #[test]
    fn the_probe_runs_as_the_identity_an_awarded_job_gets() {
        let dir = ProbeDir::new("identity");
        let rendered = probe_launch_argv(
            &docker_policy_for_probe(),
            &argv(&["cargo", "--version"]),
            dir.path(),
        )
        .expect("renders");

        let (uid, gid) = job_identity();
        assert!(
            windowed(&rendered, &["--user", &format!("{uid}:{gid}")]),
            "the probe's --user must be job_identity() — the same call run_agent_job makes: {rendered:?}"
        );
        // Presence, separately from value: `job_identity()` legitimately returns (0, 0) when the
        // daemon runs as root, and then `--user 0:0` is the honest answer. What must never happen is
        // the flag being absent, because THAT is root-by-omission on any uid.
        assert!(
            rendered.iter().any(|part| part == "--user"),
            "a docker probe without --user is root, because the image sets no USER: {rendered:?}"
        );
    }

    // #357's blast radius, discharged structurally rather than promised.
    //
    // A second container-argv builder is how the job path and the probe path drift, and a bad
    // launcher argv does not fail one probe — it fails EVERY job the seat is offered. So the probe
    // constructs nothing: it calls the same `launch` the awarded-job path calls. This asserts that
    // consequence directly — for one policy and one workdir, the probe argv and a job argv are
    // identical up to the trailing command — which is only true if there is exactly one builder.
    //
    // The comparison is against a job with no env, because the probe deliberately carries none: it
    // asks whether a binary resolves, which needs no secret, so no secret is put where a container
    // could read it.
    #[test]
    fn the_probe_argv_and_a_job_argv_differ_only_in_the_trailing_command() {
        let policy = docker_policy_for_probe();
        let dir = ProbeDir::new("parity");
        let workdir = dir.path();
        let (uid, gid) = job_identity();

        let job_command = argv(&["claude-agent-acp"]);
        let job_launch = policy
            .launch(
                &job_command,
                &JobLaunch { workdir, env: &[], uid, gid, netns: None },
            )
            .expect("a job renders");
        let job_argv: Vec<String> =
            std::iter::once(job_launch.program).chain(job_launch.args).collect();

        let probe_command = argv(&["cargo", "--version"]);
        let probe_argv = probe_launch_argv(&policy, &probe_command, workdir).expect("renders");

        let job_prefix = &job_argv[..job_argv.len() - job_command.len()];
        let probe_prefix = &probe_argv[..probe_argv.len() - probe_command.len()];
        assert_eq!(
            job_prefix, probe_prefix,
            "every flag before the command must match the job path byte-for-byte — a difference here \
             is a second argv builder, and #357 is what that costs"
        );
        // And the only difference really is the command, so the assertion above is not vacuous.
        assert_ne!(job_argv, probe_argv);
    }

    // The honest false, measured in a REAL container rather than argued from a Dockerfile.
    //
    // `#[ignore]` rather than an env-var early-return: a test that returns early when its
    // precondition is missing reports as PASSED, and a green that cannot go red is worth less than
    // no test. Ignored reports as ignored. Run it with:
    //
    // ```text
    // MAXPLAYER_PROBE_IMAGE=<image> cargo test -p maxplayer-core --features wallet \
    //   seller_exec::tests::the_probe_answers_from_inside_the_container -- --ignored --nocapture
    // ```
    //
    // TWO OPPOSING CONTROLS from one image, which is what makes either of them mean anything:
    // `node` must be PROVEN and `cargo` must be REFUSED. Without the positive control a probe that
    // always returned false would pass; without the negative one, a probe answering from the host —
    // where cargo does resolve — would pass. Only the pair separates them.
    #[test]
    #[ignore = "runs a real container; needs a docker daemon and MAXPLAYER_PROBE_IMAGE"]
    fn the_probe_answers_from_inside_the_container_not_from_the_host() {
        let image = std::env::var("MAXPLAYER_PROBE_IMAGE").expect(
            "set MAXPLAYER_PROBE_IMAGE to the sandbox image to probe — this test measures a real \
             container and has nothing to say without one",
        );
        let policy = SandboxPolicy::docker(DockerPolicy {
            image,
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });

        // A REAL directory, for the reason `probe_launch_argv` refuses a missing one.
        let dir = ProbeDir::new("live");

        let container = probe_container_name(&policy, dir.path());
        let proves = |command: &[&str]| {
            let rendered = probe_launch_argv(&policy, &argv(command), dir.path())
                .expect("a docker policy renders a probe launch");
            probe_command_outcome(&rendered, dir.path(), CAPABILITY_PROBE_TIMEOUT, container.as_deref())
                .expect("the probe must be measurable — a docker daemon is present for this test")
        };

        let (uid, gid) = job_identity();
        let node = proves(&["node", "--version"]);
        let cargo = proves(&["cargo", "--version"]);

        // Printed so the report can state WHICH identity answered, not merely that one did.
        println!("probed as uid={uid} gid={gid}: node={node} cargo={cargo}");
        assert!(
            node,
            "the positive control failed — a probe that proves nothing proves nothing about absence \
             either, so the cargo leg below would be meaningless"
        );
        assert!(
            !cargo,
            "cargo resolved, which means this answered from the HOST: the runtime image carries no \
             rust toolchain (#358), so a true here is the wrong-environment probe, not a capability"
        );
    }

    // RED-PROVE: the capability probe must be BOUNDED. Before #784 the probe called an unbounded
    // `.status()`, so a stuck `--version` or a docker launch with no answer would hang the seller on
    // the pre-advertise path — before it ever serves. The assertion is on ELAPSED TIME, not only the
    // returned variant: a `TimedOut` that arrives after the caller already waited forever is not a fix.
    #[test]
    fn a_capability_probe_that_never_returns_is_bounded_and_killed() {
        let dir = ProbeDir::new("bounded");
        let argv = argv(&["sleep", "60"]);
        let started = Instant::now();
        let outcome = probe_command_outcome(&argv, dir.path(), Duration::from_millis(300), None);
        let elapsed = started.elapsed();

        assert!(
            matches!(outcome, Err(ProbeRunError::TimedOut { .. })),
            "a probe still running at its deadline must be a measurement FAILURE, never a silent \
             'not proven': {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the probe must RETURN at its deadline — waited {elapsed:?} for a 300ms bound"
        );
    }

    // The positive controls for the bound above: without them `probe_command_outcome` could return
    // `TimedOut`/`Ok(false)` unconditionally and the timeout test would still pass.
    #[test]
    fn a_bounded_probe_separates_success_absence_and_unmeasurable() {
        let dir = ProbeDir::new("separates");
        assert_eq!(
            probe_command_outcome(&argv(&["true"]), dir.path(), Duration::from_secs(10), None).ok(),
            Some(true),
            "a command that exits 0 is proven"
        );
        assert_eq!(
            probe_command_outcome(&argv(&["false"]), dir.path(), Duration::from_secs(10), None).ok(),
            Some(false),
            "a command that exits non-zero ran and is not proven — omit, do not error"
        );
        // Under a pass-through policy the probe program IS the target, so its absence is a clean
        // 'not proven', never an unmeasurable error.
        assert_eq!(
            probe_command_outcome(
                &argv(&["maxplayer-no-such-binary"]),
                dir.path(),
                Duration::from_secs(10),
                None,
            )
            .ok(),
            Some(false),
            "an absent bare probe binary is 'not proven' (Ok(false)), not a boot-failing error"
        );
        // Under docker the program is `docker`; a launcher that cannot spawn means the probe never
        // ran, which MUST be an error rather than a false 'not proven'. Simulated here with an absent
        // launcher name and a docker cleanup target.
        assert!(
            matches!(
                probe_command_outcome(
                    &argv(&["maxplayer-no-such-launcher"]),
                    dir.path(),
                    Duration::from_secs(10),
                    Some("maxplayer-job-probe-x"),
                ),
                Err(ProbeRunError::LauncherUnspawnable(_))
            ),
            "a docker launcher that cannot spawn is unmeasurable, never a silent 'no'"
        );
    }

    // RED-PROVE for the probe cwd: a host or launcher probe must run in the JOB's workdir, not
    // wherever the daemon happens to sit. Docker hides this — `-w` sets the container's cwd, so a
    // docker probe answers correctly even when the host child inherits the daemon's directory — and
    // that is exactly why the proof has to be built on a NON-docker policy.
    //
    // The probe command is a RELATIVE-path predicate, which is the only kind that can tell the two
    // directories apart: `test -f probe-marker` resolves against the child's cwd and nothing else.
    //
    // TWO OPPOSING CONTROLS, for the same reason the real-docker pair has them: the marker must be
    // FOUND in the directory we supply and MISSED in one we do not. Without the negative leg, a child
    // that ignored our cwd entirely and inherited the daemon's would still pass whenever the daemon
    // happened to sit somewhere with no marker — a green that cannot go red.
    #[test]
    fn a_probe_runs_in_the_supplied_workdir_not_the_daemon_cwd() {
        let supplied = ProbeDir::new("cwd-supplied");
        let other = ProbeDir::new("cwd-other");
        std::fs::write(supplied.path().join("probe-marker"), b"x").expect("plant the marker");

        let looks_for_marker = argv(&["test", "-f", "probe-marker"]);

        assert_eq!(
            probe_command_outcome(
                &looks_for_marker,
                supplied.path(),
                Duration::from_secs(10),
                None,
            )
            .ok(),
            Some(true),
            "the probe must run IN the supplied workdir — a relative path that exists there did not \
             resolve, so the child ran somewhere else"
        );
        assert_eq!(
            probe_command_outcome(&looks_for_marker, other.path(), Duration::from_secs(10), None)
                .ok(),
            Some(false),
            "the negative control failed: the same argv answered true from a directory with NO \
             marker, which means the cwd we pass is not the cwd the child gets"
        );
    }

    // Point ② of #784's required shape: the probe workdir is under the seller-jobs root, UNIQUE across
    // seats and boots, and removed on drop by RAII — not by a caller that must remember.
    #[test]
    fn the_probe_workdir_is_unique_under_seller_jobs_and_removed_on_drop() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("mp-probe-home-{}-{nanos}", std::process::id()));
        let home = crate::home::bootstrap(&root).expect("bootstrap a test home");

        let first_path;
        let second_path;
        {
            let first = ProbeWorkdir::create(&home).expect("create the first probe workdir");
            let second = ProbeWorkdir::create(&home).expect("create the second probe workdir");
            first_path = first.path().to_path_buf();
            second_path = second.path().to_path_buf();

            assert!(first.path().is_dir(), "the probe workdir must exist while held");
            assert!(second.path().is_dir(), "the probe workdir must exist while held");
            assert_ne!(
                first.path(),
                second.path(),
                "two probe workdirs must not collide — the leaf also names the docker container"
            );
            for path in [first.path(), second.path()] {
                assert_eq!(
                    path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()),
                    Some("seller-jobs"),
                    "the probe workdir must live under the seller-jobs root, where jobs run: {path:?}"
                );
            }
        }
        assert!(!first_path.exists(), "drop must remove the probe workdir: {first_path:?}");
        assert!(!second_path.exists(), "drop must remove the probe workdir: {second_path:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // RED-PROVE: the job container must NOT carry `--rm`.
    //
    // `--rm` removes on container EXIT, so on a job that SUCCEEDS docker deletes the container before
    // `capture_then_remove` can read it. Capture fails, removal reports success, and `evidence_lost()`
    // is true by definition — on the happy path, every run. `capture_then_remove` documents that
    // evidence comes first and "the order is the whole point"; `--rm` is the single flag that makes
    // that order unenforceable from outside the process.
    //
    // Removal is owned by `capture_then_remove` and by `JobContainer`'s `Drop` fallback, both of which
    // remove BY NAME after capturing — so `--name` is asserted here too. Without it nothing downstream
    // can address THIS container, and dropping `--rm` really would leak.
    #[test]
    fn docker_policy_keeps_the_container_alive_for_its_own_diagnostics() {
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");

        assert!(
            !launch.args.iter().any(|a| a == "--rm"),
            "`--rm` deletes the container on exit, which on a PASSING job destroys the diagnostics \
             `capture_then_remove` exists to save: {:?}",
            launch.args
        );
        // Positive control: without a deterministic name, removing `--rm` would be a real leak rather
        // than a deferred, attributable removal. An assertion that only forbids `--rm` cannot see that.
        assert!(
            launch.args.iter().any(|a| a == "--name"),
            "the container must stay addressable by name so capture and removal can target THIS one: \
             {:?}",
            launch.args
        );
    }

    // The container runs a STRANGER'S code, and two docker defaults are wrong for that. Measured
    // against a live daemon: without `--security-opt no-new-privileges` the container reports
    // `NoNewPrivs: 0`, and `node:22-bookworm-slim` ships 8 setuid-root binaries (su, mount, umount
    // among them) — so a job may attempt to become container-root. Without `--init`, PID 1 is the
    // adapter itself (a node process that never reaps), so a job's subprocesses accumulate as
    // zombies; with it PID 1 is `docker-init`.
    //
    // Neither changes what the job can reach OUTSIDE the container — the host tree is unmounted
    // either way — so this pins hardening, not the containment claim, which
    // `docker_policy_mounts_only_the_job_workdir` owns.
    #[test]
    fn docker_policy_hardens_against_the_strangers_code_it_runs() {
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");

        assert!(
            windowed(&launch.args, &["--security-opt", "no-new-privileges"]),
            "a setuid-root binary in the image must not be a route to container-root: {:?}",
            launch.args
        );
        assert!(
            windowed(&launch.args, &["--cap-drop", "ALL"]),
            "the bounding capability set must be empty, so no capability can be regained: {:?}",
            launch.args
        );
        assert!(
            launch.args.iter().any(|a| a == "--init"),
            "PID 1 must reap, or a job's subprocesses pile up as zombies: {:?}",
            launch.args
        );
        // Both must precede the image, or docker reads them as arguments to the agent command.
        let image_at = launch
            .args
            .iter()
            .position(|a| a == "maxplayer-sandbox:latest")
            .expect("the image is in the argv");
        let init_at = launch.args.iter().position(|a| a == "--init").expect("--init");
        let secopt_at = launch
            .args
            .iter()
            .position(|a| a == "--security-opt")
            .expect("--security-opt");
        assert!(
            init_at < image_at && secopt_at < image_at,
            "hardening flags after the image would be passed to the agent, not to docker: {:?}",
            launch.args
        );
    }

    // A job container that nothing can address is a job container nothing can capture. `docker run`
    // without `--name`, `--label` or `--cidfile` yields a handle only on stdout of a `-d` run — and
    // this run is `-i`, so there is no id to read. The launch must therefore carry a DETERMINISTIC
    // name derived from the job, exactly as the netns holder does (`maxplayer-netns-{job_id}`), so a
    // leaked container can be attributed to its job instead of merely noticed.
    //
    // Both must precede the image, or docker reads them as arguments to the agent command.
    #[test]
    fn job_container_carries_a_deterministic_name_and_label() {
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");

        assert!(
            windowed(&launch.args, &["--name", "maxplayer-job-w"]),
            "the job container must be addressable by a deterministic name, or nothing can capture \
             its diagnostics before removing it: {:?}",
            launch.args
        );
        assert!(
            windowed(&launch.args, &["--label", "ai.maxplayer.job=w"]),
            "the job container must carry a job label, so a leaked one can be attributed: {:?}",
            launch.args
        );
        let image_at = launch
            .args
            .iter()
            .position(|a| a == "maxplayer-sandbox:latest")
            .expect("the image is in the argv");
        let name_at = launch.args.iter().position(|a| a == "--name").expect("--name");
        let label_at = launch.args.iter().position(|a| a == "--label").expect("--label");
        assert!(
            name_at < image_at && label_at < image_at,
            "an addressing flag after the image would be passed to the agent, not to docker: {:?}",
            launch.args
        );
    }

    // ---- failed-job cleanup and evidence capture ----

    static NEXT_CAPTURE_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn capture_dir(label: &str) -> PathBuf {
        let id = NEXT_CAPTURE_DIR.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "maxplayer-capture-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A `docker` stand-in that RECORDS what it was asked to do, in order.
    ///
    /// The sequencing property here cannot be checked by reading the source: block order in a
    /// function is not evidence that execution followed it. So the calls are recorded and the order
    /// asserted on what actually ran. `fail_on` makes one verb fail, for the capture-failure case.
    struct RecordingDocker {
        /// The first word after `docker` for each call, in call order.
        verbs: Vec<String>,
        /// Whether the capture files existed on disk at the instant `rm` was requested. `None` until
        /// `rm` is asked for. This is the real ordering evidence: argv order alone would not prove the
        /// bytes had landed.
        files_present_at_removal: Option<bool>,
        /// The directory capture writes into, so the check above can look for the files.
        dir: PathBuf,
        /// A verb that returns an error instead of output.
        fail_on: Option<&'static str>,
        /// Output handed back for `logs`, so a test can plant a credential in it.
        logs_output: String,
    }

    impl RecordingDocker {
        fn new(dir: &Path) -> Self {
            Self {
                verbs: Vec::new(),
                files_present_at_removal: None,
                dir: dir.to_path_buf(),
                fail_on: None,
                logs_output: "job line one\njob line two".into(),
            }
        }
    }

    impl DockerCli for RecordingDocker {
        fn run(&mut self, argv: &[String]) -> Result<String, String> {
            let verb = argv.get(1).cloned().unwrap_or_default();
            if verb == "rm" {
                self.files_present_at_removal =
                    Some(self.dir.join("inspect.txt").exists() && self.dir.join("logs.txt").exists());
            }
            self.verbs.push(verb.clone());
            if self.fail_on == Some(verb.as_str()) {
                return Err(format!("{verb} refused"));
            }
            match verb.as_str() {
                "logs" => Ok(self.logs_output.clone()),
                _ => Ok("status=exited exit_code=137 oom_killed=false".into()),
            }
        }
    }

    // THE ordering property, and the one the whole task exists for: the evidence must be on disk
    // BEFORE the container is removed. A timeout is the case that matters — the driver kills the
    // `docker run` client, `--rm` never fires, the container survives, and whatever removes it next
    // takes `docker logs` and `docker inspect` with it.
    //
    // Asserted on RECORDED calls, not on block order: the fake reports whether the capture files
    // existed at the instant `rm` was asked for, so a reordering that still "looks" evidence-first in
    // source would fail here.
    #[test]
    fn capture_is_written_before_the_container_is_removed() {
        let dir = capture_dir("order");
        let mut cli = RecordingDocker::new(&dir);
        let report = capture_then_remove(&mut cli, "maxplayer-job-j1", &dir, None, &[]);

        assert!(report.capture.is_ok(), "capture: {:?}", report.capture);
        assert!(report.removal.is_ok(), "removal: {:?}", report.removal);
        let rm_at = cli.verbs.iter().position(|v| v == "rm").expect("rm was issued");
        let inspect_at = cli.verbs.iter().position(|v| v == "inspect").expect("inspect was issued");
        let logs_at = cli.verbs.iter().position(|v| v == "logs").expect("logs were read");
        assert!(
            inspect_at < rm_at && logs_at < rm_at,
            "every capture call must precede the removal: {:?}",
            cli.verbs
        );
        assert_eq!(
            cli.files_present_at_removal,
            Some(true),
            "the capture files must already be on disk when removal is requested, not merely \
             requested earlier: {:?}",
            cli.verbs
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A capture that fails must not read as a clean cleanup. The container is still removed — leaving
    // it would leak a running container AND collide with the next attempt on this job id — so the
    // only protection against a silent orphan is that the two legs are reported separately and the bad
    // pair is nameable.
    #[test]
    fn a_capture_failure_is_reported_and_never_a_silent_orphan() {
        let dir = capture_dir("failed");
        let mut cli = RecordingDocker::new(&dir);
        cli.fail_on = Some("inspect");
        let report = capture_then_remove(&mut cli, "maxplayer-job-j2", &dir, None, &[]);

        assert!(
            report.capture.is_err(),
            "a failed inspect must be reported as a capture failure, not swallowed: {:?}",
            report.capture
        );
        assert!(
            report.removal.is_ok(),
            "removal must still be attempted, or one failure becomes two: {:?}",
            report.removal
        );
        assert!(
            cli.verbs.iter().any(|v| v == "rm"),
            "the container must still be removed: {:?}",
            cli.verbs
        );
        assert!(
            report.evidence_lost(),
            "container removed with no evidence saved is the one state a caller must be able to \
             SEE, rather than infer from two fields"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Nothing a capture writes may retain a credential. Three routes, three checks — because a
    // redactor that only knows patterns answers "clean" identically for text it simply does not
    // recognise.
    //
    // The structural half is the important one: `docker inspect` must never be asked for
    // `Config.Env`. On a run without credential containment that field holds the real
    // `ANTHROPIC_API_KEY` this module puts there with `-e`, so a bare inspect would persist a live
    // credential to a file. Absent by construction beats scrubbed afterwards.
    #[test]
    fn capture_retains_no_credential_header_or_token() {
        let format = inspect_argv("maxplayer-job-j3").join(" ");
        assert!(
            !format.contains("Config.Env") && !format.contains(".Env"),
            "a field-selected inspect must never request the container environment: {format}"
        );

        let secret = "sk-ant-api03-REALKEYMATERIALREALKEYMATERIAL";
        let text = format!(
            "starting up\nAuthorization: Bearer {secret}\nx-api-key: {secret}\nbody: {{\"key\":\"{secret}\"}}\nplain sk-short line"
        );
        let clean = redact(&text, &[secret.to_string()]);

        assert!(
            !clean.contains(secret),
            "the exact forwarded credential value must not survive redaction: {clean}"
        );
        assert!(
            !clean.contains("Bearer"),
            "a sensitive header's VALUE must go, not just the token inside it: {clean}"
        );
        assert!(
            clean.contains("starting up"),
            "redaction must leave the diagnostic text it exists to preserve: {clean}"
        );
        // A short `sk-` run in ordinary prose is not a token and must survive, or every capture file
        // becomes unreadable noise.
        assert!(
            clean.contains("sk-short"),
            "a short sk- run is prose, not a credential: {clean}"
        );
        // The pattern backstop, with NO value supplied — a credential the daemon never held.
        let unlisted = redact("token=sk-ant-api03-UNLISTEDBUTSTILLSECRET", &[]);
        assert!(
            !unlisted.contains("UNLISTEDBUTSTILLSECRET"),
            "a token shape must be caught even when its value was never forwarded: {unlisted}"
        );
    }

    // The job container and its netns holder are TWO per-job docker objects, and each must be
    // removable by its own exact name. One teardown story, two objects: the holder keeps the
    // namespace (its own `Drop` owns that, and this change does not touch it), and the job container
    // is this module's.
    //
    // ⚠ Bound, stated rather than implied: this asserts both removals are ISSUED against distinct
    // exact names. It does not observe docker, so it is not evidence that either container actually
    // disappeared — that needs a live run, which is gated until capture is active.
    #[test]
    fn job_and_holder_are_distinct_objects_removed_by_exact_name() {
        let job = job_container_name("j4");
        let holder = crate::sandbox_netns::holder_name("j4");
        assert_ne!(
            job, holder,
            "one name for both objects would make removing either ambiguous"
        );

        let remove_job = force_remove_argv(&job);
        assert_eq!(remove_job.last().map(String::as_str), Some(job.as_str()));
        assert!(
            remove_job.iter().any(|a| a == "--force"),
            "the container this exists for is still RUNNING — a plain rm would refuse: {remove_job:?}"
        );
        assert!(
            !remove_job.iter().any(|a| a == holder.as_str()),
            "removing the job container must not name the holder: {remove_job:?}"
        );
    }

    // `Drop` is the fallback for an unwind or an early return, and it is REMOVE-ONLY on purpose:
    // `NetnsHolder`'s own `Drop` states that failure there is logged and never propagated, so a
    // capture placed in a `Drop` is a guard whose failure mode is silence sitting over the evidence
    // itself. What `Drop` must do instead is say it skipped capture — otherwise "no diagnostics" is
    // ambiguous between *captured and found nothing* and *removed before anything captured it*.
    #[test]
    fn drop_fallback_is_remove_only_and_settling_disarms_it() {
        // Unsettled: an unwind or an early return reached the guard with no explicit cleanup. It must
        // SAY it skipped capture and then remove — never remove silently.
        assert_eq!(
            drop_fallback(false),
            DropFallback::ReportSkippedThenRemove,
            "an unsettled guard must report the skipped capture before removing, or 'no diagnostics' \
             is ambiguous between captured-and-found-nothing and removed-before-capture"
        );
        // Settled: the explicit evidence-first path ran, so the fallback must issue nothing rather
        // than report a skip that did not happen.
        assert_eq!(
            drop_fallback(true),
            DropFallback::Nothing,
            "a settled guard must not also run the fallback"
        );
        // The marker is an operator-facing interface — it is what gets grepped when a job has no
        // diagnostics — so its exact text is pinned.
        assert_eq!(CAPTURE_SKIPPED_MARKER, "capture_skipped=drop_fallback");

        let mut container = JobContainer::adopt(job_container_name("j5"));
        assert_eq!(container.name(), "maxplayer-job-j5");
        container.settle();
        drop(container);
    }

    // The evidence must not land where the job being diagnosed can rewrite it. The workdir is the ONE
    // host path bind-mounted into the container, so diagnostics go to a sibling directory instead —
    // and the inversion of `job_workdir` is checked rather than assumed, because a wrong directory
    // scatters diagnostics somewhere nobody looks, which reads exactly like a capture that never ran.
    #[test]
    fn diagnostics_land_outside_the_bind_mounted_workdir() {
        let home = Path::new("/home/seat/.maxplayer");
        let workdir = home.join("seller-jobs").join("job-7");
        let dir = job_diagnostics_dir(&workdir).expect("a job_workdir inverts");
        // The containment property FIRST, and on its own line, so it can fail by itself. Asserted
        // before the exact path because it is the load-bearing one: a capture written inside the
        // bind mount could be rewritten by the very job it indicts.
        assert!(
            !dir.starts_with(&workdir),
            "diagnostics inside the mount could be rewritten by the job they indict: {}",
            dir.display()
        );
        assert_eq!(dir, home.join("seller-diagnostics").join("job-7"));
        // Not a path `job_workdir` built ⇒ no guess.
        assert_eq!(job_diagnostics_dir(Path::new("/tmp/elsewhere/job-7")), None);
    }

    // The v1 posture runs the job under gVisor on Linux by naming a container runtime. Unset ⇒ the
    // argv carries no `--runtime` (the daemon default, `runc`); set ⇒ `docker run --runtime <name>`,
    // and it must precede the image or docker reads it as the agent command. A Mac seat is the unset
    // case — the platform VM is its boundary, so no runtime override is named.
    #[test]
    fn docker_runtime_is_named_only_when_configured_and_precedes_the_image() {
        // Unset: no --runtime anywhere.
        let default_rt = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let launch = default_rt
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");
        assert!(
            !launch.args.iter().any(|a| a == "--runtime"),
            "an unset runtime must not emit a --runtime flag: {:?}",
            launch.args
        );

        // Set: --runtime runsc, before the image.
        let gvisor = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: Some("runsc".into()),
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let launch = gvisor
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");
        assert!(
            windowed(&launch.args, &["--runtime", "runsc"]),
            "a configured runtime must reach the argv: {:?}",
            launch.args
        );
        let runtime_at =
            launch.args.iter().position(|a| a == "runsc").expect("runtime value in argv");
        let image_at = launch
            .args
            .iter()
            .position(|a| a == "maxplayer-sandbox:latest")
            .expect("the image is in the argv");
        assert!(
            runtime_at < image_at,
            "the runtime must precede the image, or docker reads it as the agent command: {:?}",
            launch.args
        );
    }

    // The #797 sandbox network reaches the argv when configured, and is absent when not. The
    // absent case is the one worth pinning: the flag must not appear at all, because an empty
    // `--network ` is a docker error and a defaulted one would silently move every existing seat
    // off the network its containers run on today.
    #[test]
    fn a_configured_sandbox_network_reaches_the_argv_and_an_unset_one_emits_no_flag() {
        let unset = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let launch = unset
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");
        assert!(
            !launch.args.iter().any(|a| a == "--network"),
            "an unset network must not emit a --network flag: {:?}",
            launch.args
        );

        let joined = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: Some("maxplayer-sbx".into()),
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let launch = joined
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");
        assert!(
            windowed(&launch.args, &["--network", "maxplayer-sbx"]),
            "a configured network must reach the argv: {:?}",
            launch.args
        );
        // Same ordering hazard as `--runtime`: a run flag after the image is read as the agent
        // command, so the container would launch with `--network` as its argv0.
        let network_at =
            launch.args.iter().position(|a| a == "maxplayer-sbx").expect("network value in argv");
        let image_at = launch
            .args
            .iter()
            .position(|a| a == "maxplayer-sandbox:latest")
            .expect("the image is in the argv");
        assert!(
            network_at < image_at,
            "the network must precede the image, or docker reads it as the agent command: {:?}",
            launch.args
        );
    }

    // from_config threads the network and the proxy port range through, and refuses a malformed
    // range at config-resolution time rather than at job time.
    #[test]
    fn from_config_resolves_the_network_and_the_proxy_port_range() {
        use crate::home::{SandboxConfig, SandboxMode};
        let configured = SandboxConfig {
            mode: SandboxMode::Docker,
            image: Some("img".into()),
            network: Some("maxplayer-sbx".into()),
            proxy_port_range: Some("49200-49299".into()),
            ..Default::default()
        };
        let policy = SandboxPolicy::from_config(Some(&configured)).expect("ok");
        assert_eq!(policy.sandbox_network(), Some("maxplayer-sbx"));
        assert_eq!(
            policy.proxy_ports(),
            Some(crate::sandbox_net::PortRange::new(49200, 49299).unwrap())
        );

        // Unset ⇒ both absent. This is the shipped behaviour and it must survive the new fields.
        let bare =
            SandboxConfig { mode: SandboxMode::Docker, image: Some("img".into()), ..Default::default() };
        let policy = SandboxPolicy::from_config(Some(&bare)).expect("ok");
        assert_eq!(policy.sandbox_network(), None);
        assert_eq!(policy.proxy_ports(), None);

        // Blank strings are unset, not an empty flag and not a parse error.
        let blank = SandboxConfig {
            mode: SandboxMode::Docker,
            image: Some("img".into()),
            network: Some("   ".into()),
            proxy_port_range: Some("  ".into()),
            ..Default::default()
        };
        let policy = SandboxPolicy::from_config(Some(&blank)).expect("blank is unset, not invalid");
        assert_eq!(policy.sandbox_network(), None);
        assert_eq!(policy.proxy_ports(), None);

        // A malformed range FAILS resolution. The alternative — falling back to an ephemeral port —
        // would put the proxy outside the range the firewall pinhole names, so every job would fail
        // to reach its model with nothing naming the port as the cause.
        let bad = SandboxConfig {
            mode: SandboxMode::Docker,
            image: Some("img".into()),
            proxy_port_range: Some("49300-49200".into()),
            ..Default::default()
        };
        let error = SandboxPolicy::from_config(Some(&bad)).expect_err("an inverted range must fail");
        assert!(
            matches!(&error, ExecError::Config(message) if message.contains("proxy_port_range")),
            "the refusal must name the config key an operator has to fix: {error:?}"
        );
    }

    // from_config threads the runtime through, and a blank string is treated as unset rather than
    // forwarded as an empty `--runtime ` that docker would reject.
    #[test]
    fn from_config_resolves_and_trims_the_runtime() {
        use crate::home::{SandboxConfig, SandboxMode};
        let with_runtime = SandboxConfig {
            mode: SandboxMode::Docker,
            image: Some("img".into()),
            runtime: Some("runsc".into()),
            ..Default::default()
        };
        let policy = SandboxPolicy::from_config(Some(&with_runtime)).expect("ok");
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");
        assert!(windowed(&launch.args, &["--runtime", "runsc"]), "{:?}", launch.args);

        // A blank runtime is a config no-op, not an empty flag.
        let blank = SandboxConfig { runtime: Some("  ".into()), ..with_runtime };
        let policy = SandboxPolicy::from_config(Some(&blank)).expect("ok");
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");
        assert!(
            !launch.args.iter().any(|a| a == "--runtime"),
            "a blank runtime must not emit a --runtime flag: {:?}",
            launch.args
        );
    }

    // A container inherits NOTHING from the daemon, so an agent CLI inside it has no credential
    // unless one is carried in. This is the allowlist that carries it — and the reason it is an
    // allowlist rather than "forward the environment" is that the container runs a stranger's code:
    // every variable that crosses is one the job can read. The whole-environment case below is the
    // one that must never regress.
    #[test]
    fn only_allowlisted_agent_auth_env_crosses_into_the_container() {
        let daemon_env = |key: &str| -> Option<String> {
            match key {
                "ANTHROPIC_API_KEY" => Some("sk-ant-xxx".to_owned()),
                "OPENAI_API_KEY" => Some("sk-oai-xxx".to_owned()),
                // Set in the daemon, NOT on the allowlist, and must not cross.
                "AWS_SECRET_ACCESS_KEY" => Some("the-seat's-other-secret".to_owned()),
                "MAXPLAYER_HOME" => Some("/home/seller/.maxplayer".to_owned()),
                _ => None,
            }
        };
        let docker = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let carried = forwarded_agent_env_from(&docker, daemon_env);
        let names: Vec<&str> = carried.iter().map(|(k, _)| k.as_str()).collect();

        assert!(names.contains(&"ANTHROPIC_API_KEY") && names.contains(&"OPENAI_API_KEY"));
        assert!(
            !names.contains(&"AWS_SECRET_ACCESS_KEY"),
            "an unrelated daemon secret must never reach a stranger's job: {names:?}"
        );
        assert!(
            !names.contains(&"MAXPLAYER_HOME"),
            "the seat's own paths are not the agent's business: {names:?}"
        );
        // An allowlisted name that is UNSET must not become `-e FOO=`, which would blank out a value
        // an operator baked into their image.
        assert!(
            !names.contains(&"ANTHROPIC_AUTH_TOKEN"),
            "an unset variable must not be forwarded empty: {carried:?}"
        );
    }

    fn file_cred() -> crate::home::FileCredential {
        crate::home::FileCredential {
            path: PathBuf::from("/home/seller/.config/cursor/auth.json"),
            field: "accessToken".into(),
            env: "CURSOR_AUTH_TOKEN".into(),
            upstream: "https://api2.cursor.sh".into(),
            endpoint_args: vec!["--endpoint".into()],
        }
    }

    fn docker_with(file_credentials: Vec<crate::home::FileCredential>) -> crate::home::SandboxConfig {
        crate::home::SandboxConfig {
            mode: crate::home::SandboxMode::Docker,
            file_credentials,
            ..Default::default()
        }
    }

    // A relative path is REFUSED at config resolution, not resolved against whatever cwd the daemon
    // happens to have. A daemon started by systemd need not share the operator's `$HOME`, so silently
    // resolving one would present as an auth failure inside the job with nothing naming the path.
    #[test]
    fn a_file_credential_path_must_be_absolute() {
        let relative = crate::home::FileCredential {
            path: PathBuf::from(".config/cursor/auth.json"),
            ..file_cred()
        };
        let error = SandboxPolicy::from_config(Some(&docker_with(vec![relative])))
            .expect_err("a relative path must be refused");
        let message = error.to_string();
        assert!(
            message.contains("must be absolute") && message.contains(".config/cursor/auth.json"),
            "the error must name the offending path: {message}"
        );
        SandboxPolicy::from_config(Some(&docker_with(vec![file_cred()])))
            .expect("an absolute path resolves");
    }

    // Every field is load-bearing, so a blank one is a config error rather than a silent no-op: a
    // blank endpoint flag would leave the client talking to the vendor, and a blank `env` would put
    // the placeholder nowhere — both presenting as an unexplained auth failure per job.
    #[test]
    fn a_file_credential_refuses_blank_fields_and_a_malformed_upstream() {
        for (label, cred) in [
            ("field", crate::home::FileCredential { field: "  ".into(), ..file_cred() }),
            ("env", crate::home::FileCredential { env: String::new(), ..file_cred() }),
            ("upstream", crate::home::FileCredential { upstream: " ".into(), ..file_cred() }),
            (
                "endpoint_args",
                crate::home::FileCredential { endpoint_args: vec!["".into()], ..file_cred() },
            ),
            (
                "endpoint_args",
                crate::home::FileCredential { endpoint_args: Vec::new(), ..file_cred() },
            ),
        ] {
            let error = SandboxPolicy::from_config(Some(&docker_with(vec![cred])))
                .expect_err("a blank {label} must be refused");
            assert!(
                error.to_string().contains(label),
                "the error must name which field was blank, got: {error}"
            );
        }
        let bad_upstream = crate::home::FileCredential {
            upstream: "api2.cursor.sh".into(), // no scheme
            ..file_cred()
        };
        let error = SandboxPolicy::from_config(Some(&docker_with(vec![bad_upstream])))
            .expect_err("an upstream with no scheme must be refused");
        assert!(error.to_string().contains("not a valid URL"), "{error}");
    }

    // The value is read from the file at call time. The failure messages are asserted to name the
    // FIELD and never a value: this file holds a live credential, so an error that quoted its content
    // would write the secret into a log the operator never chose to expose.
    #[cfg(feature = "acp")]
    #[test]
    fn read_file_credential_takes_the_named_field_and_no_error_quotes_a_value() {
        let dir = std::env::temp_dir().join(format!("mp852-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("auth.json");

        std::fs::write(
            &path,
            br#"{"accessToken":"the-real-one","refreshToken":"the-refresh-one"}"#,
        )
        .expect("write");
        let cred = crate::home::FileCredential { path: path.clone(), ..file_cred() };
        assert_eq!(read_file_credential(&cred).expect("reads"), "the-real-one");

        // The neighbouring refresh token is never read, which is what bounds a leaked placeholder to
        // one job: only the named field is ever substituted.
        let refresh_only =
            crate::home::FileCredential { field: "nonexistent".into(), ..cred.clone() };
        let error = read_file_credential(&refresh_only).expect_err("missing field");
        let message = error.to_string();
        assert!(message.contains("nonexistent"), "must name the field: {message}");
        assert!(
            !message.contains("the-real-one") && !message.contains("the-refresh-one"),
            "an error must never quote a credential value: {message}"
        );

        std::fs::write(&path, b"{not json").expect("write");
        let error = read_file_credential(&cred).expect_err("malformed json");
        let message = error.to_string();
        assert!(message.contains("not valid JSON"), "{message}");
        assert!(
            !message.contains("not json"),
            "a parse error must report position only, never content: {message}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // RED-PROVE. The container sees a PLACEHOLDER and the redirect flag; the real credential appears
    // in neither, including inside an unrelated forwarded variable that happens to carry the same
    // secret. Pure transforms, so this needs no container, network, proxy or real key.
    #[cfg(feature = "acp")]
    #[test]
    fn a_file_credential_gives_the_container_a_placeholder_and_redirects_by_argv() {
        const REAL: &str = "REAL-CURSOR-SESSION-SENTINEL";
        let base_url = "http://host.docker.internal:41111";
        let cred = file_cred();
        let placeholder = crate::credential_proxy::mint_jwt_placeholder(
            "session",
            std::time::Duration::from_secs(600),
        );

        // The same value-based scrub the env-sourced entries get: a file-sourced secret that also
        // appears in a forwarded variable is replaced there too.
        let forwarded = vec![("SOME_OPERATOR_VAR".to_owned(), format!("prefix {REAL} suffix"))];
        let substitutions = vec![(REAL.to_owned(), placeholder.clone())];
        let mut view = contain_env_values(&forwarded, &substitutions, &[], base_url);
        let (file_env, argv_extra) =
            file_credential_launch_additions(&[(&cred, placeholder.clone())], base_url);
        view.extend(file_env);

        assert!(
            view.iter().all(|(_, value)| !value.contains(REAL)),
            "the real credential reached the container view: {view:?}"
        );
        assert!(
            view.contains(&("CURSOR_AUTH_TOKEN".to_owned(), placeholder.clone())),
            "the placeholder must arrive in the variable the operator named: {view:?}"
        );
        assert_eq!(
            argv_extra,
            vec!["--endpoint".to_owned(), base_url.to_owned()],
            "the client is redirected by ITS OWN flag, because its base-URL env vars are ignored for \
             credential traffic"
        );
        // The client PARSES this value, so a placeholder that is not a well-formed JWT would be
        // refused locally and never reach the wire — presenting as "containment broke the seat".
        assert_eq!(placeholder.split('.').count(), 3, "placeholder must be a JWT: {placeholder}");
        assert!(
            !placeholder.contains('=') && !placeholder.contains('+') && !placeholder.contains('/'),
            "segments must be unpadded base64url: {placeholder}"
        );
    }

    // Containment now has TWO registries, and the auditor must enumerate both. A predicate over only
    // the built-in table would report a file-sourced credential as crossing UNCONTAINED — and, worse,
    // would let the doctor emit a completeness claim covering a registry it cannot see.
    //
    // Constructed directly rather than through `from_config`, which refuses this combination outright
    // (see below): the point here is the PREDICATE, so it is exercised where config validation cannot
    // mask it.
    #[test]
    fn the_uncontained_audit_counts_both_registries_not_just_the_table() {
        let cred = file_cred();
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: vec!["CURSOR_AUTH_TOKEN".into(), "MY_AGENT_TOKEN".into()],
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: vec![cred],
        });
        let uncontained =
            uncontained_forwarded_credentials(&policy, |_| Some("set-to-something".to_owned()));
        assert!(
            !uncontained.contains(&"CURSOR_AUTH_TOKEN".to_owned()),
            "a file-sourced credential IS contained and must not be reported as leaking: \
             {uncontained:?}"
        );
        assert!(
            uncontained.contains(&"MY_AGENT_TOKEN".to_owned()),
            "an unrecognized forwarded variable must still be flagged, or this check has stopped \
             discriminating: {uncontained:?}"
        );
    }

    // Forwarding the same variable the placeholder occupies is refused, not merged. Both would arrive
    // as `-e NAME=…` and only one can win, so behaviour would rest on argument order; and a daemon
    // copy that DIFFERS from the file's (a stale export beside a re-logged-in file) is not in the
    // substitution set and would cross RAW.
    #[test]
    fn a_file_credential_variable_may_not_also_be_forwarded() {
        let mut config = docker_with(vec![file_cred()]);
        config.forward_env = vec!["CURSOR_AUTH_TOKEN".into()];
        let error = SandboxPolicy::from_config(Some(&config))
            .expect_err("the same variable from two sources must be refused");
        let message = error.to_string();
        assert!(
            message.contains("CURSOR_AUTH_TOKEN") && message.contains("also forwarded"),
            "the error must name the collision: {message}"
        );

        // A built-in allowlist name collides too — the allowlist forwards it without the operator
        // naming it, so the collision is invisible from `forward_env` alone.
        let mut builtin = docker_with(vec![crate::home::FileCredential {
            env: "ANTHROPIC_API_KEY".into(),
            ..file_cred()
        }]);
        builtin.forward_env = Vec::new();
        SandboxPolicy::from_config(Some(&builtin))
            .expect_err("a built-in allowlist name must collide too");

        // And the non-colliding case still resolves, so the guard is not refusing everything.
        SandboxPolicy::from_config(Some(&docker_with(vec![file_cred()])))
            .expect("a file credential with its own variable resolves");
    }

    // Two entries claiming one variable, refused at boot. Neither value is real, so this is
    // fail-visible rather than a leak — but the visible failure is a per-job auth rejection whose
    // cause is a config typo, and boot is where the operator is still looking at the config.
    #[test]
    fn two_file_credentials_may_not_claim_the_same_variable() {
        let doubled = docker_with(vec![
            file_cred(),
            crate::home::FileCredential { upstream: "https://other.example".into(), ..file_cred() },
        ]);
        let error = SandboxPolicy::from_config(Some(&doubled))
            .expect_err("one variable claimed twice must be refused");
        let message = error.to_string();
        assert!(
            message.contains("CURSOR_AUTH_TOKEN") && message.contains("two entries"),
            "the error must name the variable and the cause: {message}"
        );

        // Distinct variables still resolve, so the guard counts NAMES and not entries.
        SandboxPolicy::from_config(Some(&docker_with(vec![
            file_cred(),
            crate::home::FileCredential {
                env: "OTHER_TOKEN".into(),
                upstream: "https://other.example".into(),
                ..file_cred()
            },
        ])))
        .expect("two file credentials with distinct variables resolve");
    }

    // The launch-time half, for the sources config resolution cannot see: the delivery identity and
    // any base-URL override are assembled per job. Docker keeps the last `-e NAME=…`, so a
    // placeholder sharing a name with one of them is dropped and the vendor looks unreachable.
    #[test]
    fn a_docker_launch_refuses_a_placeholder_name_the_job_env_already_sets() {
        let policy =
            SandboxPolicy::from_config(Some(&docker_with(vec![file_cred()]))).expect("a policy");
        let agent_command = vec!["cursor-agent".to_string()];
        let workdir = Path::new("/home/seller/.maxplayer/seller-jobs/job1");
        let collided = vec![
            ("CURSOR_AUTH_TOKEN".to_string(), "from-another-source".to_string()),
            ("CURSOR_AUTH_TOKEN".to_string(), "the-placeholder".to_string()),
        ];

        let error = policy
            .launch(&agent_command, &job(workdir, &collided))
            .expect_err("a name set twice must be refused");
        assert!(
            error.to_string().contains("CURSOR_AUTH_TOKEN"),
            "the error must name the variable: {error}"
        );

        // One claimant launches, so the guard is not refusing every containment launch.
        let clean = vec![("CURSOR_AUTH_TOKEN".to_string(), "the-placeholder".to_string())];
        policy.launch(&agent_command, &job(workdir, &clean)).expect("one claimant launches");

        // INERTNESS, asserted rather than argued: a seat with no `file_credentials` iterates
        // nothing, so the identical duplicate launches exactly as it does today. This guard cannot
        // change a launch that works now.
        SandboxPolicy::from_config(Some(&docker_with(Vec::new())))
            .expect("a policy with no file credentials")
            .launch(&agent_command, &job(workdir, &collided))
            .expect("no file credentials ⇒ the guard is inert");
    }

    // A host executor is already a child of the daemon and inherits its whole environment, so
    // forwarding there would be a no-op that only lengthened the argv.
    #[test]
    fn a_host_executor_forwards_nothing_because_it_inherits_everything() {
        let always_set = |_: &str| Some("value".to_owned());
        assert!(forwarded_agent_env_from(&SandboxPolicy::passthrough(), always_set).is_empty());
        assert!(
            forwarded_agent_env_from(&SandboxPolicy::wrapped(argv(&["bwrap"])), always_set).is_empty()
        );
    }

    // `[sandbox] forward_env` serves a custom preset whose CLI reads a name the built-in set cannot
    // know. Repeating a built-in name must not double the `-e`.
    #[test]
    fn operator_named_env_extends_the_allowlist_without_duplicating_it() {
        let env = |key: &str| match key {
            "ANTHROPIC_API_KEY" => Some("sk-ant-xxx".to_owned()),
            "MY_AGENT_TOKEN" => Some("custom".to_owned()),
            _ => None,
        };
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "img".into(),
            forward_env: vec!["MY_AGENT_TOKEN".into(), "ANTHROPIC_API_KEY".into()],
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let carried = forwarded_agent_env_from(&policy, env);
        let names: Vec<&str> = carried.iter().map(|(k, _)| k.as_str()).collect();

        assert!(names.contains(&"MY_AGENT_TOKEN"), "{names:?}");
        assert_eq!(
            names.iter().filter(|n| **n == "ANTHROPIC_API_KEY").count(),
            1,
            "a built-in repeated by the operator must appear once: {names:?}"
        );
    }

    // The forwarded pairs must reach the container as real `-e` flags, or the allowlist is theatre.
    #[test]
    fn forwarded_env_reaches_the_container_argv() {
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let env = vec![("ANTHROPIC_API_KEY".to_string(), "sk-ant-xxx".to_string())];
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &env))
            .expect("command");
        assert!(
            windowed(&launch.args, &["-e", "ANTHROPIC_API_KEY=sk-ant-xxx"]),
            "the credential must cross as a -e flag: {:?}",
            launch.args
        );
    }

    // ── Credential containment red-prove (#647 acceptance #2) ────────────────────────────────────

    // A SYNTHETIC stand-in for a real model credential — never a real key (#647 discipline). Vendor
    // prefix + length so it is realistic, with an obvious `SYNTHETIC` marker so no reader mistakes it.
    // One distinct synthetic "real" credential per CONTAINED variable (never a real key). Each carries
    // an obvious `SYNTHETIC` marker and a per-var tag so a leak names which one leaked.
    #[cfg(feature = "acp")]
    const SYNTHETIC_REALS: &[(&str, &str)] = &[
        ("ANTHROPIC_API_KEY", "sk-ant-api03-SYNTHETICreal-apikey-000000000000000000000000000000000000000000AA"),
        ("ANTHROPIC_AUTH_TOKEN", "sk-ant-SYNTHETICreal-authtoken-00000000000000000000000000000000000000000000"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-SYNTHETICreal-oauth-00000000000000000000000000000000000000AA"),
        ("OPENAI_API_KEY", "sk-SYNTHETICreal-openai-00000000000000000000000000"),
    ];

    // The entire container view of a launch: the program plus every argument, as one searchable list.
    #[cfg(feature = "acp")]
    fn container_view(launch: &AgentLaunch) -> Vec<String> {
        let mut view = vec![launch.program.clone()];
        view.extend(launch.args.iter().cloned());
        view
    }

    // RED-PROVE, the FAILING half. Today every credential is forwarded verbatim as `-e VAR=<real>`, so
    // the container view CONTAINS each secret — the exact state the containment removes. Asserting the
    // leak here proves the green test below is not vacuous: strip the containment and the "absent"
    // assertions go red.
    #[cfg(feature = "acp")]
    #[test]
    fn todays_forwarding_leaks_every_real_credential_into_the_container_view() {
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let forwarded: Vec<(String, String)> =
            SYNTHETIC_REALS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &forwarded))
            .expect("launch");
        let view = container_view(&launch);
        for (var, real) in SYNTHETIC_REALS {
            assert!(
                view.iter().any(|s| s.contains(real)),
                "precondition: today's -e forwarding must leak {var}, else the red-prove is vacuous"
            );
        }
    }

    // RED-PROVE, the PASSING half (#647 acceptance #2, extended to ALL contained vars). With
    // containment NONE of the four real credentials appears anywhere in the container view — not in its
    // own variable, and not in an operator-added forward_env var that carried the same secret — while
    // each variable carries a distinct format-plausible placeholder, both vendor base URLs are routed
    // to the proxy, and the host-gateway pinhole is opened.
    #[cfg(feature = "acp")]
    #[test]
    fn contained_launch_keeps_every_real_credential_out_of_the_container_view() {
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "maxplayer-sandbox:latest".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        // All four credentials, an operator var carrying one of the secrets (must be scrubbed too), and
        // both vendor base URLs (the overrides must replace, not append).
        let mut forwarded: Vec<(String, String)> =
            SYNTHETIC_REALS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let openai_real = SYNTHETIC_REALS[3].1;
        forwarded.push(("MY_AGENT_TOKEN".to_string(), openai_real.to_string()));
        forwarded.push(("ANTHROPIC_BASE_URL".to_string(), "https://api.anthropic.com".to_string()));
        forwarded.push(("OPENAI_BASE_URL".to_string(), "https://api.openai.com".to_string()));

        // Mint a distinct placeholder per credential and drive the container-facing rewrite the same
        // way `start_credential_containment` does.
        let base_url = "http://host.docker.internal:54321";
        let placeholders: Vec<(String, String)> = SYNTHETIC_REALS
            .iter()
            .map(|(var, _)| (var.to_string(), format!("ph-{var}-000")))
            .collect();
        let substitutions: Vec<(String, String)> = SYNTHETIC_REALS
            .iter()
            .zip(&placeholders)
            .map(|((_, real), (_, ph))| (real.to_string(), ph.clone()))
            .collect();
        let contained = contain_env_values(
            &forwarded,
            &substitutions,
            &["ANTHROPIC_BASE_URL", "OPENAI_BASE_URL"],
            base_url,
        );

        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &contained))
            .expect("launch");
        let view = container_view(&launch);

        // No real credential survives anywhere in the container view.
        for (var, real) in SYNTHETIC_REALS {
            for element in &view {
                assert!(
                    !element.contains(real),
                    "real credential {var} leaked into the container view: {element}"
                );
            }
        }
        // Each credential variable now carries its placeholder.
        for (var, ph) in &placeholders {
            let pair = format!("{var}={ph}");
            assert!(
                windowed(&launch.args, &["-e", pair.as_str()]),
                "{var} must carry its placeholder: {:?}",
                launch.args
            );
        }
        // The operator var that carried the OpenAI secret is scrubbed to the OpenAI placeholder.
        let openai_ph = &placeholders[3].1;
        let operator = format!("MY_AGENT_TOKEN={openai_ph}");
        assert!(
            windowed(&launch.args, &["-e", operator.as_str()]),
            "an operator var carrying a secret must be scrubbed: {:?}",
            launch.args
        );
        // Both vendor base URLs point at the proxy, each exactly once (overridden, not appended).
        for base_env in ["ANTHROPIC_BASE_URL", "OPENAI_BASE_URL"] {
            let pair = format!("{base_env}={base_url}");
            assert!(windowed(&launch.args, &["-e", pair.as_str()]), "{base_env} must route to the proxy");
            assert_eq!(
                launch.args.iter().filter(|a| a.starts_with(&format!("{base_env}="))).count(),
                1,
                "{base_env} must be overridden in place, not duplicated: {:?}",
                launch.args
            );
        }
        assert!(
            windowed(&launch.args, &["--add-host", "host.docker.internal:host-gateway"]),
            "the host-gateway pinhole must be opened: {:?}",
            launch.args
        );
    }

    // A docker launch that carries no proxy alias opens no host-gateway pinhole — the flag is added
    // only when something references the alias (cleancut: no inert flags).
    #[cfg(feature = "acp")]
    #[test]
    fn docker_launch_without_containment_opens_no_pinhole() {
        let policy = SandboxPolicy::docker(DockerPolicy {
            image: "img".into(),
            forward_env: Vec::new(),
            runtime: None,
            network: None,
            proxy_ports: None,
            file_credentials: Vec::new(),
        });
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("launch");
        assert!(
            !launch.args.iter().any(|a| a == "--add-host"),
            "no alias referenced ⇒ no --add-host: {:?}",
            launch.args
        );
    }

    // The scope-gap detector (#647 P2): every KNOWN credential var is contained, so only an
    // operator-added forward_env var the daemon cannot recognize is flagged; a known credential named
    // in forward_env is not; a non-docker policy reports nothing (host inherits, no container).
    #[test]
    fn uncontained_forwarded_credentials_flags_only_unrecognized_operator_vars() {
        let docker = |forward_env: Vec<String>| {
            SandboxPolicy::docker(DockerPolicy {
                image: "img".into(),
                forward_env,
                runtime: None,
                network: None,
                proxy_ports: None,
                file_credentials: Vec::new(),
            })
        };
        // Operator forwards an unknown var (set), a known credential (contained), and a blank one.
        let policy = docker(vec![
            "MY_AGENT_TOKEN".into(),
            "OPENAI_API_KEY".into(), // a KNOWN credential — contained, must NOT be flagged
            "BLANK_VAR".into(),
        ]);
        let env = |key: &str| match key {
            "MY_AGENT_TOKEN" => Some("operator-secret".to_owned()),
            "OPENAI_API_KEY" => Some("sk-real".to_owned()),
            "BLANK_VAR" => Some("  ".to_owned()), // set-but-blank must not count
            _ => None,
        };
        assert_eq!(uncontained_forwarded_credentials(&policy, env), vec!["MY_AGENT_TOKEN".to_owned()]);
        // No operator extras ⇒ nothing to warn about even with every known credential set.
        let all_known = |key: &str| is_contained_credential(key, &[]).then(|| "real".to_owned());
        assert!(uncontained_forwarded_credentials(&docker(Vec::new()), all_known).is_empty());
        // A non-docker policy forwards nothing into a container ⇒ never flagged.
        assert!(uncontained_forwarded_credentials(
            &SandboxPolicy::passthrough(),
            |_| Some("set".to_owned())
        )
        .is_empty());
    }

    // A docker seat with no `image` set DEFAULTS to the binary-owned GHCR ref (issue #792 phase 3):
    // it resolves instead of failing closed, and that exact ref appears in the docker run argv, so a
    // fresh seller who sets only `mode = "docker"` gets a working container. An explicit image still
    // wins; an absent config is still pass-through.
    #[test]
    fn from_config_defaults_docker_image_when_unset() {
        use crate::home::{SandboxConfig, SandboxMode};
        let base = SandboxConfig {
            mode: SandboxMode::Docker,
            ..Default::default()
        };
        // docker with no image ⇒ resolves to the default, NOT an error.
        let policy = SandboxPolicy::from_config(Some(&base)).expect("defaults the image");
        assert_eq!(policy.docker_image(), Some(DEFAULT_SANDBOX_IMAGE));
        // The default ref is the version-pinned GHCR image this build owns.
        assert_eq!(
            DEFAULT_SANDBOX_IMAGE,
            concat!("ghcr.io/makeprisms/maxplayer-sandbox:v", env!("CARGO_PKG_VERSION")),
        );
        // The default ref must actually reach the docker run argv the ACP driver spawns.
        let launch = policy
            .launch(&argv(&["claude-agent-acp"]), &job(Path::new("/w"), &[]))
            .expect("command");
        assert!(
            launch.args.iter().any(|a| a == DEFAULT_SANDBOX_IMAGE),
            "default image ref {DEFAULT_SANDBOX_IMAGE:?} must appear in argv: {:?}",
            launch.args,
        );
        // A blank image is treated as unset and defaults too.
        let blank = SandboxConfig { image: Some("   ".into()), ..base.clone() };
        assert_eq!(
            SandboxPolicy::from_config(Some(&blank)).expect("ok").docker_image(),
            Some(DEFAULT_SANDBOX_IMAGE),
        );
        // An explicit image still wins over the default.
        let complete = SandboxConfig { image: Some("img".into()), ..base };
        assert_eq!(
            SandboxPolicy::from_config(Some(&complete)).expect("ok").docker_image(),
            Some("img"),
        );
        // Absent config ⇒ pass-through.
        assert!(SandboxPolicy::from_config(None).expect("ok").is_passthrough());
    }

    // Does `haystack` contain `needle` as a contiguous run? (argv flag/value adjacency check.)
    fn windowed(haystack: &[String], needle: &[&str]) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w.iter().zip(needle).all(|(a, b)| a == b))
    }

    // END-TO-END: actually run the docker launch and prove the isolation property. Gated on
    // `MAXPLAYER_SANDBOX_DOCKER_E2E=1` (and needs a docker daemon + a base image with a shell), so it is
    // a no-op in CI/unit runs and runs only where docker is available. The probe stands in for the
    // agent: it tries to read a secret placed under the host $MAXPLAYER_HOME (which is a PARENT of the
    // mounted workdir) and writes its verdict + a file into the workdir.
    #[test]
    fn docker_run_hides_maxplayer_home_and_persists_workdir_output() {
        if std::env::var("MAXPLAYER_SANDBOX_DOCKER_E2E").as_deref() != Ok("1") {
            return; // docker-dependent; skipped unless explicitly enabled
        }
        let image = std::env::var("MAXPLAYER_SANDBOX_E2E_IMAGE").unwrap_or_else(|_| "alpine:latest".into());

        // Host layout: <home>/wallet.secret (the thing to protect) and <home>/seller-jobs/job1 (the
        // ONLY directory the container mounts). workdir is a child of the sensitive home.
        let home = std::env::temp_dir().join(format!("maxplayer-e2e-{}", std::process::id()));
        let workdir = home.join("seller-jobs").join("job1");
        std::fs::create_dir_all(&workdir).expect("mkdir workdir");
        std::fs::write(home.join("wallet.secret"), b"SEED PHRASE").expect("write secret");

        // Probe: cat the secret by its HOST absolute path (absent in the container) and via the
        // container root (`/work/..`); either success would be a LEAK. Then write into the workdir.
        let probe = format!(
            "if cat '{}' 2>/dev/null || cat /work/../wallet.secret 2>/dev/null; then echo LEAKED; \
             else echo ISOLATED; fi > /work/verdict.txt; echo hello > /work/wrote.txt",
            home.join("wallet.secret").display()
        );
        let agent_command = argv(&["sh", "-c", &probe]);
        let policy =
            SandboxPolicy::docker(DockerPolicy {
                image,
                forward_env: Vec::new(),
                runtime: None,
                network: None,
                proxy_ports: None,
                file_credentials: Vec::new(),
            });
        let job = JobLaunch {
            workdir: &workdir,
            env: &[],
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            netns: None,
        };
        let launch = policy.launch(&agent_command, &job).expect("docker launch");

        let status = std::process::Command::new(&launch.program)
            .args(&launch.args)
            .status()
            .expect("run docker");
        assert!(status.success(), "docker run exited non-zero");

        // $MAXPLAYER_HOME was unreadable in the container, and the workdir write persisted to the host.
        let verdict = std::fs::read_to_string(workdir.join("verdict.txt")).expect("verdict");
        assert_eq!(verdict.trim(), "ISOLATED", "the container could reach $MAXPLAYER_HOME");
        assert_eq!(
            std::fs::read_to_string(workdir.join("wrote.txt")).expect("wrote").trim(),
            "hello",
            "workdir output did not land on the host for the snapshot"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // The seller-side receipt-preimage delivery discriminator is DERIVED from the typed
    // `GitDelivery` ("fork"), not a hardcoded label — buyer and seller agree by construction.
    #[test]
    fn seller_delivery_kind_derives_fork_from_typed_delivery() {
        let kind = seller_delivery_kind(
            "https://relay.example/git/job.git",
            "maxplayer/abcd1234",
            &"a".repeat(40),
        )
        .expect("commit delivery types");
        assert_eq!(kind, crate::receipt::DeliveryKind::Fork);
        assert_eq!(kind.as_str(), "fork");
    }

    #[cfg(feature = "acp")]
    #[test]
    fn response_timeout_is_classified_by_its_typed_timer_source() {
        use crate::driver::DriverError;
        use crate::engine::EngineError;

        let deadline = classify_run_error(
            EngineError::Driver(DriverError::ResponseTimeout { request_id: 3 }),
            AgentRunTimeout::JobDeadline(Duration::from_secs(60)),
        );
        assert!(matches!(deadline, ExecError::DeadlineExceeded));
        assert_eq!(
            deadline.to_string(),
            "job deadline reached while the agent was still running"
        );

        // The same driver timer is a health failure under the independently bounded self-probe.
        // This guards against exempting genuine probe timeouts from the existing drop rule.
        let probe = classify_run_error(
            EngineError::Driver(DriverError::ResponseTimeout { request_id: 7 }),
            AgentRunTimeout::HarnessProbe(Duration::from_secs(120)),
        );
        assert!(matches!(probe, ExecError::Agent(_)));
        assert!(probe.to_string().contains("ACP request 7 timed out"));
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
                    Ok::<AgentRunReport, ExecError>(AgentRunReport::default())
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
            async move { Err::<AgentRunReport, ExecError>(ExecError::Agent("always".into())) }
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
            async move { Err::<AgentRunReport, ExecError>(ExecError::Agent("late".into())) }
        })
        .await;
        assert!(out.is_err(), "past deadline ⇒ error (caller publishes feedback-kind)");
        assert_eq!(attempts.get(), 1, "no retry once the deadline has passed");
    }

    // The seller appends explicit, secret-free delivery instructions.
    #[test]
    fn composed_prompt_carries_task_and_owned_delivery_instructions() {
        let remote = "https://relay.example/git/abc.git";
        let prompt = compose_agent_prompt("build a widget", remote, 1_800_000_123, None, None);
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

    // #685: the preamble must carry THIS JOB'S OWN VALUES, not fixed prose. Two different jobs are
    // composed and each is checked for its own values AND for the absence of the other's — a
    // hardcoded constant cannot satisfy both halves, which is what makes this a value check rather
    // than a prose check.
    #[test]
    fn preamble_carries_this_jobs_own_values_and_never_invites_refusal() {
        let remote_a = "https://relay.example/git/aaa.git";
        let remote_b = "https://relay.example/git/bbb.git";
        // Composed WITH a declared output type (#686) so every check below — the wrap-seam
        // instruments and the refusal ban especially — covers that line too, not only the #685 text.
        let a = compose_agent_prompt("task A", remote_a, 1_800_000_123, Some("text/plain"), None);
        let b = compose_agent_prompt(
            "task B",
            remote_b,
            1_900_000_456,
            Some("application/json"),
            None,
        );

        // Identity, deadline and boundaries are present at all — the three things #685 adds.
        for prompt in [&a, &b] {
            assert!(
                prompt.contains("maxplayer marketplace"),
                "identity present: {prompt}"
            );
            assert!(prompt.contains("DEADLINE:"), "deadline stated: {prompt}");
            assert!(prompt.contains("BOUNDARIES:"), "boundaries stated: {prompt}");
            // The preamble's sentences span `\`-continuations in the source, and each one swallows
            // the newline AND the next line's indentation. Both failure modes are silent, and they
            // need different instruments:
            //
            // - a DOUBLED space is caught totally, at every seam that exists or is ever added,
            //   because our composed text contains no legitimate double space. This test's task
            //   strings have none either, so any hit came from the seams.
            // - a LOST space concatenates two words, which no general rule sees — so the joined
            //   phrases are named, one per seam I introduced.
            //
            // Without these, a rewrap ships as mangled prose that only the hired agent ever reads.
            assert!(
                !prompt.contains("  "),
                "no doubled space at any source-wrap seam: {prompt}"
            );
            for joined in [
                "It applies to how you carry out the task above",
                "A buyer posted the task above",
                "and the buyer settles payment when your work is delivered",
                "may never be delivered",
                "Work only inside it",
                "or any file outside this directory",
                "on the offer, so produce the deliverable in that form",
                "cannot contain — a PID or pidfile captured at start",
                "never `pgrep -f` on a substring of your own command line",
                "so a compiler, runtime or CLI your task needs may be absent",
                "the container is yours alone and is discarded when the job ends",
                "an unprivileged user with no sudo and no capabilities",
                "install under `$HOME` instead, and never into the job directory",
                "your deliverable. Prefer `nix develop` when the project ships a flake",
                "otherwise a user-local installer such as rustup",
                "with a prefix under `$HOME`.",
                "say so in your output and carry on with the rest of the task",
                "worth more to the buyer than a deadline spent hiding one.",
            ] {
                assert!(
                    prompt.contains(joined),
                    "preamble joins cleanly across a source wrap ({joined:?}): {prompt}"
                );
            }
        }

        // Each job's own task, deadline epoch and bound remote appear; the other job's do not.
        assert!(a.contains("task A") && a.contains("aaa.git"), "job A values: {a}");
        assert!(b.contains("task B") && b.contains("bbb.git"), "job B values: {b}");
        assert!(a.contains("1800000123"), "job A's exact deadline epoch: {a}");
        assert!(b.contains("1900000456"), "job B's exact deadline epoch: {b}");
        assert!(!a.contains("1900000456"), "not the other job's deadline: {a}");
        assert!(!a.contains("bbb.git"), "not the other job's remote: {a}");

        // The deadline VALUE reaches the text: hold every other input fixed and only it varies.
        let early = compose_agent_prompt("t", "r", 1_000_000_001, None, None);
        let late = compose_agent_prompt("t", "r", 1_000_000_002, None, None);
        assert_ne!(
            early, late,
            "the deadline argument must reach the prompt, not be dropped"
        );

        // ⛔ #685 excludes a refusal instruction: today a refusal either quarantines the harness or
        // mints a sentinel and gets PAID, so inviting one before the pre-money seam handles it
        // means paying for refusals. Asserted, not merely omitted — an omission leaves no trace
        // and the next hand re-adds it.
        let lower = a.to_lowercase();
        for banned in ["refuse", "decline", "you may reject", "if you cannot"] {
            assert!(
                !lower.contains(banned),
                "must not invite refusal (found {banned:?}): {a}"
            );
        }
    }

    // TOOTH (#686) — the buyer's DECLARED OUTPUT TYPE reaches the hired agent, as a VALUE.
    //
    // The offer's `output` tag is mandatory on ingest and is a MIME / output type, but until this
    // change it stopped at the parsed offer: the agent was never told what form the buyer asked for.
    // Two prompts are composed with EVERY other input held fixed and only the output type varying,
    // and each is checked for its own value AND for the absence of the other's — a hardcoded string
    // (or a dropped argument) cannot satisfy both halves.
    //
    // Bite (measured): pass `None` for `declared_output` inside `compose_agent_prompt`, or drop
    // `{output_section}` from the format string, and this test goes red on the first assertion.
    #[test]
    fn the_declared_output_type_reaches_the_prompt_and_absence_states_nothing() {
        let json = compose_agent_prompt("t", "r", 1_000, Some("application/json"), None);
        let plain = compose_agent_prompt("t", "r", 1_000, Some("text/plain"), None);

        assert!(
            json.contains("application/json"),
            "the declared output type must reach the prompt: {json}"
        );
        assert!(
            !json.contains("text/plain"),
            "not some other job's output type: {json}"
        );
        assert!(
            plain.contains("text/plain"),
            "the declared output type must reach the prompt: {plain}"
        );
        assert_ne!(
            json, plain,
            "the output-type argument must reach the prompt, not be dropped"
        );
        assert!(
            json.contains("DECLARED OUTPUT TYPE:"),
            "it is stated as the buyer's declared output type, so the agent can tell whose fact it \
             is: {json}"
        );

        // The task stays FIRST — our own prose never pushes the buyer's instructions down.
        assert!(json.starts_with("t\n\n"), "task still first: {json}");

        // ABSENT ⇒ SILENT, byte-for-byte. An offer recorded before the column existed declares no
        // output type, and stating a default would put a fact in the prompt no buyer ever gave.
        let absent = compose_agent_prompt("t", "r", 1_000, None, None);
        assert!(
            !absent.contains("DECLARED OUTPUT TYPE"),
            "no declared type ⇒ nothing stated: {absent}"
        );
        // Blank is absence too (a whitespace-only tag value states nothing), so it lands on the
        // very same bytes rather than on an empty "DECLARED OUTPUT TYPE: ." line.
        assert_eq!(
            compose_agent_prompt("t", "r", 1_000, Some("   "), None),
            absent,
            "a blank output type states nothing, exactly as an absent one does"
        );

        // ⛔ NOT ENFORCEMENT (#686 scope): the prompt STATES the type, and states that the task
        // wins if the two disagree. Nothing here invites the agent to refuse over a format — the
        // refusal ban asserted in the test above covers this line because it is composed with an
        // output type set.
        assert!(
            json.contains("The task above wins where the two disagree."),
            "the buyer's task, not our tag, is authoritative: {json}"
        );
    }

    // TOOTH (#731) — the preamble warns the hired agent off self-matching process waiters.
    //
    // A waiter whose command line CONTAINS the substring it greps for (`pgrep -f "cargo test …"`)
    // matches sibling waiters forever. Measured on a live seller: after the job had already failed,
    // 6 processes matched the needle, 0 real cargo, 0 real rustc. Daemon cleanup tolerates leaking
    // a grandchild ("recoverable"), which is true for mortal grandchildren; a self-matching waiter
    // is not mortal. The preamble is the only place this reaches: it is agent-authored shell.
    // Daemon-side reaping is #733, not this change.
    //
    // Bite: drop the BOUNDARIES bullet, and the first assertion goes red.
    #[test]
    fn preamble_warns_off_self_matching_process_waiters() {
        let prompt = compose_agent_prompt("t", "r", 1_000, None, None);
        let boundaries = prompt
            .split("BOUNDARIES:\n")
            .nth(1)
            .and_then(|rest| rest.split("\n---\n").next())
            .expect("BOUNDARIES section");

        assert!(
            boundaries.contains("never `pgrep -f`"),
            "forbids pgrep -f, the self-matching waiter: {prompt}"
        );
        assert!(
            boundaries.contains("`pgrep -x`"),
            "names pgrep -x as the exact-name alternative: {prompt}"
        );
        assert!(
            boundaries.contains("pidfile"),
            "names a pidfile captured at start as a match the waiter cannot contain: {prompt}"
        );
        assert!(
            boundaries.contains("cannot contain"),
            "states the invariant — match something the waiter itself cannot contain: {prompt}"
        );
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
    fn the_dashboard_family_vocabulary_is_not_the_wire_family_vocabulary() {
        // TWO DIFFERENT THINGS SHARE THE NAME "harness family", and this test exists so that fact is
        // EXECUTABLE rather than only written down:
        //
        // - The DASHBOARD classifier (mirrored in the test above): substring match over a receipt's
        //   `harness` id, vocabulary {claude, cursor, codex} plus a catch-all "other".
        // - The WIRE tag (#784, `crate::agent_presets::HARNESS_FAMILIES`): a closed enum matched
        //   EXACTLY, no catch-all, and it spells the Claude family `claude-code`.
        //
        // The hazard is not the shared name — it is a shared name with a NEARLY shared vocabulary.
        // Disjoint vocabularies would be caught on first use because everything would miss;
        // identical ones would be harmless. Overlapping in 2 of 3 is the lethal middle: joining the
        // two fields returns correct-looking results for codex and cursor seats and silently drops
        // every claude one.
        //
        // So the assertion is on the RELATIONSHIP, and it is deliberately two-sided: it pins where
        // they agree AND where they differ. Anyone who later "aligns" either side — renaming the
        // wire enum to `claude`, or teaching the dashboard `claude-code` — trips this and is told
        // what they are actually changing. Aligning them is not forbidden; doing it silently is.
        use crate::agent_presets::{HARNESS_FAMILIES, harness_family_for_preset};

        // Where they AGREE: same preset, same token, on both sides.
        for shared in ["codex", "cursor"] {
            assert_eq!(harness_family(shared), shared, "dashboard side of {shared:?}");
            assert_eq!(
                harness_family_for_preset(shared),
                Some(shared),
                "wire side of {shared:?}"
            );
        }

        // Where they DIVERGE, and it is exactly one token.
        assert_eq!(harness_family("claude"), "claude", "dashboard spells it claude");
        assert_eq!(
            harness_family_for_preset("claude"),
            Some("claude-code"),
            "the wire spells it claude-code — this is the ONE divergence, and #784 chose it"
        );
        assert!(
            !HARNESS_FAMILIES.contains(&"claude"),
            "the wire vocabulary must NOT also accept the dashboard spelling — two spellings of one \
             family on the wire is exactly the canonicalisation failure #784 forbids"
        );

        // And the structural difference behind the divergence: the dashboard has a catch-all, the
        // wire has none. An unknown id is classified by one and unmatchable by the other.
        assert_eq!(harness_family("something-unknown"), "other");
        assert_eq!(harness_family_for_preset("something-unknown"), None);
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
            "maxplayer delivery: Fix the parser"
        );
        // Leading blank lines skipped; whitespace collapsed.
        assert_eq!(
            delivery_message("\n\n   add   retry   logic  "),
            "maxplayer delivery: add retry logic"
        );
        // Empty task falls back to a fixed label.
        assert_eq!(delivery_message("   \n  "), "maxplayer delivery");
        // Long first line is capped. #637 moved the budget from the summary alone to the WHOLE
        // subject line, so the expected total moved with it: 72 bytes of subject, not 20 + 72 = 92.
        // The fixture and its intent are unchanged — a 200-byte first line must come out capped —
        // only the constant it is anchored to, which is the constant the fix deliberately changed.
        let long = "x".repeat(200);
        let msg = delivery_message(&long);
        assert!(msg.starts_with("maxplayer delivery: "));
        assert_eq!(msg.len(), SUBJECT_BYTE_BUDGET);
    }

    // ── #637: the subject cap cuts mid-word, and it counts chars where a subject counts bytes ────
    //
    // RED-PROVE, run at this commit with
    // `cargo test -p maxplayer-core --features wallet --locked --offline --lib -- \
    //  seller_exec::tests::delivery_message`:
    //
    //   (a) REVERTED — `crates/maxplayer-core/src/seller_exec.rs:358`, the capping call in
    //       `delivery_message`, put back to the pre-fix one-liner it replaced:
    //         `let capped: String = summary.chars().take(72).collect();`
    //       and, so that field (c) reports the pre-existing test as it actually stood, its length
    //       assertion (this file, :1182) put back to its own pre-fix form:
    //         `assert_eq!(msg.len(), "maxplayer delivery: ".len() + 72);`
    //
    //   (b) FAILED ASSERTIONS, quoted verbatim from that run — 3 failed, 1 passed. Line numbers
    //       are this file's, i.e. where each assertion sits in the delivered diff; the reverted
    //       build printed slightly different ones, offset by the reverted edit itself:
    //
    //       seller_exec.rs:1248, delivery_message_caps_a_multibyte_summary_by_bytes_not_chars —
    //         "72-byte subject budget, got 91 bytes: maxplayer delivery: ünïcödé ünïcödé ünïcödé
    //          ünïcödé ünïcödé ünïcödé"
    //       47 chars slid under the 72-CHAR cap untouched and emitted a 91-byte subject: the char
    //       cap is not a byte bound.
    //
    //       seller_exec.rs:1299, delivery_message_cuts_a_long_summary_at_a_word_boundary —
    //         "assertion `left == right` failed: no partial trailing word — the original must
    //          continue with a space after the cut, not mid-token
    //            left: Some(118)
    //           right: Some(32)"
    //       118 is `v`, not a space: the cut landed inside `delivery`, emitting #637's own exhibit
    //         "maxplayer delivery: Implement Phase 0 of maxplayerai issue #599 (\"verified
    //          contribution deli"
    //       — 92 bytes, unclosed paren, unclosed quote.
    //
    //       seller_exec.rs:1344, delivery_message_leaves_an_under_budget_summary_byte_identical —
    //         "assertion `left == right` failed
    //            left: "maxplayer delivery: wwwwwwwwww…" (53 w, 73 bytes)
    //           right: "maxplayer delivery: wwwwwwwwww…" (52 w, 72 bytes)"
    //
    //   (c) PRE-EXISTING COVERAGE under that break: NONE — `delivery_message_summarizes_the_task`
    //       stayed GREEN. That is the finding rather than an accident: its long-line fixture is
    //       `"x".repeat(200)`, a single ASCII word, with no word boundary to cut badly and no
    //       multi-byte char to widen, so it pinned the byte count of a case both defects are
    //       invisible in. It is also the only pre-existing test anywhere that feeds
    //       `delivery_message` a summary over the cap (`seller_node::run`'s caller test uses
    //       "build a widget"). Nothing else in the suite could go red for either defect.

    #[test]
    fn delivery_message_caps_a_multibyte_summary_by_bytes_not_chars() {
        // The input that separates a byte cap from a char cap: FEWER chars than the old 72-char cap
        // (so the pre-fix code passes it through whole) but MORE bytes than the 52-byte summary
        // budget. Truncation must happen, must not panic, and must not emit a broken scalar.
        let word = "ünïcödé"; // 7 chars, 11 bytes
        let summary = vec![word; 6].join(" "); // 47 chars, 71 bytes
        assert!(
            summary.chars().count() < 72
                && summary.len() > SUBJECT_BYTE_BUDGET - SUBJECT_PREFIX.len(),
            "fixture must slip a 72-CHAR cap while busting the 52-byte summary budget: \
             {} chars, {} bytes",
            summary.chars().count(),
            summary.len()
        );

        let msg = delivery_message(&summary);
        assert!(
            msg.len() <= SUBJECT_BYTE_BUDGET,
            "72-byte subject budget, got {} bytes: {msg}",
            msg.len()
        );
        // Valid UTF-8, explicitly: a byte-sliced subject that split a scalar could not round-trip.
        assert_eq!(
            std::str::from_utf8(msg.as_bytes()).expect("subject is valid UTF-8"),
            msg
        );
        let capped = msg
            .strip_prefix(SUBJECT_PREFIX)
            .expect("prefixed subject: {msg}");
        assert!(
            summary.starts_with(capped),
            "the cut is a prefix of the input — no mangled or re-encoded scalar: {capped}"
        );
        // Byte 52 of this fixture falls INSIDE the `ï` of the fifth word, so the boundary walk runs
        // and then the word trim takes it back to four whole words.
        assert_eq!(capped, vec![word; 4].join(" "));

        // Hard-cut arm, multi-byte: one word with no space anywhere to trim back to. Byte 52 lands
        // inside an `é` (the leading "a" makes every char boundary odd), so a naive `&summary[..52]`
        // would PANIC here — this is the case the boundary walk exists for.
        let one_word = format!("a{}", "é".repeat(60)); // 61 chars, 121 bytes
        assert!(!one_word.is_char_boundary(SUBJECT_BYTE_BUDGET - SUBJECT_PREFIX.len()));
        let msg = delivery_message(&one_word);
        let capped = msg.strip_prefix(SUBJECT_PREFIX).expect("prefixed subject");
        assert_eq!(
            capped.len(),
            51,
            "floored to the char boundary below 52: {capped}"
        );
        assert_eq!(capped.chars().count(), 26, "'a' + 25 whole é: {capped}");
        assert!(one_word.starts_with(capped));
        assert_eq!(msg.len(), SUBJECT_PREFIX.len() + 51);
    }

    #[test]
    fn delivery_message_cuts_a_long_summary_at_a_word_boundary() {
        // #637's own exhibit: the first line of #635, which reached `main` cut mid-token with an
        // unclosed paren and an unclosed quote.
        let summary =
            "Implement Phase 0 of maxplayerai issue #599 (\"verified contribution delivery\") e2e";
        let msg = delivery_message(summary);
        let capped = msg.strip_prefix(SUBJECT_PREFIX).expect("prefixed subject");

        assert!(
            summary.starts_with(capped),
            "the cut is a prefix of the input: {capped}"
        );
        assert_eq!(
            summary.as_bytes().get(capped.len()),
            Some(&b' '),
            "no partial trailing word — the original must continue with a space after the cut, \
             not mid-token"
        );
        assert!(
            msg.len() <= SUBJECT_BYTE_BUDGET,
            "{} bytes: {msg}",
            msg.len()
        );
        assert!(
            !capped.ends_with(' '),
            "no trailing-space damage: {capped:?}"
        );
        assert_eq!(capped, "Implement Phase 0 of maxplayerai issue #599");
    }

    #[test]
    fn delivery_message_leaves_an_under_budget_summary_byte_identical() {
        // The other arm: a fix that mangles short subjects trades one defect for another. Nothing is
        // appended, nothing is trimmed, no ellipsis — right up to the budget, ASCII or not.
        let budget = SUBJECT_BYTE_BUDGET - SUBJECT_PREFIX.len();
        for summary in [
            "Fix the parser",
            "add retry logic and a regression test", // spaces, comfortably under
            "café ☕ ünïcödé — short but multi-byte", // multi-byte, under budget in BYTES
            &"w".repeat(budget),                     // exactly AT the budget
            &format!("{} tail", "w".repeat(budget - 5)), // ends exactly at the budget, with words
        ] {
            assert!(
                summary.len() <= budget,
                "fixture is under budget: {summary}"
            );
            assert_eq!(
                delivery_message(summary),
                format!("{SUBJECT_PREFIX}{summary}"),
                "under-budget summaries pass through byte-identical"
            );
        }

        // One byte over the budget, single word: the hard-cut arm fills the budget exactly and adds
        // nothing — 72 bytes of subject, no ellipsis, no trailing space.
        let over = "w".repeat(budget + 1);
        let msg = delivery_message(&over);
        assert_eq!(msg, format!("{SUBJECT_PREFIX}{}", "w".repeat(budget)));
        assert_eq!(msg.len(), SUBJECT_BYTE_BUDGET);
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
            AgentRunTimeout::JobDeadline(Duration::from_secs(1)),
        )
        .await
        .expect_err("acp required");
        assert!(matches!(err, ExecError::AcpRequired));
        assert!(err.to_string().contains("acp"));
    }

    // ---- every endpoint flag gets its own redirect pair ------------------------------------------
    //
    // Emitting only the first flag contains only the endpoints named first and leaves the rest
    // reaching the vendor with the placeholder. That authenticates nothing, so the job fails — and it
    // fails invisibly, because traffic that bypasses the proxy leaves no trace in the proxy's log.
    // Measured on `cursor-agent 2026.08.11-e8db854`: `--endpoint` moves the control plane and a
    // separate undocumented `--agent-endpoint` moves the agent leg.
    //
    // Gated to match the function it calls. A test must be gated at least as tightly as the tightest
    // thing it references, or it fails to COMPILE in the narrower feature rows — which is a build
    // break, not a test failure, and it takes the whole row down with it.
    #[cfg(feature = "acp")]
    #[test]
    fn every_endpoint_flag_gets_its_own_redirect_pair() {
        let cred = crate::home::FileCredential {
            endpoint_args: vec!["--endpoint".into(), "--agent-endpoint".into()],
            ..file_cred()
        };
        let placeholder = "PLACEHOLDER-VALUE".to_owned();
        let base_url = "http://127.0.0.1:9300";

        let (env, argv) = file_credential_launch_additions(&[(&cred, placeholder.clone())], base_url);

        assert_eq!(env, vec![("CURSOR_AUTH_TOKEN".to_owned(), placeholder)]);
        assert_eq!(
            argv,
            vec![
                "--endpoint".to_owned(),
                base_url.to_owned(),
                "--agent-endpoint".to_owned(),
                base_url.to_owned(),
            ],
            "each flag must be followed by the proxy URL, and NO flag may be dropped"
        );
        // Stated as a count too, so a future edit that emits the flags without their URLs (or the
        // URLs once) fails here rather than passing a looser shape.
        assert_eq!(argv.len(), cred.endpoint_args.len() * 2);
    }

    // A one-flag client is the common case and must not gain a spurious second redirect.
    #[cfg(feature = "acp")]
    #[test]
    fn a_single_endpoint_flag_still_emits_exactly_one_pair() {
        let (_, argv) = file_credential_launch_additions(&[(&file_cred(), "P".to_owned())], "http://u");
        assert_eq!(argv, vec!["--endpoint".to_owned(), "http://u".to_owned()]);
    }

    // The same invariant for a CODE-constructed credential, which never passes through serde — so the
    // deserializer's guard is not a guard over this path. Two enforcement points because there are two
    // ways in, not because one is redundant.
    #[test]
    fn a_code_constructed_endpoint_flag_with_whitespace_is_refused() {
        for padded in [" --endpoint", "--endpoint ", "--endpoint --agent-endpoint", "\t--endpoint"] {
            let cred = crate::home::FileCredential {
                endpoint_args: vec![padded.into()],
                ..file_cred()
            };
            let error = SandboxPolicy::from_config(Some(&docker_with(vec![cred])))
                .expect_err("a whitespace-bearing flag must be refused in code too");
            assert!(
                error.to_string().contains("whitespace"),
                "the error must name whitespace as the cause for {padded:?}, got: {error}"
            );
        }
        // And the clean form still resolves, so the guard is not refusing everything.
        SandboxPolicy::from_config(Some(&docker_with(vec![file_cred()])))
            .expect("an unpadded flag must still resolve");
    }
}
