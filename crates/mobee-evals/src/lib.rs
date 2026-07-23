//! Deterministic Mobee eval scenarios.
//!
//! Normal `cargo test -p mobee-evals` runs are hermetic and never write snapshots.
//! To intentionally refresh checked-in snapshots, run:
//!
//! ```text
//! MOBEE_EVALS_BLESS=1 cargo test -p mobee-evals
//! ```

use std::error::Error;
use std::fmt::{self, Display};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mobee_core::EventLog;
use mobee_core::driver::{
    MockDriver, PermissionOutcome, PermissionRequest, ScriptedSession, SessionUpdate,
};
use mobee_core::engine::{RunEvent, RunParams, run_job};
use mobee_core::event::{Event, JobExecutionStatus, JobId, RuntimeId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub job_id: JobId,
    pub script: ScriptedSession,
    pub permission_outcomes: Vec<PermissionOutcome>,
    pub expected_terminal: JobExecutionStatus,
    #[serde(default)]
    pub expect_run_error: bool,
    pub snapshot: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    pub log_payloads: Vec<Event>,
    pub updates: Vec<SessionUpdate>,
    pub permissions: Vec<PermissionDecision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecision {
    pub request: PermissionRequest,
    pub outcome: PermissionOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub grader: &'static str,
    pub detail: String,
}

pub trait Grader {
    fn name(&self) -> &'static str;
    fn grade(&self, scenario: &Scenario, actual: &Transcript) -> Vec<Finding>;
}

#[derive(Debug)]
pub enum EvalError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Log(String),
}

impl Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::Log(error) => write!(f, "{error}"),
        }
    }
}

impl Error for EvalError {}

impl From<std::io::Error> for EvalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for EvalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn scenario_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios")
}

pub fn snapshot_dir() -> PathBuf {
    std::env::var_os("MOBEE_EVALS_SNAPSHOT_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("snapshots"),
        PathBuf::from,
    )
}

pub fn load_scenarios(dir: &Path) -> Result<Vec<Scenario>, EvalError> {
    let mut paths = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();

    paths
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| {
            let bytes = fs::read(&path)?;
            serde_json::from_slice(&bytes).map_err(EvalError::from)
        })
        .collect()
}

pub fn run_scenario(scenario: &Scenario) -> Result<Transcript, Vec<Finding>> {
    let lint_findings = lint_scenario(scenario);
    if !lint_findings.is_empty() {
        return Err(lint_findings);
    }

    let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![scenario.script.clone()])
        .with_permission_outcomes(scenario.permission_outcomes.clone());
    let path = temp_log_path(&scenario.name);
    let mut log = EventLog::open(&path).map_err(|error| {
        vec![Finding {
            grader: "runner",
            detail: error.to_string(),
        }]
    })?;
    let mut updates = Vec::new();
    let mut permissions = Vec::new();

    let result = block_on(run_job(
        &mut driver,
        &mut log,
        &scenario.job_id,
        RunParams::mock_defaults(),
        &mut |event| match event {
            RunEvent::Update(update) => updates.push(update.clone()),
            RunEvent::PermissionDecided { request, outcome } => {
                permissions.push(PermissionDecision {
                    request: request.clone(),
                    outcome: outcome.clone(),
                });
            }
        },
    ));
    let run_error = result.err().map(|error| error.to_string());
    if scenario.expect_run_error != run_error.is_some() {
        return Err(vec![Finding {
            grader: "runner",
            detail: format!(
                "expect_run_error was {}, run error present was {}",
                scenario.expect_run_error,
                run_error.is_some()
            ),
        }]);
    }
    if scenario.expect_run_error && run_error.as_deref().unwrap_or("").is_empty() {
        return Err(vec![Finding {
            grader: "runner",
            detail: "expected non-empty run error message".into(),
        }]);
    }

    let replay = log.replay(0);
    if let Some(error) = replay.error {
        return Err(vec![Finding {
            grader: "runner",
            detail: error.to_string(),
        }]);
    }
    Ok(Transcript {
        log_payloads: replay
            .envelopes
            .into_iter()
            .map(|envelope| envelope.payload)
            .collect(),
        updates,
        permissions,
    })
}

pub fn lint_scenario(scenario: &Scenario) -> Vec<Finding> {
    if scenario.expect_run_error && !scenario.script.artifacts.is_empty() {
        return vec![Finding {
            grader: "scenario-lint",
            detail: "expect_run_error scenarios must not script artifacts".into(),
        }];
    }
    Vec::new()
}

pub fn grade_scenario(
    scenario: &Scenario,
    actual: &Transcript,
    snapshot_root: &Path,
) -> Vec<Finding> {
    let graders: Vec<Box<dyn Grader>> = vec![
        Box::new(TerminalStateGrader),
        Box::new(SequenceSnapshotGrader {
            expected: read_snapshot(snapshot_root, scenario),
        }),
        Box::new(ArtifactGrader),
        Box::new(PermissionGrader),
    ];
    graders
        .into_iter()
        .flat_map(|grader| grader.grade(scenario, actual))
        .collect()
}

pub fn run_and_grade(
    scenario: &Scenario,
    snapshot_root: &Path,
    bless: bool,
) -> Result<(), Vec<Finding>> {
    let actual = run_scenario(scenario)?;
    if bless {
        write_snapshot(snapshot_root, scenario, &actual).map_err(|error| {
            vec![Finding {
                grader: "sequence-snapshot",
                detail: error.to_string(),
            }]
        })?;
        return Ok(());
    }

    let findings = grade_scenario(scenario, &actual, snapshot_root);
    if findings.is_empty() {
        Ok(())
    } else {
        Err(findings)
    }
}

pub fn read_snapshot(snapshot_root: &Path, scenario: &Scenario) -> Result<Transcript, EvalError> {
    let bytes = fs::read(snapshot_root.join(&scenario.snapshot))?;
    serde_json::from_slice(&bytes).map_err(EvalError::from)
}

pub fn write_snapshot(
    snapshot_root: &Path,
    scenario: &Scenario,
    transcript: &Transcript,
) -> Result<(), EvalError> {
    fs::create_dir_all(snapshot_root)?;
    let bytes = serde_json::to_vec_pretty(transcript)?;
    fs::write(
        snapshot_root.join(&scenario.snapshot),
        [bytes, b"\n".to_vec()].concat(),
    )?;
    Ok(())
}

pub struct TerminalStateGrader;

impl Grader for TerminalStateGrader {
    fn name(&self) -> &'static str {
        "terminal-state"
    }

    fn grade(&self, scenario: &Scenario, actual: &Transcript) -> Vec<Finding> {
        let terminal_positions = actual
            .log_payloads
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                Event::JobExecutionChanged { status, .. } if is_terminal(status) => {
                    Some((index, status))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut findings = Vec::new();
        if terminal_positions.len() != 1 {
            findings.push(Finding {
                grader: self.name(),
                detail: format!(
                    "expected 1 terminal execution event, found {}",
                    terminal_positions.len()
                ),
            });
            return findings;
        }

        let (terminal_index, status) = terminal_positions[0];
        if status != &scenario.expected_terminal {
            findings.push(Finding {
                grader: self.name(),
                detail: format!(
                    "expected terminal {:?}, found {:?}",
                    scenario.expected_terminal, status
                ),
            });
        }
        if actual.log_payloads[terminal_index + 1..]
            .iter()
            .any(|event| matches!(event, Event::JobExecutionChanged { .. }))
        {
            findings.push(Finding {
                grader: self.name(),
                detail: "execution event followed terminal state".into(),
            });
        }
        findings
    }
}

pub struct SequenceSnapshotGrader {
    pub expected: Result<Transcript, EvalError>,
}

impl Grader for SequenceSnapshotGrader {
    fn name(&self) -> &'static str {
        "sequence-snapshot"
    }

    fn grade(&self, _scenario: &Scenario, actual: &Transcript) -> Vec<Finding> {
        let expected = match &self.expected {
            Ok(expected) => expected,
            Err(error) => {
                return vec![Finding {
                    grader: self.name(),
                    detail: error.to_string(),
                }];
            }
        };
        if expected == actual {
            return Vec::new();
        }

        vec![Finding {
            grader: self.name(),
            detail: first_difference(expected, actual),
        }]
    }
}

pub struct ArtifactGrader;

impl Grader for ArtifactGrader {
    fn name(&self) -> &'static str {
        "artifact"
    }

    fn grade(&self, scenario: &Scenario, actual: &Transcript) -> Vec<Finding> {
        let expected = scenario
            .script
            .artifacts
            .iter()
            .map(|artifact| artifact.uri_or_path.clone())
            .collect::<Vec<_>>();
        let produced = actual
            .log_payloads
            .iter()
            .filter_map(|event| match event {
                Event::ArtifactProduced { artifact_id } => Some(artifact_id.0.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if expected == produced {
            Vec::new()
        } else {
            vec![Finding {
                grader: self.name(),
                detail: format!("expected artifacts {expected:?}, produced {produced:?}"),
            }]
        }
    }
}

pub struct PermissionGrader;

impl Grader for PermissionGrader {
    fn name(&self) -> &'static str {
        "permission"
    }

    fn grade(&self, scenario: &Scenario, actual: &Transcript) -> Vec<Finding> {
        let actual_outcomes = actual
            .permissions
            .iter()
            .map(|decision| decision.outcome.clone())
            .collect::<Vec<_>>();
        if scenario.permission_outcomes == actual_outcomes {
            Vec::new()
        } else {
            vec![Finding {
                grader: self.name(),
                detail: format!(
                    "expected permission outcomes {:?}, found {:?}",
                    scenario.permission_outcomes, actual_outcomes
                ),
            }]
        }
    }
}

fn is_terminal(status: &JobExecutionStatus) -> bool {
    matches!(
        status,
        JobExecutionStatus::Completed | JobExecutionStatus::Failed | JobExecutionStatus::Cancelled
    )
}

fn first_difference(expected: &Transcript, actual: &Transcript) -> String {
    let expected_value = serde_json::to_value(expected).expect("transcript to json");
    let actual_value = serde_json::to_value(actual).expect("transcript to json");
    first_value_difference("$", &expected_value, &actual_value)
}

fn first_value_difference(path: &str, expected: &Value, actual: &Value) -> String {
    match (expected, actual) {
        (Value::Array(expected), Value::Array(actual)) => {
            for index in 0..expected.len().min(actual.len()) {
                if expected[index] != actual[index] {
                    return first_value_difference(
                        &format!("{path}[{index}]"),
                        &expected[index],
                        &actual[index],
                    );
                }
            }
            format!(
                "first difference at {path}: expected length {}, actual length {}",
                expected.len(),
                actual.len()
            )
        }
        (Value::Object(expected), Value::Object(actual)) => {
            let mut keys = expected
                .keys()
                .chain(actual.keys())
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();
            keys.dedup();
            for key in keys {
                if expected.get(&key) != actual.get(&key) {
                    return first_value_difference(
                        &format!("{path}.{key}"),
                        expected.get(&key).unwrap_or(&Value::Null),
                        actual.get(&key).unwrap_or(&Value::Null),
                    );
                }
            }
            "transcripts differed but no field difference was found".into()
        }
        _ => format!("first difference at {path}: expected {expected}, actual {actual}"),
    }
}

fn temp_log_path(name: &str) -> PathBuf {
    let id = NEXT_TEST_ID.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "mobee-evals-{name}-{}-{id}.jsonl",
        std::process::id()
    ))
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);

    loop {
        match Pin::new(&mut future).poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }

    fn noop_raw_waker() -> RawWaker {
        RawWaker::new(std::ptr::null(), &NOOP_WAKER_VTABLE)
    }

    static NOOP_WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(noop_clone, noop_wake, noop_wake, noop_drop);

    unsafe fn noop_clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }

    unsafe fn noop_wake(_: *const ()) {}

    unsafe fn noop_drop(_: *const ()) {}
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use mobee_core::driver::{
        Artifact, PermissionOutcome, PermissionRequest, ScriptedSession, SessionUpdate, StopReason,
    };
    use mobee_core::event::{Event, JobExecutionStatus, JobId};
    use serde_json::json;

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn terminal_grader_fails_on_wrong_terminal() {
        let scenario = scenario_base();
        let transcript = Transcript {
            log_payloads: vec![Event::JobExecutionChanged {
                job_id: JobId("job-1".into()),
                status: JobExecutionStatus::Failed,
            }],
            updates: Vec::new(),
            permissions: Vec::new(),
        };

        assert_eq!(
            TerminalStateGrader.grade(&scenario, &transcript)[0].grader,
            "terminal-state"
        );
    }

    #[test]
    fn snapshot_grader_fails_on_reordered_update() {
        let scenario = scenario_base();
        let expected = Transcript {
            log_payloads: Vec::new(),
            updates: vec![
                SessionUpdate::Plan {
                    entries: vec!["one".into()],
                },
                SessionUpdate::Plan {
                    entries: vec!["two".into()],
                },
            ],
            permissions: Vec::new(),
        };
        let actual = Transcript {
            updates: expected.updates.iter().cloned().rev().collect(),
            ..expected.clone()
        };
        let grader = SequenceSnapshotGrader {
            expected: Ok(expected),
        };

        assert_eq!(
            grader.grade(&scenario, &actual)[0].grader,
            "sequence-snapshot"
        );
    }

    #[test]
    fn artifact_grader_fails_on_missing_artifact() {
        let mut scenario = scenario_base();
        scenario.script.artifacts = vec![Artifact {
            uri_or_path: "out/result.txt".into(),
            mime: None,
            bytes: None,
        }];
        let transcript = Transcript {
            log_payloads: Vec::new(),
            updates: Vec::new(),
            permissions: Vec::new(),
        };

        assert_eq!(
            ArtifactGrader.grade(&scenario, &transcript)[0].grader,
            "artifact"
        );
    }

    #[test]
    fn permission_grader_fails_on_flipped_outcome() {
        let mut scenario = scenario_base();
        scenario.permission_outcomes = vec![PermissionOutcome::Allow];
        let transcript = Transcript {
            log_payloads: Vec::new(),
            updates: Vec::new(),
            permissions: vec![PermissionDecision {
                request: PermissionRequest {
                    tool: "shell".into(),
                    detail: json!({"cmd": "false"}),
                },
                outcome: PermissionOutcome::Deny,
            }],
        };

        assert_eq!(
            PermissionGrader.grade(&scenario, &transcript)[0].grader,
            "permission"
        );
    }

    #[test]
    fn error_scenario_with_artifacts_lints_before_grading() {
        let mut scenario = scenario_base();
        scenario.expect_run_error = true;
        scenario.script.artifacts = vec![Artifact {
            uri_or_path: "out/should-not-exist.txt".into(),
            mime: None,
            bytes: None,
        }];

        assert_eq!(lint_scenario(&scenario)[0].grader, "scenario-lint");
    }

    #[test]
    fn bless_mode_rewrites_snapshot_copy() {
        let mut scenario = scenario_base();
        scenario.snapshot = "bless.json".into();
        let snapshot_root = temp_dir("bless-snapshots");
        fs::create_dir_all(&snapshot_root).expect("snapshot dir");
        fs::write(
            snapshot_root.join(&scenario.snapshot),
            br#"{"log_payloads":[],"updates":[],"permissions":[]}"#,
        )
        .expect("write stale snapshot");

        run_and_grade(&scenario, &snapshot_root, true).expect("bless");
        let blessed = read_snapshot(&snapshot_root, &scenario).expect("read blessed snapshot");

        assert_eq!(
            blessed.log_payloads.last(),
            Some(&Event::JobExecutionChanged {
                job_id: JobId("job-1".into()),
                status: JobExecutionStatus::Completed
            })
        );
    }

    fn scenario_base() -> Scenario {
        Scenario {
            name: "unit".into(),
            job_id: JobId("job-1".into()),
            script: ScriptedSession {
                session_id: "session-1".into(),
                updates: vec![SessionUpdate::TurnEnded(StopReason::Completed)],
                artifacts: Vec::new(),
            },
            permission_outcomes: Vec::new(),
            expected_terminal: JobExecutionStatus::Completed,
            expect_run_error: false,
            snapshot: "unit.json".into(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-evals-{name}-{}-{id}", std::process::id()))
    }
}
