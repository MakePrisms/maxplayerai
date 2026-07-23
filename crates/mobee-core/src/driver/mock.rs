use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::driver::acp::{PROTOCOL_VERSION, UpdateStream};
use crate::driver::{
    Artifact, Driver, DriverError, PermissionOutcome, PermissionRequest, PromptTurn, Readiness,
    RuntimeId, SessionConfig, SessionId, SessionUpdate, UsageMetadata,
};

#[derive(Clone, Debug, serde::Deserialize, PartialEq, Eq, serde::Serialize)]
pub struct ScriptedSession {
    pub session_id: SessionId,
    pub updates: Vec<SessionUpdate>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug)]
struct SessionState {
    session_id: SessionId,
    updates: Vec<SessionUpdate>,
    artifacts: Vec<Artifact>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub struct MockDriver {
    runtime_id: RuntimeId,
    ready: bool,
    scripts: VecDeque<ScriptedSession>,
    sessions: Vec<SessionState>,
    permission_outcomes: VecDeque<PermissionOutcome>,
    permission_history: Vec<(PermissionRequest, PermissionOutcome)>,
    prompt_history: Vec<(SessionId, PromptTurn)>,
    usage: Option<UsageMetadata>,
}

impl MockDriver {
    pub fn new(runtime_id: RuntimeId, scripts: Vec<ScriptedSession>) -> Self {
        Self {
            runtime_id,
            ready: false,
            scripts: scripts.into(),
            sessions: Vec::new(),
            permission_outcomes: VecDeque::new(),
            permission_history: Vec::new(),
            prompt_history: Vec::new(),
            usage: None,
        }
    }

    pub fn with_permission_outcomes(mut self, outcomes: Vec<PermissionOutcome>) -> Self {
        self.permission_outcomes = outcomes.into();
        self
    }

    /// Script the usage the driver reports so the engine→`RunOutcome` plumbing is exercisable
    /// without a live ACP rig.
    pub fn with_usage(mut self, usage: UsageMetadata) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn permission_history(&self) -> &[(PermissionRequest, PermissionOutcome)] {
        &self.permission_history
    }

    pub fn prompt_history(&self) -> &[(SessionId, PromptTurn)] {
        &self.prompt_history
    }

    fn session(&self, session_id: &SessionId) -> Result<&SessionState, DriverError> {
        self.sessions
            .iter()
            .find(|session| &session.session_id == session_id)
            .ok_or_else(|| DriverError::SessionNotFound(session_id.clone()))
    }
}

impl Driver for MockDriver {
    fn id(&self) -> RuntimeId {
        self.runtime_id.clone()
    }

    async fn ready(&mut self) -> Result<Readiness, DriverError> {
        self.ready = true;
        Ok(Readiness {
            runtime_id: self.runtime_id.clone(),
            protocol_version: PROTOCOL_VERSION,
        })
    }

    async fn start_session(&mut self, _cfg: SessionConfig) -> Result<SessionId, DriverError> {
        if !self.ready {
            return Err(DriverError::NotReady);
        }

        let script = self
            .scripts
            .pop_front()
            .ok_or(DriverError::ScriptExhausted)?;
        let session_id = script.session_id.clone();
        self.sessions.push(SessionState {
            session_id: session_id.clone(),
            updates: script.updates,
            artifacts: script.artifacts,
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        Ok(session_id)
    }

    async fn prompt(
        &mut self,
        session_id: &SessionId,
        turn: PromptTurn,
    ) -> Result<UpdateStream, DriverError> {
        let session = self.session(session_id)?;
        if session.cancelled.load(Ordering::SeqCst) {
            return Err(DriverError::SessionCancelled(session_id.clone()));
        }

        let updates = session.updates.clone();
        let cancelled = session.cancelled.clone();
        self.prompt_history.push((session_id.clone(), turn));
        Ok(UpdateStream::new(updates, cancelled))
    }

    async fn on_permission(&mut self, req: PermissionRequest) -> PermissionOutcome {
        let outcome = self
            .permission_outcomes
            .pop_front()
            .unwrap_or(PermissionOutcome::Deny);
        self.permission_history.push((req, outcome.clone()));
        outcome
    }

    async fn artifacts(&self, session_id: &SessionId) -> Result<Vec<Artifact>, DriverError> {
        Ok(self.session(session_id)?.artifacts.clone())
    }

    async fn cancel(&mut self, session_id: &SessionId) -> Result<(), DriverError> {
        self.session(session_id)?
            .cancelled
            .store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), DriverError> {
        for session in &self.sessions {
            session.cancelled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn usage(&self) -> Option<UsageMetadata> {
        self.usage.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    use serde_json::json;

    use crate::driver::{
        Artifact, ContentBlock, Driver, ExtMethod, MockDriver, PermissionOutcome,
        PermissionRequest, PromptTurn, ScriptedSession, SessionConfig, SessionUpdate, StopReason,
    };
    use crate::event::{ArtifactId, Event, JobExecutionStatus, JobId, RuntimeId};
    use crate::log::EventLog;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn scripted_turn_yields_updates_in_order_and_turn_ended() {
        let updates = vec![
            SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                text: "working".into(),
            }]),
            SessionUpdate::Plan {
                entries: vec!["finish".into()],
            },
            SessionUpdate::TurnEnded(StopReason::Completed),
        ];
        let mut driver = driver_with_script(updates.clone());

        let session_id = block_on(driver.start_session(session_config())).expect("start session");
        let mut stream = block_on(driver.prompt(&session_id, prompt_turn())).expect("prompt");

        assert_eq!(block_on(stream.next()), Some(updates[0].clone()));
        assert_eq!(block_on(stream.next()), Some(updates[1].clone()));
        assert_eq!(block_on(stream.next()), Some(updates[2].clone()));
        assert_eq!(block_on(stream.next()), None);
    }

    #[test]
    fn permission_requests_route_to_allow_and_deny_outcomes() {
        let mut driver = driver_with_script(vec![SessionUpdate::TurnEnded(StopReason::Completed)])
            .with_permission_outcomes(vec![PermissionOutcome::Allow, PermissionOutcome::Deny]);
        let allow_req = PermissionRequest {
            tool: "write".into(),
            detail: json!({"path": "one"}),
        };
        let deny_req = PermissionRequest {
            tool: "shell".into(),
            detail: json!({"cmd": "rm"}),
        };

        assert_eq!(
            block_on(driver.on_permission(allow_req.clone())),
            PermissionOutcome::Allow
        );
        assert_eq!(
            block_on(driver.on_permission(deny_req.clone())),
            PermissionOutcome::Deny
        );
        assert_eq!(
            driver.permission_history(),
            &[
                (allow_req, PermissionOutcome::Allow),
                (deny_req, PermissionOutcome::Deny)
            ]
        );
    }

    #[test]
    fn ext_method_is_surfaced_unmodified() {
        let ext = ExtMethod {
            method: "cursor/ask_question".into(),
            params: json!({"question": "Proceed?"}),
        };
        let mut driver = driver_with_script(vec![
            SessionUpdate::Ext(ext.clone()),
            SessionUpdate::TurnEnded(StopReason::Completed),
        ]);

        let session_id = block_on(driver.start_session(session_config())).expect("start session");
        let mut stream = block_on(driver.prompt(&session_id, prompt_turn())).expect("prompt");

        assert_eq!(block_on(stream.next()), Some(SessionUpdate::Ext(ext)));
    }

    #[test]
    fn cancel_mid_script_terminates_with_cancelled_without_post_cancel_updates() {
        let mut driver = driver_with_script(vec![
            SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                text: "first".into(),
            }]),
            SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                text: "should not surface".into(),
            }]),
            SessionUpdate::TurnEnded(StopReason::Completed),
        ]);

        let session_id = block_on(driver.start_session(session_config())).expect("start session");
        let mut stream = block_on(driver.prompt(&session_id, prompt_turn())).expect("prompt");
        assert!(matches!(
            block_on(stream.next()),
            Some(SessionUpdate::AgentMessage(_))
        ));

        block_on(driver.cancel(&session_id)).expect("cancel");

        assert_eq!(
            block_on(stream.next()),
            Some(SessionUpdate::TurnEnded(StopReason::Cancelled))
        );
        assert_eq!(block_on(stream.next()), None);
    }

    #[test]
    fn scripted_driver_run_appends_events_to_log() {
        let artifact = Artifact {
            uri_or_path: "out/result.txt".into(),
            mime: Some("text/plain".into()),
            bytes: None,
        };
        let mut driver = driver_with_script(vec![SessionUpdate::TurnEnded(StopReason::Completed)]);
        let mut log = EventLog::open(test_log_path("driver-seam")).expect("open log");
        let job_id = JobId("job-1".into());
        let artifact_id = ArtifactId(artifact.uri_or_path.clone());

        let readiness = block_on(driver.ready()).expect("ready");
        let session_id = block_on(driver.start_session(session_config())).expect("start session");
        let mut stream = block_on(driver.prompt(&session_id, prompt_turn())).expect("prompt");
        log.append(Event::DriverReady {
            runtime_id: readiness.runtime_id,
        })
        .expect("append driver ready");
        log.append(Event::JobExecutionChanged {
            job_id: job_id.clone(),
            status: JobExecutionStatus::Queued,
        })
        .expect("append queued");
        log.append(Event::JobExecutionChanged {
            job_id: job_id.clone(),
            status: JobExecutionStatus::Running,
        })
        .expect("append running");
        assert_eq!(
            block_on(stream.next()),
            Some(SessionUpdate::TurnEnded(StopReason::Completed))
        );
        log.append(Event::JobExecutionChanged {
            job_id,
            status: JobExecutionStatus::Completed,
        })
        .expect("append completed");
        log.append(Event::ArtifactProduced { artifact_id })
            .expect("append artifact");

        let replay = log.replay(0);
        assert_eq!(replay.error, None);
        assert_eq!(
            replay
                .envelopes
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

    fn driver_with_script(updates: Vec<SessionUpdate>) -> MockDriver {
        let mut driver = MockDriver::new(
            RuntimeId("mock".into()),
            vec![ScriptedSession {
                session_id: "session-1".into(),
                updates,
                artifacts: Vec::new(),
            }],
        );
        block_on(driver.ready()).expect("ready");
        driver
    }

    fn session_config() -> SessionConfig {
        SessionConfig {
            cwd: std::env::temp_dir(),
            mcp_servers: Vec::new(),
            env: Vec::new(),
        }
    }

    fn prompt_turn() -> PromptTurn {
        PromptTurn {
            input: vec![ContentBlock::Text {
                text: "do the work".into(),
            }],
        }
    }

    fn test_log_path(name: &str) -> std::path::PathBuf {
        let test_id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mobee-driver-{name}-{}-{test_id}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match Future::poll(Pin::as_mut(&mut future), &mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn noop_waker() -> Waker {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        unsafe fn wake(_: *const ()) {}
        unsafe fn wake_by_ref(_: *const ()) {}
        unsafe fn drop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }
}
