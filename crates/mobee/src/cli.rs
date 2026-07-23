use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(feature = "acp")]
use std::time::Duration;

use mobee_core::EventLog;
#[cfg(feature = "acp")]
use mobee_core::driver::{AcpDriver, AgentCommand};
#[cfg(feature = "acp")]
use mobee_core::driver::{ContentBlock, PromptTurn, SessionConfig};
use mobee_core::driver::{MockDriver, PermissionOutcome, ScriptedSession, SessionUpdate};
use mobee_core::engine::{RunEvent, RunParams, run_job};
use mobee_core::event::{JobId, RuntimeId};
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
        Some("version") if args.len() == 2 => {
            let _ = writeln!(out, "mobee {}", mobee_core::version());
            SUCCESS
        }
        Some("mcp") if args.len() == 2 => crate::mcp::run(out, err),
        Some("sell") => crate::sell::run(&args[2..], out, err),
        Some("doctor") => crate::doctor::run(&args[2..], out, err),
        Some("wallet") => crate::wallet_cli::run(&args[2..], out, err),
        Some("log") => run_log(&args[2..], out, err),
        Some("mock") => run_mock(&args[2..], out, err),
        Some("run") => run_agent(&args[2..], out, err),
        _ => usage(err),
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
        "mobee run requires rebuilding with the acp feature: cargo run -p mobee --features acp -- run ..."
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

    let mut write_error = None;
    let result = crate::exec::block_on(run_job(
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
    let _ = writeln!(
        err,
        "Usage:\n  mobee version\n  mobee mcp\n  mobee doctor   # seller environment self-check (git, credential helper, relay, mint, agent)\n  mobee wallet <balance|mint|send|receive|melt|invoice|mints> ...\n  mobee sell --agent <claude|cursor|codex> --rate-sats <n> [--git-remote <url>] [--claim-open-pool]\n  mobee sell   # zero-prompt relaunch from config.toml\n  mobee log replay <path>\n  mobee mock run --script <path> --log <path> [--job-id <id>] [--permission-policy allow|deny]\n  mobee run --agent-command <cmd> --task <text> --log <path> [--cwd <dir>] [--job-id <id>] [--permission-policy allow|allow-always|deny] [--idle-timeout <secs>]\n\nExit codes: 0 success, 1 usage error, 2 runtime error"
    );
    USAGE_ERROR
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

    use mobee_core::Envelope;
    use mobee_core::driver::{
        Artifact, ContentBlock, PermissionRequest, ScriptedSession, SessionUpdate, StopReason,
    };
    use mobee_core::event::{ArtifactId, Event, JobExecutionStatus, JobId, RuntimeId};
    use serde_json::{Value, json};

    use super::*;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn usage_and_version() {
        let (code, out, err) = run_captured(["mobee"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("Usage:"));

        let (code, out, err) = run_captured(["mobee", "unknown"]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("Usage:"));

        let (code, out, err) = run_captured(["mobee", "version"]);
        assert_eq!(code, 0);
        assert_eq!(out, format!("mobee {}\n", mobee_core::version()));
        assert!(err.is_empty());
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

        let (code, out, err) = run_captured(["mobee", "log", "replay", path.to_str().unwrap()]);
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

        let (code, out, err) = run_captured(["mobee", "log", "replay", path.to_str().unwrap()]);
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
            "mobee",
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
            "mobee",
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
            "mobee",
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

    #[cfg(feature = "acp")]
    #[test]
    #[ignore = "requires MOBEE_ACP_SMOKE=1 and MOBEE_ACP_SMOKE_CMD"]
    fn acp_smoke_real_agent_command_writes_terminal_log() {
        if std::env::var("MOBEE_ACP_SMOKE").ok().as_deref() != Some("1") {
            eprintln!("set MOBEE_ACP_SMOKE=1 to run the ACP smoke test");
            return;
        }
        let command = match std::env::var("MOBEE_ACP_SMOKE_CMD") {
            Ok(command) => command,
            Err(_) => {
                eprintln!("set MOBEE_ACP_SMOKE_CMD to run the ACP smoke test");
                return;
            }
        };
        let log = test_path("acp-smoke-log");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            [
                "mobee".to_owned(),
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
            "mobee-cli-{name}-{}-{id}.jsonl",
            std::process::id()
        ))
    }
}
