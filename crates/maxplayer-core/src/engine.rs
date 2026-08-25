use crate::driver::{
    Artifact, ContentBlock, Driver, DriverError, PermissionOutcome, PermissionRequest, PromptTurn,
    SessionConfig, SessionId, SessionUpdate, StopReason, UsageMetadata,
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
    /// The model this job NAMED, if it named one, verbatim as the buyer signed it.
    ///
    /// `None` means no model was requested and the run takes whatever the harness defaults to —
    /// today's behaviour for every caller, unchanged. `Some` means the run is only allowed to
    /// proceed on that exact model: [`bind_session_model`] asks the harness to bind it and refuses
    /// the job if the harness reports anything else.
    ///
    /// Carried verbatim on purpose. Normalising it anywhere en route would hide the substitution
    /// the comparison exists to catch — `claude-agent-acp` resolves `opus` onto a canonical id and
    /// reports success, so the requested string and the bound string must stay separately
    /// observable all the way to the comparison.
    pub requested_model: Option<String>,
}

impl RunParams {
    pub fn mock_defaults() -> Self {
        Self {
            session_config: SessionConfig {
                cwd: std::env::current_dir().unwrap_or_else(|_| ".".into()),
                mcp_servers: Vec::new(),
                env: Vec::new(),
            },
            // No model requested: the default path, and the one every caller takes today.
            requested_model: None,
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
    /// The job named a model and the harness bound something else. The run is refused.
    ///
    /// Carries BOTH strings because the pair is the evidence: a report saying only "model mismatch"
    /// cannot distinguish an alias the harness canonicalised from a value it ignored outright, and
    /// those want different follow-ups. `bound: None` means the harness reported no usable model at
    /// all after the write.
    ModelBindMismatch {
        requested: String,
        bound: Option<String>,
    },
}

impl Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Driver(error) => write!(f, "{error}"),
            Self::Log(error) => write!(f, "{error}"),
            Self::MissingTerminal => write!(f, "mock update stream ended without turn_ended"),
            Self::ModelBindMismatch { requested, bound } => match bound {
                Some(bound) => write!(
                    f,
                    "job requested model {requested} but the harness bound {bound}"
                ),
                None => write!(
                    f,
                    "job requested model {requested} but the harness reported no bound model"
                ),
            },
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Driver(error) => Some(error),
            Self::Log(error) => Some(error),
            // No source: the mismatch IS the fault, not a wrapper around a lower-level one. The
            // harness succeeded at everything it was asked; the refusal is ours.
            Self::MissingTerminal | Self::ModelBindMismatch { .. } => None,
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

/// Drive one job to its terminal state, then shut the driver down.
///
/// #729: `shutdown()` runs on EVERY exit, not only the success epilogue. The old shape placed it
/// after the last `?`, so exactly the runs that failed — the ones that repeat — returned early and
/// left the ACP child alive and unreaped (measured on a live seat: the child still healthy 80
/// minutes past `execute fail`, then a zombie after a manual kill — kill without `wait`).
///
/// The can't-forget form chosen here is a single-exit wrapper, not a `Drop` guard, because a
/// guard cannot exist for this trait: `Driver::shutdown` is async (the ACP driver reaps
/// off-runtime via `spawn_blocking` and must be awaited) and `Drop` cannot await — a blocking
/// Drop on the seller node's single-threaded LocalSet would stall every sibling job, the exact
/// hazard #223 removed. The wrapper keeps the guard's property: ALL fallible work lives in
/// [`run_turn`], so any `?` added to the turn tomorrow still funnels through the one shutdown
/// below. (No caller drops this future mid-await — the job deadline is enforced inside the
/// driver's response wait and surfaces as an `Err` through this same seam.)
pub async fn run_job<D: Driver>(
    driver: &mut D,
    log: &mut EventLog,
    job_id: &JobId,
    params: RunParams,
    sink: &mut dyn FnMut(RunEvent<'_>),
) -> Result<RunOutcome, EngineError> {
    let outcome = run_turn(driver, log, job_id, params, sink).await;
    let shutdown = driver.shutdown().await;
    match outcome {
        Ok(run) => {
            shutdown?;
            Ok(run)
        }
        // The run error is what the seller reports and routes feedback on; a secondary shutdown
        // failure must not mask it.
        Err(error) => Err(error),
    }
}

/// Bind `requested` as the session's model and PROVE the harness took it, or refuse the run.
///
/// ⛔ The setter's success is not the check. Measured by reading both adapters' sources: `codex-acp`
/// accepts an unrecognised model verbatim and forwards it to Codex, rejecting only the empty string;
/// `claude-agent-acp` fuzzy-resolves aliases like `opus` onto a canonical id, then deliberately
/// substitutes "the canonical option value so downstream code always receives the model ID rather
/// than the caller-supplied alias". Both return OK having bound something other than what was named,
/// so a caller that trusted the return would run a job on a model the buyer did not ask for.
///
/// The comparison is EXACT and lives here rather than in the driver — one place, so a driver cannot
/// bypass it, and testable without a harness. No aliasing forgiveness on our side: this crate's own
/// rule is that a named request is exact or nothing, with no nearest-match fallback, because
/// silently running a job on something the buyer did not ask for is the failure the registry exists
/// to prevent. A harness that canonicalises `opus` to `claude-opus-4-6` has bound a DIFFERENT STRING
/// than the one signed, and the buyer filtered and paid on the string.
///
/// Returns the bound model on success, for callers that want to log what was proven.
async fn bind_session_model<D: Driver>(
    driver: &mut D,
    session_id: &SessionId,
    requested: &str,
) -> Result<String, EngineError> {
    verify_bound_model(requested, driver.select_model(session_id, requested).await?)
}

/// The comparison itself: EXACT, or refuse.
///
/// Split out from [`bind_session_model`] so the policy is a pure function — no driver, no runtime,
/// no session. The whole value of this change is which pairs it accepts, and that is worth testing
/// directly rather than through a stub whose own behaviour would need trusting.
fn verify_bound_model(requested: &str, bound: Option<String>) -> Result<String, EngineError> {
    match bound {
        Some(bound) if bound == requested => Ok(bound),
        bound => Err(EngineError::ModelBindMismatch {
            requested: requested.to_owned(),
            bound,
        }),
    }
}

/// The fallible body of [`run_job`]: readiness through usage capture, shutdown excluded.
async fn run_turn<D: Driver>(
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
    // A named model is bound and PROVEN bound before any work happens. Refusing here costs nothing:
    // the session exists but no prompt has been sent, so nothing has been spent on compute and the
    // job fails without a delivery. Ordering matters — this sits before `prompt`, never after.
    if let Some(requested) = params.requested_model.as_deref() {
        bind_session_model(driver, &session_id, requested).await?;
    }
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

pub(crate) fn update_text(update: &SessionUpdate) -> Option<String> {
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

/// The agent's own account of a turn, accumulated from the sink [`run_job`] already calls for every
/// update.
///
/// A turn can complete having done nothing and say WHY in its last message — a blocked host, an
/// exhausted plan, a refusal. A caller that discards the stream keeps only the turn's SHAPE, and a
/// completed-but-empty turn has the same shape whatever the cause, so the cause must be guessed.
///
/// Named rather than written inline at the call site so the retention rule can be tested on its own.
/// An inline closure is reachable only through the driver it is installed beside, which is why the
/// rule went untested when it was one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AgentMessageCapture {
    last: Option<String>,
}

impl AgentMessageCapture {
    /// Retains `event`'s text when it carries any, so the LAST message the agent sent wins.
    ///
    /// Whitespace-only text is not an account of anything and must not displace a real message;
    /// agents routinely close a turn with a bare newline chunk.
    pub(crate) fn observe(&mut self, event: RunEvent<'_>) {
        if let RunEvent::Update(update) = event
            && let Some(text) = update_text(update)
            && !text.trim().is_empty()
        {
            self.last = Some(text);
        }
    }

    /// The retained message, or `None` when the agent sent no text at all.
    ///
    /// `None` is a positive claim — the agent said nothing — and a caller must not render it as an
    /// unknown, because a capture that silently failed to fill would be indistinguishable from it.
    pub(crate) fn into_last_message(self) -> Option<String> {
        self.last
    }
}

#[cfg(test)]
mod model_binding_tests {
    use super::{EngineError, verify_bound_model};

    #[test]
    fn an_exactly_matching_bound_model_is_accepted() {
        // The only accepting case, and the control for every refusal below: if this were not Ok the
        // rest of the module would pass for the wrong reason.
        assert_eq!(
            verify_bound_model("claude-opus-4-8", Some("claude-opus-4-8".into())).unwrap(),
            "claude-opus-4-8"
        );
    }

    #[test]
    fn a_fuzzily_resolved_alias_is_refused() {
        // THE case #785 names. `claude-agent-acp` resolves `opus` onto a canonical id and returns
        // SUCCESS, deliberately substituting "the canonical option value so downstream code always
        // receives the model ID rather than the caller-supplied alias". The set succeeded; a
        // different model is bound. The buyer named `opus`, filtered on `opus` and paid on `opus`,
        // so a run on `claude-opus-4-6` is a different product and this must refuse.
        let error = verify_bound_model("opus", Some("claude-opus-4-6".into())).unwrap_err();
        assert!(
            matches!(
                &error,
                EngineError::ModelBindMismatch { requested, bound }
                    if requested == "opus" && bound.as_deref() == Some("claude-opus-4-6")
            ),
            "both strings must survive into the error, or a reader cannot tell a canonicalised \
             alias from an ignored value: {error}"
        );
    }

    #[test]
    fn a_harness_reporting_no_model_is_refused() {
        // Absence is not agreement. A harness that wrote something and then reported nothing usable
        // has not shown us the requested model is bound, so the run does not proceed.
        let error = verify_bound_model("claude-opus-4-8", None).unwrap_err();
        assert!(matches!(
            error,
            EngineError::ModelBindMismatch { bound: None, .. }
        ));
    }

    #[test]
    fn the_comparison_is_exact_and_forgives_nothing() {
        // No trimming, no case-folding, no prefix or suffix tolerance. Each of these is a real
        // near-miss shape and every one of them is a DIFFERENT model id than the one requested.
        // Forgiving any of them re-introduces nearest-match dispatch through the back door.
        for bound in [
            "claude-opus-4-8 ",       // trailing space
            " claude-opus-4-8",       // leading space
            "Claude-Opus-4-8",        // case
            "claude-opus-4-8[medium]", // composed: the effort axis appended
            "claude-opus-4",          // prefix
            "claude-opus-4-80",       // the requested id is a prefix of this one
        ] {
            assert!(
                verify_bound_model("claude-opus-4-8", Some(bound.into())).is_err(),
                "bound {bound:?} differs from the request and must be refused"
            );
        }
    }

    #[test]
    fn an_echoed_unknown_model_is_not_caught_here_and_that_is_deliberate() {
        // ⛔ THE LIMIT OF THIS CHECK, pinned so nobody later reads it as total coverage.
        // `codex-acp` accepts an unrecognised id VERBATIM and forwards it, so the read-back echoes
        // the nonsense and request == bound. This comparison therefore ACCEPTS it — correctly, on
        // its own terms, because the harness did report exactly what was asked for.
        //
        // The guard for that direction is membership, checked pre-write in the driver
        // (`DriverError::ModelNotOffered`). Two different failures need two different guards, and
        // this test exists so a future reader cannot mistake the exact comparison for both.
        assert_eq!(
            verify_bound_model("gpt-9-does-not-exist", Some("gpt-9-does-not-exist".into())).unwrap(),
            "gpt-9-does-not-exist"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use crate::driver::{
        Artifact, ContentBlock, DriverError, MockDriver, ScriptedSession, SessionUpdate,
        StopReason, UsageMetadata,
    };
    use crate::engine::{
        AgentMessageCapture, EngineError, RunEvent, RunOutcome, RunParams, run_job,
    };
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
        assert_eq!(driver.shutdown_calls(), 1);
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
        // #729 regression: cleanup used to sit after the last `?`, so this exact early return
        // skipped `shutdown()` and left the ACP child alive and unreaped.
        assert_eq!(driver.shutdown_calls(), 1);
    }

    #[test]
    fn shutdown_runs_when_an_early_question_mark_fails_the_run() {
        // No scripts: `start_session` is the first `?` after readiness — the earliest failure
        // exit. #729: the driver must be shut down through it all the same.
        let mut driver = MockDriver::new(RuntimeId("mock".into()), Vec::new());
        let mut log = EventLog::open(test_path("early-exit-shutdown")).expect("open log");

        let result = block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |_| {},
        ));

        assert!(matches!(
            result,
            Err(EngineError::Driver(DriverError::ScriptExhausted))
        ));
        assert_eq!(driver.shutdown_calls(), 1);
    }

    #[test]
    fn a_run_error_is_not_masked_by_a_failing_shutdown() {
        // Both the run AND the shutdown fail: the run error is what the seller reports and routes
        // feedback on, so it must win. Shutdown still ran (the child was still reaped).
        let script = ScriptedSession {
            session_id: "session-1".into(),
            updates: Vec::new(), // stream ends without turn_ended → MissingTerminal
            artifacts: Vec::new(),
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script])
            .with_shutdown_error(DriverError::Other("shutdown broke".into()));
        let mut log = EventLog::open(test_path("run-error-wins")).expect("open log");

        let result = block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |_| {},
        ));

        assert!(matches!(result, Err(EngineError::MissingTerminal)));
        assert_eq!(driver.shutdown_calls(), 1);
    }

    #[test]
    fn a_clean_run_still_surfaces_a_shutdown_error() {
        // Unchanged behavior from before #729: when the run itself succeeded, a shutdown failure
        // is the only fault and must not be swallowed.
        let script = ScriptedSession {
            session_id: "session-1".into(),
            updates: vec![SessionUpdate::TurnEnded(StopReason::Completed)],
            artifacts: Vec::new(),
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script])
            .with_shutdown_error(DriverError::Other("shutdown broke".into()));
        let mut log = EventLog::open(test_path("shutdown-error-surfaces")).expect("open log");

        let result = block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |_| {},
        ));

        assert!(matches!(
            result,
            Err(EngineError::Driver(DriverError::Other(message))) if message == "shutdown broke"
        ));
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

    /// The fixture is the message that cost a day: cursor folded a DNS failure for its model host
    /// into ordinary assistant text and ended the turn normally, so the turn's shape said "flaky
    /// model" while its text said "blocked egress".
    const BLOCKED_HOST_MESSAGE: &str =
        "Error: RetriableError: [unavailable] getaddrinfo EAI_AGAIN agentn.global.api5.cursor.sh";

    #[test]
    fn the_capture_keeps_the_agents_last_non_empty_message() {
        let early = SessionUpdate::AgentMessage(vec![ContentBlock::Text {
            text: "looking at the repo".into(),
        }]);
        let blank = SessionUpdate::AgentMessageChunk(ContentBlock::Text {
            text: "  \n".into(),
        });
        let last = SessionUpdate::AgentMessage(vec![ContentBlock::Text {
            text: BLOCKED_HOST_MESSAGE.into(),
        }]);
        let ended = SessionUpdate::TurnEnded(StopReason::Completed);
        let mut capture = AgentMessageCapture::default();

        for update in [&early, &blank, &last, &ended] {
            capture.observe(RunEvent::Update(update));
        }

        assert_eq!(
            capture.into_last_message().as_deref(),
            Some(BLOCKED_HOST_MESSAGE),
            "the last non-empty message must win: neither the trailing blank chunk nor the terminal \
             update carries an account of the turn, so neither may displace one"
        );
    }

    #[test]
    fn a_turn_that_said_nothing_captures_none() {
        let blank = SessionUpdate::AgentMessageChunk(ContentBlock::Text { text: "   ".into() });
        let ended = SessionUpdate::TurnEnded(StopReason::Completed);
        let mut capture = AgentMessageCapture::default();

        capture.observe(RunEvent::Update(&blank));
        capture.observe(RunEvent::Update(&ended));

        assert_eq!(
            capture.into_last_message(),
            None,
            "`None` must mean the agent said nothing, which is only true if a real message would \
             have been kept — the assertion above is what earns that reading"
        );
    }

    #[test]
    fn run_job_feeds_the_capture_the_agents_last_message() {
        let script = ScriptedSession {
            session_id: "session-1".into(),
            updates: vec![
                SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                    text: "looking at the repo".into(),
                }]),
                SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                    text: BLOCKED_HOST_MESSAGE.into(),
                }]),
                SessionUpdate::TurnEnded(StopReason::Completed),
            ],
            artifacts: Vec::new(),
        };
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![script]);
        let path = test_path("capture-through-run-job");
        let mut log = EventLog::open(&path).expect("open log");
        let mut capture = AgentMessageCapture::default();

        let outcome = block_on(run_job(
            &mut driver,
            &mut log,
            &JobId("job-1".into()),
            RunParams::mock_defaults(),
            &mut |event| capture.observe(event),
        ))
        .expect("run job");

        assert_eq!(outcome.terminal, JobExecutionStatus::Completed);
        assert!(
            outcome.artifacts.is_empty(),
            "the fixture is the completed-but-empty turn, so the artifact list must stay empty"
        );
        assert_eq!(
            capture.into_last_message().as_deref(),
            Some(BLOCKED_HOST_MESSAGE),
            "a capture that stays empty across a talking turn renders as \"the agent said nothing\" \
             — byte-identical to a genuinely silent turn, and the reason this seam went a day \
             reporting the wrong cause"
        );
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
            "maxplayer-engine-{name}-{}-{id}.jsonl",
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
