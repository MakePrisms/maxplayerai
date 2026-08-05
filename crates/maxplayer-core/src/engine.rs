use crate::driver::{
    Artifact, ContentBlock, Driver, DriverError, PermissionOutcome, PermissionRequest, PromptTurn,
    SessionConfig, SessionUpdate, StopReason, UsageMetadata,
};
use crate::event::{ArtifactId, Envelope, Event, JobExecutionStatus, JobId};
use crate::log::{EventLog, LogError};
use std::error::Error;
use std::fmt::{self, Display};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEvent<'a> {
    Update(&'a SessionUpdate),
    PermissionDecided {
        request: &'a PermissionRequest,
        outcome: &'a PermissionOutcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    pub terminal: JobExecutionStatus,
    pub artifacts: Vec<Artifact>,
    /// Usage the driver surfaced for this run (seller-claimed). `None` when the harness exposed
    /// nothing — carried optionally so absent-stays-absent survives the seam.
    pub usage: Option<UsageMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunParams {
    pub session_config: SessionConfig,
    pub prompt: PromptTurn,
}

impl RunParams {
    pub fn mock_defaults() -> Self {
        Self {
            session_config: SessionConfig {
                cwd: std::env::current_dir().unwrap_or_else(|_| ".".into()),
                mcp_servers: Vec::new(),
                env: Vec::new(),
            },
            prompt: PromptTurn {
                input: vec![ContentBlock::Text {
                    text: "do the work".into(),
                }],
            },
        }
    }
}

#[derive(Debug)]
pub enum EngineError {
    Driver(DriverError),
    Log(LogError),
    MissingTerminal,
}

impl Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Driver(error) => write!(f, "{error}"),
            Self::Log(error) => write!(f, "{error}"),
            Self::MissingTerminal => write!(f, "mock update stream ended without turn_ended"),
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Driver(error) => Some(error),
            Self::Log(error) => Some(error),
            Self::MissingTerminal => None,
        }
    }
}

impl From<DriverError> for EngineError {
    fn from(error: DriverError) -> Self {
        Self::Driver(error)
    }
}

impl From<LogError> for EngineError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

pub async fn run_job<D: Driver>(
    driver: &mut D,
    log: &mut EventLog,
    job_id: &JobId,
    params: RunParams,
    sink: &mut dyn FnMut(RunEvent<'_>),
) -> Result<RunOutcome, EngineError> {
    let readiness = driver.ready().await?;
    log.append(Event::DriverReady {
        runtime_id: readiness.runtime_id,
    })?;

    append_execution(log, job_id, JobExecutionStatus::Queued)?;
    append_execution(log, job_id, JobExecutionStatus::Running)?;

    let session_id = driver.start_session(params.session_config).await?;
    let mut stream = match driver.prompt(&session_id, params.prompt).await {
        Ok(stream) => stream,
        Err(error) => {
            append_execution(log, job_id, JobExecutionStatus::Failed)?;
            return Err(error.into());
        }
    };

    let mut terminal = None;
    while let Some(update) = stream.next().await {
        sink(RunEvent::Update(&update));
        if let Some(text) = update_text(&update)
            && !text.trim().is_empty()
        {
            log.append(Event::AgentMessage {
                job_id: job_id.clone(),
                text,
            })?;
        }
        if let SessionUpdate::PermissionRequest(request) = update.clone() {
            let outcome = driver.on_permission(request.clone()).await;
            sink(RunEvent::PermissionDecided {
                request: &request,
                outcome: &outcome,
            });
        }
        if let SessionUpdate::TurnEnded(reason) = update {
            let status = terminal_status(reason);
            append_execution(log, job_id, status.clone())?;
            terminal = Some(status);
            break;
        }
    }

    let Some(terminal) = terminal else {
        append_execution(log, job_id, JobExecutionStatus::Failed)?;
        return Err(EngineError::MissingTerminal);
    };

    let artifacts = driver.artifacts(&session_id).await?;
    for artifact in &artifacts {
        log.append(Event::ArtifactProduced {
            artifact_id: ArtifactId(artifact.uri_or_path.clone()),
        })?;
    }
    // Lift whatever usage the driver captured (absent-stays-absent → None).
    let usage = driver.usage();
    driver.shutdown().await?;
    Ok(RunOutcome {
        terminal,
        artifacts,
        usage,
    })
}

fn append_execution(
    log: &mut EventLog,
    job_id: &JobId,
    status: JobExecutionStatus,
) -> Result<Envelope, LogError> {
    log.append(Event::JobExecutionChanged {
        job_id: job_id.clone(),
        status,
    })
}

fn terminal_status(reason: StopReason) -> JobExecutionStatus {
    match reason {
        StopReason::Completed => JobExecutionStatus::Completed,
        StopReason::Failed => JobExecutionStatus::Failed,
        StopReason::Cancelled => JobExecutionStatus::Cancelled,
    }
}

fn update_text(update: &SessionUpdate) -> Option<String> {
    let text = match update {
        SessionUpdate::AgentMessage(blocks) => blocks
            .iter()
            .filter_map(content_block_text)
            .collect::<Vec<_>>()
            .join("\n"),
        SessionUpdate::AgentMessageChunk(block) => content_block_text(block).unwrap_or_default(),
        _ => String::new(),
    };
    (!text.is_empty()).then_some(text)
}

fn content_block_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text { text } => Some(text.clone()),
        ContentBlock::Artifact(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use crate::driver::{
        Artifact, ContentBlock, MockDriver, ScriptedSession, SessionUpdate, StopReason,
        UsageMetadata,
    };
    use crate::engine::{EngineError, RunEvent, RunOutcome, RunParams, run_job};
    use crate::event::{ArtifactId, Event, JobExecutionStatus, JobId, RuntimeId};
    use crate::log::EventLog;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn run_job_appends_events_to_log() {
        let script = ScriptedSession {
            session_id: "session-1".into(),
            updates: vec![SessionUpdate::TurnEnded(StopReason::Completed)],
            artifacts: vec![Artifact {
                uri_or_path: "out/result.txt".into(),
                mime: Some("text/plain".into()),
                bytes: None,
            }],
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script]);
        let path = test_path("run-job-log");
        let mut log = EventLog::open(&path).expect("open log");

        let outcome = block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |_| {},
        ))
        .expect("run job");

        assert_eq!(
            outcome,
            RunOutcome {
                terminal: JobExecutionStatus::Completed,
                artifacts: vec![Artifact {
                    uri_or_path: "out/result.txt".into(),
                    mime: Some("text/plain".into()),
                    bytes: None,
                }],
                usage: None,
            }
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
    fn run_job_threads_driver_usage_into_outcome() {
        // Usage the driver surfaced must ride out on RunOutcome (the seam the seller reads).
        // A driver that exposes nothing keeps `usage: None` (absent-stays-absent).
        let usage = UsageMetadata {
            model: Some("claude-opus-4-8".into()),
            input_tokens: Some(100),
            output_tokens: Some(40),
            ..UsageMetadata::default()
        };
        let script = ScriptedSession {
            session_id: "session-1".into(),
            updates: vec![SessionUpdate::TurnEnded(StopReason::Completed)],
            artifacts: Vec::new(),
        };
        let mut driver =
            MockDriver::new(RuntimeId("mock".into()), vec![script]).with_usage(usage.clone());
        let mut log = EventLog::open(test_path("usage-threads")).expect("open log");

        let outcome = block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |_| {},
        ))
        .expect("run job");

        assert_eq!(outcome.usage, Some(usage));
    }

    #[test]
    fn stream_without_terminal_appends_failed_and_returns_err() {
        let script = ScriptedSession {
            session_id: "session-1".into(),
            updates: vec![SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                text: "partial".into(),
            }])],
            artifacts: Vec::new(),
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script]);
        let path = test_path("no-terminal-log");
        let mut log = EventLog::open(&path).expect("open log");
        let mut updates = Vec::new();

        let result = block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |event| {
                if let RunEvent::Update(update) = event {
                    updates.push(update.clone());
                }
            },
        ));

        assert!(matches!(result, Err(EngineError::MissingTerminal)));
        assert_eq!(updates.len(), 1);
        assert_eq!(
            replay_payloads(&log).last(),
            Some(&Event::JobExecutionChanged {
                job_id: JobId("job-1".into()),
                status: JobExecutionStatus::Failed
            })
        );
    }

    #[test]
    fn agent_message_chunks_are_logged_for_audit() {
        let script = ScriptedSession {
            session_id: "session-1".into(),
            updates: vec![
                SessionUpdate::AgentMessageChunk(ContentBlock::Text {
                    text: "hello ".into(),
                }),
                SessionUpdate::AgentMessageChunk(ContentBlock::Text {
                    text: "world".into(),
                }),
                SessionUpdate::TurnEnded(StopReason::Completed),
            ],
            artifacts: Vec::new(),
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script]);
        let path = test_path("agent-message-log");
        let mut log = EventLog::open(&path).expect("open log");

        block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |_| {},
        ))
        .expect("run job");

        assert!(replay_payloads(&log).iter().any(|event| {
            matches!(
                event,
                Event::AgentMessage {
                    job_id: JobId(value),
                    text
                } if value == "job-1" && text == "hello "
            )
        }));
        assert!(replay_payloads(&log).iter().any(|event| {
            matches!(
                event,
                Event::AgentMessage {
                    job_id: JobId(value),
                    text
                } if value == "job-1" && text == "world"
            )
        }));
    }

    #[test]
    fn post_terminal_updates_are_dropped() {
        let script = ScriptedSession {
            session_id: "session-1".into(),
            updates: vec![
                SessionUpdate::TurnEnded(StopReason::Completed),
                SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                    text: "too late".into(),
                }]),
            ],
            artifacts: Vec::new(),
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script]);
        let path = test_path("post-terminal-log");
        let mut log = EventLog::open(&path).expect("open log");
        let mut updates = Vec::new();

        block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |event| {
                if let RunEvent::Update(update) = event {
                    updates.push(update.clone());
                }
            },
        ))
        .expect("run job");

        assert_eq!(
            updates,
            vec![SessionUpdate::TurnEnded(StopReason::Completed)]
        );
        assert!(!replay_payloads(&log).iter().any(|event| {
            matches!(
                event,
                Event::ArtifactProduced {
                    artifact_id: ArtifactId(value)
                } if value == "too late"
            )
        }));
    }

    fn replay_payloads(log: &EventLog) -> Vec<Event> {
        let replay = log.replay(0);
        assert_eq!(replay.error, None);
        replay
            .envelopes
            .into_iter()
            .map(|envelope| envelope.payload)
            .collect()
    }

    fn test_path(name: &str) -> std::path::PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-engine-{name}-{}-{id}.jsonl",
            std::process::id()
        ))
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);

        loop {
            match Pin::new(&mut future).poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn noop_waker() -> Waker {
        unsafe { Waker::from_raw(noop_raw_waker()) }
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
