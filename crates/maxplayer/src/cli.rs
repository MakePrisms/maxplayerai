use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(feature = "acp")]
use std::time::Duration;

use maxplayer_core::EventLog;
#[cfg(feature = "acp")]
use maxplayer_core::driver::{AcpDriver, AgentCommand};
#[cfg(feature = "acp")]
use maxplayer_core::driver::{ContentBlock, PromptTurn, SessionConfig};
use maxplayer_core::driver::{MockDriver, PermissionOutcome, ScriptedSession, SessionUpdate};
use maxplayer_core::engine::{RunEvent, RunParams, run_job};
use maxplayer_core::event::{JobId, RuntimeId};
use serde::Serialize;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

pub fn run<I, S>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    match args.get(1).map(String::as_str) {
        Some("version" | "--version") if args.len() == 2 => {
            let _ = writeln!(out, "maxplayer {}", maxplayer_core::version());
            SUCCESS
        }
        Some("--help") if args.len() == 2 => {
            write_usage(out);
            SUCCESS
        }
        // #570: the inline debug/replay surfaces (mcp, log, mock, run) carry no usage of their own —
        // they are documented in the top-level usage — so a sole `--help` on any of them prints THAT
        // to stdout and exits 0, before dispatch. The module subcommands (buyer, wallet, …) answer
        // `--help` from their own usage at the top of each `run`, so they are handled there, not here.
        Some("mcp" | "log" | "mock" | "run") if is_help_request(&args[2..]) => {
            write_usage(out);
            SUCCESS
        }
        Some("mcp") if args.len() == 2 => crate::mcp::run(out, err),
        Some("buyer") => crate::buyer::run(&args[2..], out, err),
        // Seller advertise surface — compiled in only with `acp` (#360). On a buyer-only build this
        // falls through to `usage`, so `seller` cannot boot or publish a seat it can never deliver on.
        #[cfg(feature = "acp")]
        Some("seller") => crate::sell::run(&args[2..], out, err),
        Some("accept") => crate::accept_cli::run(&args[2..], out, err),
        Some("collect") => crate::collect_cli::run(&args[2..], out, err),
        Some("doctor") => crate::doctor::run(&args[2..], out, err),
        // Run BY the boot gate, inside the configured launcher, to report what the launcher let it
        // do. Reachable by hand too: an operator debugging a sandbox wants to run exactly what the
        // gate runs rather than a description of it.
        #[cfg(feature = "wallet")]
        Some("sandbox-probe") => crate::sandbox_probe::run(&args[2..], out, err),
        Some("wallet") => crate::wallet_cli::run(&args[2..], out, err),
        Some("profile") => crate::profile_cli::run(&args[2..], out, err),
        Some("whoami") => crate::whoami::run(&args[2..], out, err),
        #[cfg(feature = "stub-pay")]
        Some("stub-pay") => crate::stub_pay_cli::run(&args[2..], out, err),
        Some("log") => run_log(&args[2..], out, err),
        Some("mock") => run_mock(&args[2..], out, err),
        Some("run") => run_agent(&args[2..], out, err),
        _ => usage(err),
    }
}

/// A sole `--help` request at a subcommand level. True when `--help` is the final token and every
/// token before it is a subcommand SELECTOR (a non-flag word such as `status` or `mints`), so it
/// matches `--help`, `<sub> --help`, and `<sub> <subsub> --help` at any nesting depth. This is a
/// sole-help check, never a loose scan: a `--help` that follows a FLAG (e.g. an `--agent-argv
/// --help` value, or `--rate-sats 100 --help`) is NOT a help request and reaches the parser
/// unchanged — the property the #549 seller arm was careful to keep.
///
/// Each subcommand calls this at the TOP of its `run`/handler; when true it prints its usage to
/// STDOUT and returns success BEFORE parsing options or taking any side effect — no daemon socket,
/// no relay, no wallet, no home bootstrap (issue #570). #549 fixed only `seller`; this is the
/// general form that closes the class across every subcommand.
pub(crate) fn is_help_request(args: &[String]) -> bool {
    match args.split_last() {
        Some((last, rest)) if last == "--help" => rest.iter().all(|token| !token.starts_with('-')),
        _ => false,
    }
}

fn run_log(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match args {
        [command, path] if command == "replay" => replay_log(path, out, err),
        _ => usage(err),
    }
}

fn replay_log(path: impl AsRef<Path>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let log = match EventLog::open(path) {
        Ok(log) => log,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let replay = log.replay(0);
    for envelope in replay.envelopes {
        if let Err(error) = write_json_line(out, &envelope) {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    }
    match replay.error {
        Some(error) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
        None => SUCCESS,
    }
}

fn run_mock(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match args.first().map(String::as_str) {
        Some("run") => {
            let options = match MockRunOptions::parse(&args[1..]) {
                Ok(options) => options,
                Err(()) => return usage(err),
            };
            mock_run(options, out, err)
        }
        _ => usage(err),
    }
}

#[cfg(not(feature = "acp"))]
fn run_agent(_args: &[String], _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let _ = writeln!(
        err,
        "maxplayer run requires rebuilding with the acp feature: cargo run -p maxplayer --features acp -- run ..."
    );
    USAGE_ERROR
}

#[cfg(feature = "acp")]
fn run_agent(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let options = match RunOptions::parse(args) {
        Ok(options) => options,
        Err(()) => return usage(err),
    };
    let mut driver = AcpDriver::new(
        AgentCommand::new(
            options.agent_command[0].clone(),
            options.agent_command[1..].to_vec(),
        ),
        options.permission_policy.outcome(),
        options.idle_timeout,
    );
    let mut log = match EventLog::open(&options.log) {
        Ok(log) => log,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };

    let params = RunParams {
        session_config: SessionConfig {
            cwd: options.cwd,
            mcp_servers: Vec::new(),
            env: Vec::new(),
        },
        prompt: PromptTurn {
            input: vec![ContentBlock::Text { text: options.task }],
        },
    };

    // The ACP driver's waits are tokio-timed (they must YIELD so N seller jobs can share one
    // thread — issue #223), so this path needs a REAL tokio runtime: the noop-waker
    // `crate::exec::block_on` has no timer or blocking pool and would panic inside
    // `tokio::time::timeout`. The mock path keeps `exec::block_on` (no tokio primitives).
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "failed to build tokio runtime: {error}");
            return RUNTIME_ERROR;
        }
    };
    let mut write_error = None;
    let result = runtime.block_on(run_job(
        &mut driver,
        &mut log,
        &options.job_id,
        params,
        &mut |event| match event {
            RunEvent::Update(update) => {
                if write_error.is_none()
                    && let Err(error) = write_json_line(out, update)
                {
                    write_error = Some(error.to_string());
                }
            }
            RunEvent::PermissionDecided { outcome, .. } => {
                if write_error.is_none()
                    && let Err(error) =
                        write_json_line(out, &PermissionOutcomeLine::new(outcome.clone()))
                {
                    write_error = Some(error.to_string());
                }
            }
        },
    ));

    match (result, write_error) {
        (Ok(_), None) => SUCCESS,
        (_, Some(error)) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
        (Err(error), None) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

fn mock_run(options: MockRunOptions, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let script = match read_script(&options.script) {
        Ok(script) => script,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };
    let permission_outcomes =
        vec![options.permission_policy.outcome(); count_permission_requests(&script.updates)];
    let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script])
        .with_permission_outcomes(permission_outcomes);
    let mut log = match EventLog::open(&options.log) {
        Ok(log) => log,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return RUNTIME_ERROR;
        }
    };

    let mut write_error = None;
    let result = crate::exec::block_on(run_job(
        &mut driver,
        &mut log,
        &options.job_id,
        RunParams::mock_defaults(),
        &mut |event| match event {
            RunEvent::Update(update) => {
                if write_error.is_none()
                    && let Err(error) = write_json_line(out, update)
                {
                    write_error = Some(error.to_string());
                }
            }
            RunEvent::PermissionDecided { outcome, .. } => {
                if write_error.is_none()
                    && let Err(error) =
                        write_json_line(out, &PermissionOutcomeLine::new(outcome.clone()))
                {
                    write_error = Some(error.to_string());
                }
            }
        },
    ));

    match (result, write_error) {
        (Ok(_), None) => SUCCESS,
        (_, Some(error)) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
        (Err(error), None) => {
            let _ = writeln!(err, "{error}");
            RUNTIME_ERROR
        }
    }
}

fn read_script(path: impl AsRef<Path>) -> Result<ScriptedSession, String> {
    let bytes = fs::read(path.as_ref())
        .map_err(|error| format!("failed to read script {}: {error}", path.as_ref().display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "failed to decode script {}: {error}",
            path.as_ref().display()
        )
    })
}

fn count_permission_requests(updates: &[SessionUpdate]) -> usize {
    updates
        .iter()
        .filter(|update| matches!(update, SessionUpdate::PermissionRequest(_)))
        .count()
}

fn write_json_line<T: Serialize + ?Sized>(out: &mut dyn Write, value: &T) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")
}

fn usage(err: &mut dyn Write) -> i32 {
    write_usage(err);
    USAGE_ERROR
}

/// Help that was asked for goes to stdout and succeeds; the same text on stderr means the
/// invocation was wrong. Only the destination and the exit code differ, so both paths render here.
fn write_usage(out: &mut dyn Write) {
    let _ = write!(
        out,
        "Usage:\n  maxplayer [--help | --version]\n  maxplayer version\n  maxplayer mcp\n  maxplayer buyer     # persistent per-home daemon (exclusive lock, unix-socket RPC); `maxplayer buyer status` = thin client\n  maxplayer doctor   # seller environment self-check (git, credential helper, relay, mint, agent)\n  maxplayer wallet <setup|balance|mint|mint-complete|send|receive|melt|invoice|mints|reconcile> ...\n  maxplayer profile set [--name <name>] [--about <about>]   # publish kind-0 identity\n  maxplayer whoami [--home <dir>]   # print this seat's public identity (hex pubkey, npub, resolved home)\n"
    );
    #[cfg(feature = "stub-pay")]
    let _ = write!(
        out,
        "  maxplayer stub-pay <amount_sats>   # exercise the config-bound budget gate\n"
    );
    // The seller surface is listed only when it is compiled in (`acp`); a buyer-only build must not
    // advertise a command that would publish a seat it cannot deliver on (#360). `run`/`mock` stay
    // on every build — they fail honestly with a rebuild hint and never publish anything.
    #[cfg(feature = "acp")]
    let _ = write!(
        out,
        "  maxplayer seller --agent <claude|cursor|codex> --rate-sats <n> [--git-remote <url>] [--claim-open-pool] [--accept-open-targeted]\n  maxplayer seller   # zero-prompt relaunch from config.toml\n"
    );
    let _ = writeln!(
        out,
        "  maxplayer accept <job_id> <claim_id> [--result-id <id>]   # buyer: bind a delivered result (collect folds this in)\n  maxplayer collect <job_id> [--out <folder>]   # buyer: accept-if-needed + verify + pay + materialize\n  maxplayer log replay <path>\n  maxplayer mock run --script <path> --log <path> [--job-id <id>] [--permission-policy allow|deny]\n  maxplayer run --agent-command <cmd> --task <text> --log <path> [--cwd <dir>] [--job-id <id>] [--permission-policy allow|allow-always|deny] [--idle-timeout <secs>]\n\nExit codes: 0 success, 1 usage error, 2 runtime error"
    );
}

struct MockRunOptions {
    script: PathBuf,
    log: PathBuf,
    job_id: JobId,
    permission_policy: PermissionPolicy,
}

impl MockRunOptions {
    fn parse(args: &[String]) -> Result<Self, ()> {
        let mut script = None;
        let mut log = None;
        let mut job_id = JobId("job-1".into());
        let mut permission_policy = PermissionPolicy::Allow;
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--script" => {
                    index += 1;
                    script = args.get(index).map(PathBuf::from);
                }
                "--log" => {
                    index += 1;
                    log = args.get(index).map(PathBuf::from);
                }
                "--job-id" => {
                    index += 1;
                    job_id = JobId(args.get(index).ok_or(())?.clone());
                }
                "--permission-policy" => {
                    index += 1;
                    permission_policy = PermissionPolicy::parse(args.get(index).ok_or(())?)?;
                }
                _ => return Err(()),
            }
            index += 1;
        }

        Ok(Self {
            script: script.ok_or(())?,
            log: log.ok_or(())?,
            job_id,
            permission_policy,
        })
    }
}

#[cfg(feature = "acp")]
struct RunOptions {
    agent_command: Vec<String>,
    task: String,
    log: PathBuf,
    cwd: PathBuf,
    job_id: JobId,
    permission_policy: PermissionPolicy,
    idle_timeout: Duration,
}

#[cfg(feature = "acp")]
impl RunOptions {
    fn parse(args: &[String]) -> Result<Self, ()> {
        let mut agent_command = None;
        let mut task = None;
        let mut log = None;
        let mut cwd = None;
        let mut job_id = JobId("job-1".into());
        let mut permission_policy = PermissionPolicy::Allow;
        let mut idle_timeout = Duration::from_secs(300);
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--agent-command" => {
                    index += 1;
                    agent_command = args.get(index).map(|value| {
                        value
                            .split_whitespace()
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    });
                }
                "--task" => {
                    index += 1;
                    task = args.get(index).cloned();
                }
                "--log" => {
                    index += 1;
                    log = args.get(index).map(PathBuf::from);
                }
                "--cwd" => {
                    index += 1;
                    cwd = args.get(index).map(PathBuf::from);
                }
                "--job-id" => {
                    index += 1;
                    job_id = JobId(args.get(index).ok_or(())?.clone());
                }
                "--permission-policy" => {
                    index += 1;
                    permission_policy = PermissionPolicy::parse(args.get(index).ok_or(())?)?;
                }
                "--idle-timeout" => {
                    index += 1;
                    idle_timeout =
                        Duration::from_secs(args.get(index).ok_or(())?.parse().map_err(|_| ())?);
                }
                _ => return Err(()),
            }
            index += 1;
        }

        let agent_command = agent_command.ok_or(())?;
        if agent_command.is_empty() {
            return Err(());
        }

        Ok(Self {
            agent_command,
            task: task.ok_or(())?,
            log: log.ok_or(())?,
            cwd: cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into())),
            job_id,
            permission_policy,
            idle_timeout,
        })
    }
}

#[derive(Clone, Copy)]
enum PermissionPolicy {
    Allow,
    AllowAlways,
    Deny,
}

impl PermissionPolicy {
    fn parse(value: &str) -> Result<Self, ()> {
        match value {
            "allow" => Ok(Self::Allow),
            "allow-always" => Ok(Self::AllowAlways),
            "deny" => Ok(Self::Deny),
            _ => Err(()),
        }
    }

    fn outcome(self) -> PermissionOutcome {
        match self {
            Self::Allow => PermissionOutcome::Allow,
            Self::AllowAlways => PermissionOutcome::AllowAlways,
            Self::Deny => PermissionOutcome::Deny,
        }
    }
}

#[derive(Serialize)]
struct PermissionOutcomeLine {
    #[serde(rename = "type")]
    outcome_type: &'static str,
    outcome: PermissionOutcome,
}

impl PermissionOutcomeLine {
    fn new(outcome: PermissionOutcome) -> Self {
        Self {
            outcome_type: "permission_outcome",
            outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use maxplayer_core::Envelope;
    use maxplayer_core::driver::{
        Artifact, ContentBlock, PermissionRequest, ScriptedSession, SessionUpdate, StopReason,
    };
    use maxplayer_core::event::{ArtifactId, Event, JobExecutionStatus, JobId, RuntimeId};
    use serde_json::{Value, json};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn usage_and_version() {
        let (code, out, err) = run_captured(["maxplayer"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("Usage:"));

        let (code, out, err) = run_captured(["maxplayer", "unknown"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("Usage:"));

        let (code, out, err) = run_captured(["maxplayer", "version"]);
        assert_eq!(code, 0);
        assert_eq!(out, format!("maxplayer {}\n", maxplayer_core::version()));
        assert!(err.is_empty());
    }

    #[test]
    fn help_and_version_flags_succeed_on_stdout() {
        let (code, out, err) = run_captured(["maxplayer", "--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("Usage:"));
        assert!(err.is_empty());

        let (code, out, err) = run_captured(["maxplayer", "--version"]);
        assert_eq!(code, 0);
        assert_eq!(out, format!("maxplayer {}\n", maxplayer_core::version()));
        assert!(err.is_empty());

        // Trailing arguments are still a usage error, so `--help` cannot swallow a mistyped command.
        let (code, out, err) = run_captured(["maxplayer", "--help", "extra"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("Usage:"));
    }

    // ---- #570: `--help` on every subcommand exits 0 with usage on stdout and no side effects ----

    /// Drive `cli::run` with an arbitrary-length argv tail (unlike the fixed-size `run_captured`).
    fn help_captured(tail: &[&str]) -> (i32, String, String) {
        let mut argv = vec!["maxplayer"];
        argv.extend_from_slice(tail);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("stdout utf8"),
            String::from_utf8(err).expect("stderr utf8"),
        )
    }

    // The sole-help predicate every subcommand relies on: `--help` as the final token, preceded only
    // by non-flag subcommand selectors — never a `--help` that is itself a FLAG VALUE (the property
    // the #549 seller arm guarded for `--agent-argv --help`).
    #[test]
    fn is_help_request_matches_sole_help_at_any_depth_only() {
        let s = |xs: &[&str]| xs.iter().map(|x| (*x).to_owned()).collect::<Vec<String>>();
        assert!(is_help_request(&s(&["--help"])));
        assert!(is_help_request(&s(&["status", "--help"])));
        assert!(is_help_request(&s(&["mints", "add", "--help"])));
        // A `--help` that follows a flag is that flag's VALUE, not a help request.
        assert!(!is_help_request(&s(&["--agent-argv", "--help"])));
        assert!(!is_help_request(&s(&["--rate-sats", "100", "--help"])));
        // `--help` must be the final and only flag token.
        assert!(!is_help_request(&s(&["--help", "extra"])));
        assert!(!is_help_request(&s(&[])));
        assert!(!is_help_request(&s(&["status"])));
    }

    // The shape #570 is about: a sole `--help` on ANY registered subcommand — at every nesting depth
    // (`buyer status`, `wallet mints add`) — prints that command's usage to STDOUT and exits 0 with
    // nothing on stderr (no parse, no home bootstrap, no daemon socket). #549 fixed only `seller`;
    // enumerating the whole advertised surface here means a subcommand added later WITHOUT a
    // sole-`--help` arm fails this test rather than shipping the regression a third time. Data-driven
    // (one assertion body, many inputs) so it is the SHAPE under test, not a few instances. The
    // per-case marker also proves each command answered from ITS OWN usage, not a generic fallthrough.
    #[test]
    fn help_on_every_subcommand_prints_usage_to_stdout_and_exits_zero() {
        // (argv tail after `maxplayer`, a string that must appear in the command's own usage)
        #[allow(unused_mut)] // feature-gated pushes below may be compiled out on a buyer-only build
        let mut cases: Vec<(&str, &str)> = vec![
            ("buyer --help", "maxplayer buyer"),
            ("buyer serve --help", "maxplayer buyer"),
            ("buyer status --help", "maxplayer buyer"),
            ("wallet --help", "maxplayer wallet"),
            ("wallet setup --help", "maxplayer wallet"),
            ("wallet balance --help", "maxplayer wallet"),
            ("wallet mint --help", "maxplayer wallet"),
            ("wallet mint-complete --help", "maxplayer wallet"),
            ("wallet send --help", "maxplayer wallet"),
            ("wallet receive --help", "maxplayer wallet"),
            ("wallet melt --help", "maxplayer wallet"),
            ("wallet invoice --help", "maxplayer wallet"),
            ("wallet mints --help", "maxplayer wallet"),
            ("wallet mints list --help", "maxplayer wallet"),
            ("wallet mints add --help", "maxplayer wallet"),
            ("wallet reconcile --help", "maxplayer wallet"),
            ("doctor --help", "maxplayer doctor"),
            ("profile --help", "maxplayer profile"),
            ("profile set --help", "maxplayer profile"),
            ("whoami --help", "maxplayer whoami"),
            ("accept --help", "maxplayer accept"),
            ("collect --help", "maxplayer collect"),
            ("mcp --help", "maxplayer mcp"),
            ("log --help", "maxplayer log replay"),
            ("log replay --help", "maxplayer log replay"),
            ("mock --help", "maxplayer mock run"),
            ("mock run --help", "maxplayer mock run"),
            ("run --help", "maxplayer run"),
        ];
        // Feature-gated surfaces are enumerated only on the builds that register them (mirroring the
        // dispatch arms in `run`), so this tracks the surface a given build actually ships.
        #[cfg(feature = "acp")]
        cases.push(("seller --help", "maxplayer seller"));
        #[cfg(feature = "wallet")]
        cases.push(("sandbox-probe --help", "maxplayer sandbox-probe"));
        #[cfg(feature = "stub-pay")]
        cases.push(("stub-pay --help", "maxplayer stub-pay"));

        for (line, marker) in cases {
            let tail: Vec<&str> = line.split(' ').collect();
            let (code, out, err) = help_captured(&tail);
            assert_eq!(code, 0, "`maxplayer {line}` must exit 0\nstdout={out}\nstderr={err}");
            assert!(
                out.contains("Usage:"),
                "`maxplayer {line}` must print usage to stdout:\nstdout={out}\nstderr={err}"
            );
            assert!(
                out.contains(marker),
                "`maxplayer {line}` must answer from its own usage (expected {marker:?}):\nstdout={out}"
            );
            assert!(
                err.is_empty(),
                "`maxplayer {line}` must have no side-effect output on stderr:\nstderr={err}"
            );
        }
    }

    // #570, the worst instance: `buyer status --help` must answer from usage WITHOUT opening the
    // daemon socket — a help request never touches the network. Run with no `--home` and no daemon:
    // the pre-fix code fell through to `status()` → `client::status(socket)`, which prints neither
    // "Usage:" nor leaves stderr empty (it connects and dumps status JSON, or fails to connect with a
    // socket error + non-zero exit). Exit 0 + buyer usage on stdout + empty stderr red-proves the
    // short-circuit: revert the buyer `--help` arm and this goes red, whether or not a daemon is up.
    #[test]
    fn buyer_status_help_never_contacts_the_daemon() {
        let (code, out, err) = run_captured(["maxplayer", "buyer", "status", "--help"]);
        assert_eq!(code, 0, "buyer status --help must exit 0:\nstdout={out}\nstderr={err}");
        assert!(
            out.contains("Usage:") && out.contains("maxplayer buyer status"),
            "must print buyer usage to stdout, not connect:\nstdout={out}"
        );
        assert!(
            err.is_empty(),
            "buyer status --help must not attempt a daemon connection (no socket error, no banner):\nstderr={err}"
        );
    }

    #[test]
    fn buyer_serve_with_home_flag_refuses_instead_of_silently_ignoring_it() {
        // #438: this must NOT silently start the daemon on $MAXPLAYER_HOME/~/.maxplayer.
        let (code, out, err) = run_captured([
            "maxplayer",
            "buyer",
            "serve",
            "--home",
            "/tmp/should-not-be-used",
        ]);
        assert_eq!(
            code, 1,
            "must refuse (usage error), not start the daemon:\nstdout={out}\nstderr={err}"
        );
        assert!(
            out.is_empty(),
            "no daemon-online banner on stdout:\nstdout={out}"
        );
        assert!(
            err.contains("--home") && err.contains("MAXPLAYER_HOME"),
            "error must name both --home and the sanctioned MAXPLAYER_HOME mechanism:\nstderr={err}"
        );
    }

    #[test]
    fn buyer_status_with_home_flag_refuses_instead_of_silently_ignoring_it() {
        let (code, out, err) = run_captured([
            "maxplayer",
            "buyer",
            "status",
            "--home",
            "/tmp/should-not-be-used",
        ]);
        assert_eq!(
            code, 1,
            "must refuse, not query a daemon on the wrong home:\nstdout={out}\nstderr={err}"
        );
        assert!(out.is_empty());
        assert!(err.contains("--home") && err.contains("MAXPLAYER_HOME"));
    }

    #[test]
    fn buyer_serve_with_home_equals_form_refuses_the_same_way() {
        // #654 review on #438: `--home=<dir>` bypassed the exact-string match and serve() ran on
        // $MAXPLAYER_HOME — the identical silent divergence, not a variant. Equals form must get
        // the same refusal as the space-separated form.
        let (code, out, err) = run_captured([
            "maxplayer",
            "buyer",
            "serve",
            "--home=/tmp/should-not-be-used",
        ]);
        assert_eq!(
            code, 1,
            "equals form must refuse, not start the daemon:\nstdout={out}\nstderr={err}"
        );
        assert!(out.is_empty(), "no daemon-online banner on stdout:\nstdout={out}");
        assert!(
            err.contains("--home") && err.contains("MAXPLAYER_HOME"),
            "error must name both --home and MAXPLAYER_HOME:\nstderr={err}"
        );
    }

    #[test]
    fn buyer_status_with_home_equals_form_refuses_the_same_way() {
        let (code, out, err) = run_captured([
            "maxplayer",
            "buyer",
            "status",
            "--home=/tmp/should-not-be-used",
        ]);
        assert_eq!(code, 1, "stdout={out}\nstderr={err}");
        assert!(out.is_empty());
        assert!(err.contains("--home") && err.contains("MAXPLAYER_HOME"));
    }

    #[test]
    fn buyer_serve_with_stray_argument_refuses_instead_of_swallowing_it() {
        // #654 review: serve/status take zero trailing args, so any unrecognized arg is refused
        // (the wallet CLI's catch-all property) — never silently ignored.
        let (code, out, err) = run_captured(["maxplayer", "buyer", "serve", "extra"]);
        assert_eq!(
            code, 1,
            "a stray argument must refuse, not be swallowed:\nstdout={out}\nstderr={err}"
        );
        assert!(out.is_empty());
        assert!(
            err.contains("unknown argument") && err.contains("extra"),
            "error must name the offending argument:\nstderr={err}"
        );
    }

    #[test]
    fn buyer_bare_with_home_flag_refuses_with_the_clear_message() {
        // Case B from #438 — already refused pre-fix via the generic usage fallthrough;
        // this pins that it now gets the SAME explanatory message as serve/status, not
        // plain "Usage:" text.
        let (code, out, err) = run_captured([
            "maxplayer",
            "buyer",
            "--home",
            "/tmp/should-not-be-used",
        ]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("--home") && err.contains("MAXPLAYER_HOME"));
    }

    // #360: the seller advertise surface is gated on `acp`. These two tests are the same assertion
    // read from opposite feature builds — the verdict must MOVE with the feature, which is what
    // proves the gate binds `seller` rather than something incidental.
    #[cfg(not(feature = "acp"))]
    #[test]
    fn sell_is_absent_from_the_buyer_surface() {
        // Not advertised in help — a buyer-only build never names a command it cannot honour.
        let (code, out, _err) = run_captured(["maxplayer", "--help"]);
        assert_eq!(code, 0);
        assert!(
            !out.contains("maxplayer seller"),
            "buyer build must not list `seller` in usage:\n{out}"
        );
        // Invoking it cannot boot the seller: it is a plain usage error, identical to any unknown
        // command, and never reaches the discoverability/heartbeat publish.
        let (code, out, err) = run_captured(["maxplayer", "seller", "--agent", "claude", "--rate-sats", "100"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("Usage:"));
    }

    #[cfg(feature = "acp")]
    #[test]
    fn sell_is_present_on_the_seller_surface() {
        let (code, out, _err) = run_captured(["maxplayer", "--help"]);
        assert_eq!(code, 0);
        assert!(
            out.contains("maxplayer seller"),
            "acp build must list `seller` in usage:\n{out}"
        );
    }

    // #533: the top-level `--help` names `doctor` with the same "seller environment self-check"
    // wording as doctor.rs's own header — the two surfaces must not drift apart again (this line
    // used to read "runner environment self-check"). A label pin in the #528 style, on the base
    // usage so it holds on every feature build.
    #[test]
    fn doctor_help_line_uses_seller_wording() {
        let (code, out, _err) = run_captured(["maxplayer", "--help"]);
        assert_eq!(code, 0);
        assert!(
            out.contains("# seller environment self-check"),
            "top-level --help must describe doctor as the seller environment self-check:\n{out}"
        );
        assert!(
            !out.contains("runner environment self-check"),
            "the retired \"runner environment self-check\" wording must not reappear:\n{out}"
        );
    }

    // No-alias (cleancut): the RETIRED `sell` subcommand must not dispatch. There is no `sell` arm,
    // so it falls through to a plain usage error like any unknown command, on every build. The arm is
    // gone structurally; this makes the property red-provable — re-pointing the dispatch at `sell`
    // (or adding a `sell` alias) fails it. Guards the blind-rename-of-the-foil hazard: the old
    // buyer-surface test's argv was renamed to `seller`, leaving no assertion that `sell` is rejected.
    #[test]
    fn retired_sell_subcommand_is_rejected_no_alias() {
        let (code, out, err) = run_captured(["maxplayer", "sell", "--agent", "claude", "--rate-sats", "100"]);
        assert_eq!(code, 1, "`maxplayer sell` must be a usage error, never a dispatch");
        assert!(out.is_empty(), "`maxplayer sell` must print nothing to stdout:\n{out}");
        assert!(
            err.contains("Usage:"),
            "`maxplayer sell` must fall through to usage, not boot a seller:\n{err}"
        );
    }

    // #549: `maxplayer seller --help` must print the seller usage to stdout and exit 0. The parser's
    // catch-all had no `--help` arm, so `--help` was reported as an unknown option (empty stdout,
    // exit 1). acp-gated because the `seller` dispatch only exists on the seller build (`mod sell`).
    #[cfg(feature = "acp")]
    #[test]
    fn seller_help_prints_usage_and_succeeds() {
        let (code, out, err) = run_captured(["maxplayer", "seller", "--help"]);
        assert_eq!(
            code, 0,
            "`maxplayer seller --help` must succeed:\nstdout={out}\nstderr={err}"
        );
        assert!(
            out.contains("Usage:") && out.contains("maxplayer seller"),
            "`seller --help` must print the seller usage to stdout:\n{out}"
        );
        // Rename-completeness: no surface may name the retired `sell` command to the user, only
        // `seller`. A token-level check so `seller` itself does not trip it.
        let names_retired_sell = |text: &str| {
            text.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|token| token == "sell")
        };
        assert!(
            !names_retired_sell(&out) && !names_retired_sell(&err),
            "seller --help must not name the retired `sell` (only `seller`):\nstdout={out}\nstderr={err}"
        );

        // Sole `--help` only (top-level `args.len() == 2` semantics): `--help` alongside other args
        // must NOT short-circuit to help — it reaches the parser. Guards against a looser scan that
        // would swallow a `--help` meant as an `--agent-argv` value for the agent. Boot-safe: the
        // parser rejects `--help` as an unknown option before any relay/home/key path.
        let (code_ns, out_ns, _err_ns) =
            run_captured(["maxplayer", "seller", "--help", "--rate-sats", "100"]);
        assert_ne!(
            code_ns, 0,
            "non-sole `--help` must not short-circuit to help-success:\nstdout={out_ns}"
        );
        assert!(
            out_ns.is_empty(),
            "non-sole `--help` must not print usage to stdout:\n{out_ns}"
        );
    }

    #[test]
    fn replay_renders_log_envelopes_in_order() {
        let path = test_path("replay-renders");
        let mut log = EventLog::open(&path).expect("open log");
        log.append(Event::DriverReady {
            runtime_id: RuntimeId("mock".into()),
        })
        .expect("append ready");
        log.append(Event::JobExecutionChanged {
            job_id: JobId("job-1".into()),
            status: JobExecutionStatus::Queued,
        })
        .expect("append queued");

        let (code, out, err) = run_captured(["maxplayer", "log", "replay", path.to_str().unwrap()]);
        assert_eq!(code, 0);
        assert!(err.is_empty());
        let envelopes = parse_lines::<Envelope>(&out);
        assert_eq!(
            envelopes
                .iter()
                .map(|envelope| envelope.seq)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            envelopes
                .into_iter()
                .map(|envelope| envelope.payload)
                .collect::<Vec<_>>(),
            vec![
                Event::DriverReady {
                    runtime_id: RuntimeId("mock".into())
                },
                Event::JobExecutionChanged {
                    job_id: JobId("job-1".into()),
                    status: JobExecutionStatus::Queued
                },
            ]
        );
    }

    #[test]
    fn replay_surfaces_corrupt_tail_after_valid_envelopes() {
        let path = test_path("replay-corrupt-tail");
        let mut log = EventLog::open(&path).expect("open log");
        log.append(Event::DriverReady {
            runtime_id: RuntimeId("mock".into()),
        })
        .expect("append ready");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open append")
            .write_all(b"{not json}\n")
            .expect("write corrupt tail");

        let (code, out, err) = run_captured(["maxplayer", "log", "replay", path.to_str().unwrap()]);
        assert_eq!(code, 2);
        assert!(err.contains("failed to decode event envelope"));
        let envelopes = parse_lines::<Envelope>(&out);
        assert_eq!(envelopes.len(), 1);
        assert_eq!(
            envelopes[0].payload,
            Event::DriverReady {
                runtime_id: RuntimeId("mock".into())
            }
        );
    }

    #[test]
    fn mock_run_happy_path_prints_updates_and_writes_replayable_log() {
        let script = test_path("happy-script");
        let log = test_path("happy-log");
        write_script(
            &script,
            ScriptedSession {
                session_id: "session-1".into(),
                updates: vec![
                    SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                        text: "working".into(),
                    }]),
                    SessionUpdate::Plan {
                        entries: vec!["finish".into()],
                    },
                    SessionUpdate::TurnEnded(StopReason::Completed),
                ],
                artifacts: vec![Artifact {
                    uri_or_path: "out/result.txt".into(),
                    mime: Some("text/plain".into()),
                    bytes: None,
                }],
            },
        );

        let (code, out, err) = run_captured([
            "maxplayer",
            "mock",
            "run",
            "--script",
            script.to_str().unwrap(),
            "--log",
            log.to_str().unwrap(),
        ]);

        assert_eq!(code, 0);
        assert!(err.is_empty());
        let updates = parse_lines::<SessionUpdate>(&out);
        assert_eq!(
            updates,
            vec![
                SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                    text: "working".into()
                }]),
                SessionUpdate::Plan {
                    entries: vec!["finish".into()]
                },
                SessionUpdate::TurnEnded(StopReason::Completed),
            ]
        );
        assert_eq!(
            replay_payloads(&log),
            vec![
                Event::DriverReady {
                    runtime_id: RuntimeId("mock".into())
                },
                Event::JobExecutionChanged {
                    job_id: JobId("job-1".into()),
                    status: JobExecutionStatus::Queued
                },
                Event::JobExecutionChanged {
                    job_id: JobId("job-1".into()),
                    status: JobExecutionStatus::Running
                },
                Event::AgentMessage {
                    job_id: JobId("job-1".into()),
                    text: "working".into()
                },
                Event::JobExecutionChanged {
                    job_id: JobId("job-1".into()),
                    status: JobExecutionStatus::Completed
                },
                Event::ArtifactProduced {
                    artifact_id: ArtifactId("out/result.txt".into())
                },
            ]
        );
    }

    #[test]
    fn permission_request_routes_deny_outcome() {
        let script = test_path("deny-script");
        let log = test_path("deny-log");
        write_script(
            &script,
            ScriptedSession {
                session_id: "session-1".into(),
                updates: vec![
                    SessionUpdate::PermissionRequest(PermissionRequest {
                        tool: "shell".into(),
                        detail: json!({"cmd": "false"}),
                    }),
                    SessionUpdate::TurnEnded(StopReason::Completed),
                ],
                artifacts: Vec::new(),
            },
        );

        let (code, out, err) = run_captured([
            "maxplayer",
            "mock",
            "run",
            "--script",
            script.to_str().unwrap(),
            "--log",
            log.to_str().unwrap(),
            "--permission-policy",
            "deny",
        ]);

        assert_eq!(code, 0);
        assert!(err.is_empty());
        let lines = parse_lines::<Value>(&out);
        assert_eq!(
            lines[1],
            json!({"type": "permission_outcome", "outcome": "deny"})
        );
    }

    #[test]
    fn failed_turn_maps_to_failed_execution_status() {
        let script = test_path("failed-script");
        let log = test_path("failed-log");
        write_script(
            &script,
            ScriptedSession {
                session_id: "session-1".into(),
                updates: vec![SessionUpdate::TurnEnded(StopReason::Failed)],
                artifacts: Vec::new(),
            },
        );

        let (code, _out, err) = run_captured([
            "maxplayer",
            "mock",
            "run",
            "--script",
            script.to_str().unwrap(),
            "--log",
            log.to_str().unwrap(),
        ]);

        assert_eq!(code, 0);
        assert!(err.is_empty());
        assert_eq!(
            replay_payloads(&log).last(),
            Some(&Event::JobExecutionChanged {
                job_id: JobId("job-1".into()),
                status: JobExecutionStatus::Failed
            })
        );
    }

    /// `whoami` dispatches through the real `Some("whoami")` arm and prints the seat's PUBLIC
    /// identity — hex pubkey, npub (bech32), and the resolved `--home` — for a given home.
    #[cfg(feature = "wallet")]
    #[test]
    fn whoami_prints_pubkey_npub_and_resolved_home() {
        let home = test_home("whoami-prints");
        let (code, out, err) = run_captured([
            "maxplayer",
            "whoami",
            "--home",
            home.to_str().unwrap(),
        ]);

        assert_eq!(code, 0, "stderr: {err}");
        assert!(err.is_empty(), "stderr: {err}");

        // The resolved home path echoed back is exactly the --home we passed.
        assert!(
            out.contains(&home.display().to_string()),
            "output should echo resolved home {}, got:\n{out}",
            home.display()
        );

        // hex pubkey: 64 hex chars.
        let pubkey = out
            .lines()
            .find_map(|line| line.strip_prefix("pubkey:"))
            .expect("pubkey line")
            .trim();
        assert_eq!(pubkey.len(), 64, "hex pubkey is 32 bytes: {pubkey}");
        assert!(
            pubkey.chars().all(|c| c.is_ascii_hexdigit()),
            "pubkey is hex: {pubkey}"
        );

        // npub: bech32 with the `npub1` HRP.
        let npub = out
            .lines()
            .find_map(|line| line.strip_prefix("npub:"))
            .expect("npub line")
            .trim();
        assert!(npub.starts_with("npub1"), "npub is bech32 npub: {npub}");
    }

    /// RED-PROVE (critical security property): `whoami` must NEVER leak key material. Read the
    /// packaged secret straight off disk and assert none of it — nor any `nsec` — appears in the
    /// command output.
    #[cfg(feature = "wallet")]
    #[test]
    fn whoami_output_never_contains_secret_key() {
        let home = test_home("whoami-nosecret");
        let (code, out, err) = run_captured([
            "maxplayer",
            "whoami",
            "--home",
            home.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "stderr: {err}");

        // bootstrap wrote the secret to `<home>/key` (0600). Read it directly, then red-prove it
        // is absent from stdout.
        let secret_hex = fs::read_to_string(home.join("key"))
            .expect("secret key file")
            .trim()
            .to_owned();
        assert!(!secret_hex.is_empty(), "precondition: key file is non-empty");

        assert!(
            !out.contains(&secret_hex),
            "SECURITY: whoami output leaked the hex secret key"
        );
        assert!(
            !out.contains("nsec1"),
            "SECURITY: whoami output contains an nsec"
        );
    }

    #[cfg(feature = "acp")]
    #[test]
    #[ignore = "requires MAXPLAYER_ACP_SMOKE=1 and MAXPLAYER_ACP_SMOKE_CMD"]
    fn acp_smoke_real_agent_command_writes_terminal_log() {
        if std::env::var("MAXPLAYER_ACP_SMOKE").ok().as_deref() != Some("1") {
            eprintln!("set MAXPLAYER_ACP_SMOKE=1 to run the ACP smoke test");
            return;
        }
        let command = match std::env::var("MAXPLAYER_ACP_SMOKE_CMD") {
            Ok(command) => command,
            Err(_) => {
                eprintln!("set MAXPLAYER_ACP_SMOKE_CMD to run the ACP smoke test");
                return;
            }
        };
        let log = test_path("acp-smoke-log");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            [
                "maxplayer".to_owned(),
                "run".to_owned(),
                "--agent-command".to_owned(),
                command,
                "--task".to_owned(),
                "say hello".to_owned(),
                "--log".to_owned(),
                log.to_string_lossy().into_owned(),
                "--idle-timeout".to_owned(),
                "30".to_owned(),
            ],
            &mut out,
            &mut err,
        );

        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
        assert!(replay_payloads(&log).iter().any(|event| {
            matches!(
                event,
                Event::JobExecutionChanged {
                    status: JobExecutionStatus::Completed
                        | JobExecutionStatus::Failed
                        | JobExecutionStatus::Cancelled,
                    ..
                }
            )
        }));
    }

    fn run_captured<const N: usize>(args: [&str; N]) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("stdout utf8"),
            String::from_utf8(err).expect("stderr utf8"),
        )
    }

    fn write_script(path: &Path, script: ScriptedSession) {
        let json = serde_json::to_vec(&script).expect("encode script");
        fs::write(path, json).expect("write script");
    }

    fn replay_payloads(path: &Path) -> Vec<Event> {
        let log = EventLog::open(path).expect("open log");
        let replay = log.replay(0);
        assert_eq!(replay.error, None);
        replay
            .envelopes
            .into_iter()
            .map(|envelope| envelope.payload)
            .collect()
    }

    fn parse_lines<T: serde::de::DeserializeOwned>(lines: &str) -> Vec<T> {
        lines
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect()
    }

    fn test_path(name: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-cli-{name}-{}-{id}.jsonl",
            std::process::id()
        ))
    }

    #[cfg(feature = "wallet")]
    fn test_home(name: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("maxplayer-cli-home-{name}-{}-{id}", std::process::id()))
    }
}
