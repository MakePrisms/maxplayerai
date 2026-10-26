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

/// How a session turn opens its conversation and when it must wrap up (docs/specs/session-jobs.md
/// §4, §6.1, §6.3).
pub struct SessionTurnParams {
    pub session_config: SessionConfig,
    /// The ACP session id recorded at the end of the previous turn; `None` on turn 1.
    pub resume: Option<crate::driver::SessionId>,
    /// The wall-clock instant of the wrap-up `session/cancel` (§4); `None` = never. Slot wait,
    /// review and `session/load` all count against it.
    pub wrap_up_at: Option<std::time::SystemTime>,
}

/// What a session turn adds to a [`RunOutcome`].
#[derive(Debug)]
pub struct SessionTurnOutcome {
    pub run: RunOutcome,
    /// The session the turn ran in: recorded for the next turn's `session/load`.
    pub session_id: crate::driver::SessionId,
    /// The previous conversation could not be reloaded and a new one was started (§6.3).
    pub context_reset: bool,
    /// The wrap-up cancel fired before the turn ended on its own (§4, `out_of_time`).
    pub cancelled_for_wrap_up: bool,
}

/// The released prompt, or `None` when the host withheld it (review refused, §6.1 step 3). Called
/// AFTER the conversation is open, with whether it was reset, so the host-supplied history can be
/// chosen — the buyer's text exists in this process only once this returns.
pub type PromptRelease<'a> = dyn FnOnce(bool) -> Result<Option<PromptTurn>, EngineError> + 'a;

/// Why [`run_session_job`] ended without prompting.
#[derive(Debug)]
pub enum SessionTurnEnd {
    Ran(Box<SessionTurnOutcome>),
    /// The host withheld the prompt; the agent was never prompted.
    Withheld,
}

/// [`run_job`] for one turn of a session job: open the conversation by `session/load` when the
/// agent supports it and a previous session id is known, else `session/new` with `context_reset`;
/// only then ask `release` for the prompt; cancel at the wrap-up instant; shut down on every exit.
pub async fn run_session_job<D: Driver>(
    driver: &mut D,
    log: &mut EventLog,
    job_id: &JobId,
    params: SessionTurnParams,
    release: Box<PromptRelease<'_>>,
    sink: &mut dyn FnMut(RunEvent<'_>),
) -> Result<SessionTurnEnd, EngineError> {
    let outcome = run_session_turn(driver, log, job_id, params, release, sink).await;
    let shutdown = driver.shutdown().await;
    match outcome {
        Ok(run) => {
            shutdown?;
            Ok(run)
        }
        Err(error) => Err(error),
    }
}

async fn run_session_turn<D: Driver>(
    driver: &mut D,
    log: &mut EventLog,
    job_id: &JobId,
    params: SessionTurnParams,
    release: Box<PromptRelease<'_>>,
    sink: &mut dyn FnMut(RunEvent<'_>),
) -> Result<SessionTurnEnd, EngineError> {
    let readiness = driver.ready().await?;
    log.append(Event::DriverReady {
        runtime_id: readiness.runtime_id,
    })?;
    append_execution(log, job_id, JobExecutionStatus::Queued)?;

    // §6.3: reload only when the agent advertised it and an id was recorded; any failure falls back
    // to a new conversation in the same container (files and tools stay) and says so.
    let (session_id, context_reset) = match params.resume.as_ref() {
        Some(id) if driver.supports_load_session() => {
            match driver.load_session(params.session_config.clone(), id).await {
                Ok(loaded) => (loaded, false),
                Err(_) => (driver.start_session(params.session_config).await?, true),
            }
        }
        Some(_) => (driver.start_session(params.session_config).await?, true),
        None => (driver.start_session(params.session_config).await?, false),
    };

    // §6.1 step 3: the buyer's text arrives only now, and only if the review passed.
    let Some(prompt) = release(context_reset)? else {
        append_execution(log, job_id, JobExecutionStatus::Cancelled)?;
        return Ok(SessionTurnEnd::Withheld);
    };
    append_execution(log, job_id, JobExecutionStatus::Running)?;

    // §4 wrap-up. `prompt` holds the driver until the turn answers, so the cancel fires from a
    // timer thread through a trigger cloned beforehand; the update loop also checks the instant
    // for drivers whose prompt returns at once. Fired at most once.
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let trigger = driver.cancel_trigger(&session_id);
    let wrap_up_after = params.wrap_up_at.map(|at| {
        at.duration_since(std::time::SystemTime::now())
            .unwrap_or_default()
    });
    let wrap_at = wrap_up_after.map(|after| std::time::Instant::now() + after);
    let fire = {
        let fired = std::sync::Arc::clone(&fired);
        let trigger = trigger.clone();
        move || {
            if !fired.swap(true, std::sync::atomic::Ordering::SeqCst)
                && let Some(trigger) = &trigger
            {
                trigger();
            }
        }
    };
    let (stop_timer, timer_rx) = std::sync::mpsc::channel::<()>();
    let timer = match (wrap_up_after, trigger.is_some()) {
        (Some(after), true) => {
            let fire = fire.clone();
            Some(std::thread::spawn(move || {
                if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    timer_rx.recv_timeout(after)
                {
                    fire();
                }
            }))
        }
        _ => None,
    };
    let stop = |timer: Option<std::thread::JoinHandle<()>>| {
        let _ = stop_timer.send(());
        if let Some(timer) = timer {
            let _ = timer.join();
        }
    };

    let prompted = driver.prompt(&session_id, prompt).await;
    let mut terminal = None;
    let mut stream = match prompted {
        Ok(stream) => Some(stream),
        // The wrap-up cancel won the race against the prompt (a turn started with almost no time
        // left): that is a cancelled turn, `out_of_time`, not a failed one.
        Err(_) if fired.load(std::sync::atomic::Ordering::SeqCst) => {
            append_execution(log, job_id, JobExecutionStatus::Cancelled)?;
            terminal = Some(JobExecutionStatus::Cancelled);
            None
        }
        Err(error) => {
            stop(timer);
            append_execution(log, job_id, JobExecutionStatus::Failed)?;
            return Err(error.into());
        }
    };
    while let Some(stream) = stream.as_mut() {
        if wrap_at.is_some_and(|at| std::time::Instant::now() >= at)
            && !fired.load(std::sync::atomic::Ordering::SeqCst)
        {
            fire();
            let _ = driver.cancel(&session_id).await;
        }
        let Some(update) = stream.next().await else {
            break;
        };
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
    stop(timer);
    let Some(terminal) = terminal else {
        append_execution(log, job_id, JobExecutionStatus::Failed)?;
        return Err(EngineError::MissingTerminal);
    };
    let cancelled_for_wrap_up = fired.load(std::sync::atomic::Ordering::SeqCst);
    let artifacts = driver.artifacts(&session_id).await?;
    let usage = driver.usage();
    Ok(SessionTurnEnd::Ran(Box::new(SessionTurnOutcome {
        run: RunOutcome {
            terminal,
            artifacts,
            usage,
        },
        session_id,
        context_reset,
        cancelled_for_wrap_up,
    })))
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

/// The line an agent MUST put first in its final message to deliver an ANSWER inline.
///
/// An answer job has no tree, so it has no execution sentinel (§8.2) and nothing else in the turn
/// separates "here is your answer" from "my plan is exhausted". Both are a completed turn, an empty
/// workdir, and some text. The marker is what the agent OPTS IN with, and it is the whole gate: a
/// reply without it is never delivered and never paid.
///
/// It is deliberately an opt-in for success, not a status code with a failure value. A failure
/// value would be a refusal instruction in the prompt, which #685 withholds on purpose — a
/// self-declared refusal that the money seam does not yet handle is a refusal that gets PAID. Under
/// an opt-in, every UNMARKED outcome (a vendor error string, a turn that said nothing, a decline
/// the model did not mark) takes the existing refusal path unchanged.
///
/// ⚠️ WHAT IT DOES NOT DO. A model that marks an answer and then writes "I could not determine
/// this" is paid, and no gate here can tell that from a real answer without judging the content.
/// The marker proves that this job's prompt reached a model which answered it — the same thing
/// §8.2 proves for a tree, and no more. Paying for a well-formed but worthless deliverable is
/// #511, and git delivery has it equally: a committed REFUSAL.md mints a sentinel and settles.
pub const INLINE_ANSWER_MARKER: &str = "MAXPLAYER-ANSWER-V1";

/// The answer an agent marked for inline delivery, or `None` when it marked none.
///
/// The marker must be the FIRST whitespace-delimited token of the FIRST non-blank line. First,
/// because a marker the agent could bury anywhere is one a quoted task description or a pasted log
/// can forge by accident. The answer is the rest of that line plus every following line, trimmed:
/// codex, opencode and muse agents routinely write `MAXPLAYER-ANSWER-V1 WORKER1 47` on one line,
/// and refusing that refused a correct, prompted answer, left it unpaid, and struck a healthy
/// harness. What keeps forgery out is the position and the exact token, not the line break: a
/// longer token (`MAXPLAYER-ANSWER-V1-DRAFT`) or a marker anywhere but first is still refused.
///
/// Markdown decoration around the token is accepted — `` `MARKER` ``, `**MARKER**`, `# MARKER`, a
/// trailing colon. A model told to emit an exact line routinely formats it, and every such reply is
/// a correct answer that would otherwise be refused, cost the seller its fee, AND strike a healthy
/// harness. The gate exists to separate an answer from a dead harness, not to test formatting
/// obedience. Nothing is weakened: the decoration characters cannot make an unmarked message
/// marked.
///
/// One repeated leading marker is stripped from the answer: an agent that wrote the marker line
/// and then `MARKER <answer>` (measured on muse) means `<answer>`, not a reply naming the marker.
/// An empty answer is `None`: a marker with nothing behind it is not an answer, and §6.4 refuses
/// an empty inline delivery anyway.
pub fn marked_inline_answer(message: &str) -> Option<&str> {
    let text = message.trim_start();
    // A fenced first line (```), which some models wrap the whole reply in, is stepped over.
    let text = match text.strip_prefix("```") {
        Some(rest) => rest.split_once('\n').map(|(_, body)| body)?.trim_start(),
        None => text,
    };
    let mut answer = after_leading_marker(text)?.trim();
    if let Some(again) = after_leading_marker(answer) {
        answer = again.trim();
    }
    (!answer.is_empty()).then_some(answer)
}

/// Everything after the marker when `text`'s first line starts with it (decoration allowed), else
/// `None`. `text` must not start with whitespace.
fn after_leading_marker(text: &str) -> Option<&str> {
    let first = text.split('\n').next().unwrap_or("");
    // Strip the heading decoration a model reaches for, then take the first word.
    let line = first.trim_start_matches('#').trim_start();
    let start = first.len() - line.len();
    let end = line.find(char::is_whitespace).unwrap_or(line.len());
    let decoration = |c: char| c == '`' || c == '*' || c == '_';
    let bare = line[..end].trim_matches(decoration);
    let bare = bare
        .strip_suffix(':')
        .unwrap_or(bare)
        .trim_matches(decoration);
    (bare == INLINE_ANSWER_MARKER).then(|| &text[start + end..])
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
/// Whether `update` ends the agent message being streamed: only tool-call-type updates (and the
/// turn's end) do. See [`AgentMessageCapture::observe`].
fn closes_message(update: &SessionUpdate) -> bool {
    match update {
        SessionUpdate::ToolCall { .. }
        | SessionUpdate::ToolResult { .. }
        | SessionUpdate::PermissionRequest(_)
        | SessionUpdate::TurnEnded(_) => true,
        SessionUpdate::Ext(ext) => {
            let kind = ext
                .params
                .get("update")
                .unwrap_or(&ext.params)
                .get("sessionUpdate")
                .and_then(serde_json::Value::as_str);
            matches!(kind, Some("tool_call" | "tool_call_update"))
        }
        SessionUpdate::Plan { .. }
        | SessionUpdate::AgentMessage(_)
        | SessionUpdate::AgentMessageChunk(_) => false,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AgentMessageCapture {
    last: Option<String>,
    /// Chunks of the message currently being streamed, not yet closed by a boundary.
    pending: String,
}

impl AgentMessageCapture {
    /// Retains the agent's last complete MESSAGE, so the last one it sent wins.
    ///
    /// A message arrives one of two ways. `AgentMessage` carries a whole one. `AgentMessageChunk`
    /// carries a FRAGMENT, and a harness is free to split one message across as many as it likes —
    /// measured in this repo, a Claude harness sent `login=pmilic021` as `login` then `=pmilic021`.
    /// So a chunk is accumulated, never retained on its own: retaining it would make the captured
    /// "message" the last fragment, which is a different string from what the agent said.
    ///
    /// A TOOL-CALL-type update closes the run of chunks (`tool_call`, `tool_call_update`, a tool
    /// result, a permission request): an agent streams its text, then calls a tool, and the text
    /// after that call is a new message, not a continuation. `TurnEnded` closes the last one.
    /// Non-content updates — usage, thoughts, plans, mode/command/session-info notices — do NOT:
    /// harnesses emit them mid-stream (the ACP driver passes them through as `Ext`), and closing on
    /// one made the captured message only the tail after it, which then failed the marker gate.
    ///
    /// Whitespace-only text is not an account of anything and must not displace a real message;
    /// agents routinely close a turn with a bare newline chunk. That test is applied to the
    /// assembled message, NOT to each chunk — a blank chunk inside a message is part of it.
    pub(crate) fn observe(&mut self, event: RunEvent<'_>) {
        let RunEvent::Update(update) = event else {
            return;
        };
        match update {
            SessionUpdate::AgentMessageChunk(block) => {
                if let Some(text) = content_block_text(block) {
                    self.pending.push_str(&text);
                }
            }
            SessionUpdate::AgentMessage(_) => {
                // A whole message of its own: close whatever was streaming, then take this one.
                self.close_pending();
                if let Some(text) = update_text(update)
                    && !text.trim().is_empty()
                {
                    self.last = Some(text);
                }
            }
            update if closes_message(update) => self.close_pending(),
            _ => {}
        }
    }

    /// Promote the accumulated chunks to the last message, if they amount to one.
    fn close_pending(&mut self) {
        let assembled = std::mem::take(&mut self.pending);
        if !assembled.trim().is_empty() {
            self.last = Some(assembled);
        }
    }

    /// The retained message, or `None` when the agent sent no text at all.
    ///
    /// `None` is a positive claim — the agent said nothing — and a caller must not render it as an
    /// unknown, because a capture that silently failed to fill would be indistinguishable from it.
    pub(crate) fn into_last_message(mut self) -> Option<String> {
        // A turn can end without a terminal update reaching us (a dropped stream, a harness that
        // never sends `TurnEnded`), so the last run of chunks is closed here too.
        self.close_pending();
        self.last
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
        AgentMessageCapture, EngineError, INLINE_ANSWER_MARKER, RunEvent, RunOutcome, RunParams,
        marked_inline_answer, run_job,
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

    /// ⛔ A harness is free to split ONE message across many chunks, and they do: this repo
    /// records a Claude harness sending `login=pmilic021` as `login` then `=pmilic021`
    /// (`evidence/20260914T085619Z-github-proxy-swap`). A capture that retained the last CHUNK
    /// would report `=pmilic021` as the agent's message — and under inline delivery (§6.4) that
    /// fragment is what the buyer pays for, with both ends agreeing on the digest of the wrong
    /// bytes. The assembled message is the only correct answer.
    #[test]
    fn streamed_chunks_assemble_into_one_message() {
        let chunk =
            |text: &str| SessionUpdate::AgentMessageChunk(ContentBlock::Text { text: text.into() });
        let mut capture = AgentMessageCapture::default();
        for update in [&chunk("login"), &chunk("=pmilic021")] {
            capture.observe(RunEvent::Update(update));
        }
        capture.observe(RunEvent::Update(&SessionUpdate::TurnEnded(
            StopReason::Completed,
        )));
        assert_eq!(
            capture.into_last_message().as_deref(),
            Some("login=pmilic021"),
            "chunks of one message must assemble, not overwrite each other"
        );
    }

    /// A blank chunk INSIDE a message is part of it — the whitespace test belongs to the assembled
    /// message, not to each fragment. Applied per chunk it would drop the space in `a b`.
    #[test]
    fn a_blank_chunk_inside_a_message_is_kept() {
        let chunk =
            |text: &str| SessionUpdate::AgentMessageChunk(ContentBlock::Text { text: text.into() });
        let mut capture = AgentMessageCapture::default();
        for update in [&chunk("a"), &chunk(" "), &chunk("b")] {
            capture.observe(RunEvent::Update(update));
        }
        assert_eq!(capture.into_last_message().as_deref(), Some("a b"));
    }

    /// A tool call ends the message that was streaming. Text after it is a NEW message, and the
    /// last one wins — otherwise every turn would report one message glued together end to end.
    #[test]
    fn a_tool_call_closes_the_streaming_message() {
        let chunk =
            |text: &str| SessionUpdate::AgentMessageChunk(ContentBlock::Text { text: text.into() });
        let tool = SessionUpdate::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            input: serde_json::Value::Null,
        };
        let mut capture = AgentMessageCapture::default();
        for update in [&chunk("looking"), &tool, &chunk("done")] {
            capture.observe(RunEvent::Update(update));
        }
        assert_eq!(capture.into_last_message().as_deref(), Some("done"));
    }

    /// §6.4 — the marker is an OPT-IN, and everything unmarked stays out of inline delivery. The
    /// vendor error string is the case that matters: it is what a quota-dead harness returns, and
    /// reading it as an answer is paying for a broken seat.
    #[test]
    fn only_a_marked_message_yields_an_inline_answer() {
        assert_eq!(
            marked_inline_answer(&format!("{INLINE_ANSWER_MARKER}\nEurope/Zagreb")),
            Some("Europe/Zagreb")
        );
        assert_eq!(
            marked_inline_answer(&format!("  \n{INLINE_ANSWER_MARKER}\n\nEurope/Zagreb\n")),
            Some("Europe/Zagreb"),
            "leading blank lines and trailing whitespace are not part of the answer"
        );
        assert_eq!(
            marked_inline_answer(BLOCKED_HOST_MESSAGE),
            None,
            "a vendor error string is not an answer"
        );
        assert_eq!(
            marked_inline_answer("Upgrade your plan to continue"),
            None,
            "an exhausted-plan notice is not an answer"
        );
        assert_eq!(
            marked_inline_answer("I cannot do this without network access"),
            None,
            "a refusal is not an answer — it takes the refusal path unchanged"
        );
        assert_eq!(
            marked_inline_answer(INLINE_ANSWER_MARKER),
            None,
            "the bare marker delivers nothing"
        );
        assert_eq!(
            marked_inline_answer(&format!("{INLINE_ANSWER_MARKER}\n   \n")),
            None,
            "a marker with only whitespace behind it delivers nothing"
        );
        assert_eq!(
            marked_inline_answer(&format!("{INLINE_ANSWER_MARKER}-DRAFT\nx")),
            None,
            "the marker must be alone on its line, not a prefix of a longer token"
        );
        assert_eq!(
            marked_inline_answer(&format!("{INLINE_ANSWER_MARKER}-DRAFT 47")),
            None,
            "a longer token is not the marker, on one line either"
        );
        assert_eq!(
            marked_inline_answer(&format!("answer: {INLINE_ANSWER_MARKER} 47")),
            None,
            "the marker must be the first word of the first line"
        );
        assert_eq!(
            marked_inline_answer(&format!("{INLINE_ANSWER_MARKER}   \n\n")),
            None,
            "a one-line marker with nothing behind it delivers nothing"
        );

        // A model told to emit an exact line formats it. Each of these is a CORRECT answer, and
        // refusing it costs the seller its fee and strikes a healthy harness for nothing.
        for decorated in [
            format!("`{INLINE_ANSWER_MARKER}`\nEurope/Zagreb"),
            format!("**{INLINE_ANSWER_MARKER}**\nEurope/Zagreb"),
            format!("# {INLINE_ANSWER_MARKER}\nEurope/Zagreb"),
            format!("{INLINE_ANSWER_MARKER}:\nEurope/Zagreb"),
            format!("{INLINE_ANSWER_MARKER}\r\nEurope/Zagreb"),
            format!("   {INLINE_ANSWER_MARKER}   \nEurope/Zagreb"),
            format!("```\n{INLINE_ANSWER_MARKER}\nEurope/Zagreb"),
        ] {
            assert_eq!(
                marked_inline_answer(&decorated),
                Some("Europe/Zagreb"),
                "decoration around the token must not lose the answer: {decorated:?}"
            );
        }
        assert_eq!(
            marked_inline_answer(&format!("here you go:\n{INLINE_ANSWER_MARKER}\nx")),
            None,
            "a marker buried below the first line is one a pasted log could forge"
        );
    }

    /// rc2 sweep: codex, opencode and muse put the answer on the marker line. That is the marker
    /// as the first word of the first line, and the answer is the rest of the reply.
    #[test]
    fn the_answer_may_share_the_marker_line() {
        assert_eq!(
            marked_inline_answer(&format!("{INLINE_ANSWER_MARKER} WORKER1 47")),
            Some("WORKER1 47")
        );
        assert_eq!(
            marked_inline_answer(&format!("{INLINE_ANSWER_MARKER} WORKER1 47\nand more\n")),
            Some("WORKER1 47\nand more"),
            "the rest of the marker line plus the following lines"
        );
        for decorated in [
            format!("**{INLINE_ANSWER_MARKER}** WORKER1 47"),
            format!("`{INLINE_ANSWER_MARKER}` WORKER1 47"),
            format!("# {INLINE_ANSWER_MARKER} WORKER1 47"),
            format!("{INLINE_ANSWER_MARKER}: WORKER1 47"),
            format!("```\n{INLINE_ANSWER_MARKER} WORKER1 47"),
            format!("\n  {INLINE_ANSWER_MARKER}\tWORKER1 47  "),
        ] {
            assert_eq!(
                marked_inline_answer(&decorated),
                Some("WORKER1 47"),
                "{decorated:?}"
            );
        }
    }

    /// muse's "passing" run sent the marker twice; the second must not become part of the answer.
    #[test]
    fn one_repeated_leading_marker_is_stripped_from_the_answer() {
        assert_eq!(
            marked_inline_answer(&format!(
                "{INLINE_ANSWER_MARKER}\n{INLINE_ANSWER_MARKER} WORKERMU1 47"
            )),
            Some("WORKERMU1 47")
        );
        assert_eq!(
            marked_inline_answer(&format!(
                "{INLINE_ANSWER_MARKER}\n**{INLINE_ANSWER_MARKER}**\nWORKERMU1 47"
            )),
            Some("WORKERMU1 47")
        );
        assert_eq!(
            marked_inline_answer(&format!("{INLINE_ANSWER_MARKER}\n{INLINE_ANSWER_MARKER}")),
            None,
            "two markers and nothing else is still an empty answer"
        );
        // Only ONE repeat is stripped, and a longer token is answer text.
        assert_eq!(
            marked_inline_answer(&format!(
                "{INLINE_ANSWER_MARKER} {INLINE_ANSWER_MARKER} {INLINE_ANSWER_MARKER} x"
            )),
            Some(format!("{INLINE_ANSWER_MARKER} x").as_str())
        );
        assert_eq!(
            marked_inline_answer(&format!(
                "{INLINE_ANSWER_MARKER}\n{INLINE_ANSWER_MARKER}-DRAFT x"
            )),
            Some(format!("{INLINE_ANSWER_MARKER}-DRAFT x").as_str())
        );
    }

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

    fn wire(kind: &str) -> SessionUpdate {
        SessionUpdate::Ext(crate::driver::ExtMethod {
            method: "session/update".into(),
            params: serde_json::json!({"sessionId": "s", "update": {"sessionUpdate": kind}}),
        })
    }

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentBlock::Text { text: text.into() })
    }

    /// rc2: usage, thought, plan and notice updates arriving between the chunks of ONE message do
    /// not split it; the captured message is the whole of it, so the marker on its first line is
    /// still first.
    #[test]
    fn non_content_updates_mid_message_do_not_split_it() {
        for between in [
            wire("usage_update"),
            wire("agent_thought_chunk"),
            wire("plan"),
            wire("available_commands_update"),
            wire("session_info_update"),
            wire("current_mode_update"),
            SessionUpdate::Plan {
                entries: vec!["step".into()],
            },
        ] {
            let mut capture = AgentMessageCapture::default();
            for update in [
                chunk(INLINE_ANSWER_MARKER),
                between.clone(),
                chunk(" WORKER1"),
                wire("usage_update"),
                chunk(" 47"),
                SessionUpdate::TurnEnded(StopReason::Completed),
            ] {
                capture.observe(RunEvent::Update(&update));
            }
            let message = capture.into_last_message().expect("a message");
            assert_eq!(
                message,
                format!("{INLINE_ANSWER_MARKER} WORKER1 47"),
                "{between:?}"
            );
            assert_eq!(marked_inline_answer(&message), Some("WORKER1 47"));
        }
    }

    /// A tool call still closes the message: the text after it is a new message, and the last wins.
    #[test]
    fn tool_call_updates_still_close_the_message() {
        for tool in [
            wire("tool_call"),
            wire("tool_call_update"),
            SessionUpdate::ToolCall {
                id: "1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
            SessionUpdate::ToolResult {
                id: "1".into(),
                output: serde_json::json!({}),
            },
        ] {
            let mut capture = AgentMessageCapture::default();
            for update in [chunk("let me look"), tool.clone(), chunk("the answer")] {
                capture.observe(RunEvent::Update(&update));
            }
            assert_eq!(
                capture.into_last_message().as_deref(),
                Some("the answer"),
                "{tool:?}"
            );
        }
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

    // ---- Session jobs (docs/specs/session-jobs.md) -------------------------------------------

    fn session_script(id: &str, updates: Vec<SessionUpdate>) -> ScriptedSession {
        ScriptedSession {
            session_id: id.into(),
            updates,
            artifacts: Vec::new(),
        }
    }

    fn session_params(
        resume: Option<&str>,
        wrap: Option<std::time::SystemTime>,
    ) -> super::SessionTurnParams {
        super::SessionTurnParams {
            session_config: RunParams::mock_defaults().session_config,
            resume: resume.map(str::to_owned),
            wrap_up_at: wrap,
        }
    }

    fn released(text: &str) -> Box<super::PromptRelease<'static>> {
        let text = text.to_owned();
        Box::new(move |_reset| {
            Ok(Some(RunParams::mock_defaults().prompt).map(|mut p| {
                p.input = vec![ContentBlock::Text { text }];
                p
            }))
        })
    }

    fn ran(end: super::SessionTurnEnd) -> super::SessionTurnOutcome {
        match end {
            super::SessionTurnEnd::Ran(outcome) => *outcome,
            super::SessionTurnEnd::Withheld => panic!("the turn was withheld"),
        }
    }

    /// Failure test 8: `session/load` reloads the recorded id when the agent advertises it; a load
    /// error, or no capability, falls back to `session/new` and reports `context_reset`.
    #[test]
    fn session_load_reloads_or_falls_back_to_new_with_context_reset() {
        let done = vec![SessionUpdate::TurnEnded(StopReason::Completed)];
        // Advertised and succeeds: no new session, no reset.
        let mut driver = MockDriver::new(
            RuntimeId("mock".into()),
            vec![session_script("unused", done.clone())],
        )
        .with_load_session(Ok(()));
        let mut log = EventLog::open(test_path("session-load-ok")).expect("open log");
        let out = ran(block_on(super::run_session_job(
            &mut driver,
            &mut log,
            &JobId("j".into()),
            session_params(Some("acp-1"), None),
            released("next"),
            &mut |_| {},
        ))
        .expect("turn"));
        assert_eq!(driver.load_calls(), &["acp-1".to_owned()]);
        assert_eq!(driver.new_calls(), 0);
        assert!(!out.context_reset);
        assert_eq!(out.session_id, "acp-1");

        // Advertised but the load fails: a new session in the same container, reset reported.
        let mut driver = MockDriver::new(
            RuntimeId("mock".into()),
            vec![session_script("fresh", done.clone())],
        )
        .with_load_session(Err(DriverError::Other("transcript gone".into())));
        let mut log = EventLog::open(test_path("session-load-err")).expect("open log");
        let mut reset_seen = None;
        let release: Box<super::PromptRelease<'_>> = Box::new(|reset| {
            reset_seen = Some(reset);
            Ok(Some(RunParams::mock_defaults().prompt))
        });
        let out = ran(block_on(super::run_session_job(
            &mut driver,
            &mut log,
            &JobId("j".into()),
            session_params(Some("acp-1"), None),
            release,
            &mut |_| {},
        ))
        .expect("turn"));
        assert_eq!(driver.load_calls(), &["acp-1".to_owned()]);
        assert_eq!(driver.new_calls(), 1);
        assert!(out.context_reset);
        assert_eq!(out.session_id, "fresh");
        assert_eq!(
            reset_seen,
            Some(true),
            "the release learns of the reset before the prompt exists"
        );

        // Not advertised: never asked to load, reset reported.
        let mut driver = MockDriver::new(
            RuntimeId("mock".into()),
            vec![session_script("fresh", done.clone())],
        );
        let mut log = EventLog::open(test_path("session-load-absent")).expect("open log");
        let out = ran(block_on(super::run_session_job(
            &mut driver,
            &mut log,
            &JobId("j".into()),
            session_params(Some("acp-1"), None),
            released("next"),
            &mut |_| {},
        ))
        .expect("turn"));
        assert!(driver.load_calls().is_empty());
        assert!(out.context_reset);

        // Turn 1 (no recorded id): a plain new session, no reset.
        let mut driver = MockDriver::new(
            RuntimeId("mock".into()),
            vec![session_script("first", done)],
        )
        .with_load_session(Ok(()));
        let mut log = EventLog::open(test_path("session-first")).expect("open log");
        let out = ran(block_on(super::run_session_job(
            &mut driver,
            &mut log,
            &JobId("j".into()),
            session_params(None, None),
            released("task"),
            &mut |_| {},
        ))
        .expect("turn"));
        assert!(driver.load_calls().is_empty());
        assert!(!out.context_reset);
    }

    /// Failure test 5 (orchestrator half): a withheld prompt never reaches the agent.
    #[test]
    fn withheld_prompt_never_reaches_the_agent() {
        let mut driver = MockDriver::new(
            RuntimeId("mock".into()),
            vec![session_script(
                "s",
                vec![SessionUpdate::TurnEnded(StopReason::Completed)],
            )],
        )
        .with_load_session(Ok(()));
        let mut log = EventLog::open(test_path("session-withheld")).expect("open log");
        let end = block_on(super::run_session_job(
            &mut driver,
            &mut log,
            &JobId("j".into()),
            session_params(Some("acp-1"), None),
            Box::new(|_| Ok(None)),
            &mut |_| {},
        ))
        .expect("withheld is not an error");
        assert!(matches!(end, super::SessionTurnEnd::Withheld));
        assert!(driver.prompt_history().is_empty(), "no prompt was sent");
        assert_eq!(
            driver.shutdown_calls(),
            1,
            "the agent is shut down on the withheld exit too"
        );
    }

    /// Failure test 9 (orchestrator half): an agent still running at the wrap-up instant is
    /// cancelled and the turn is reported `cancelled_for_wrap_up`; a turn that ends first is not.
    #[test]
    fn wrap_up_cancels_a_running_turn_and_reports_it() {
        // A turn that would keep streaming: the cancel ends it.
        let busy: Vec<SessionUpdate> = (0..50)
            .map(|i| {
                SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                    text: format!("step {i}"),
                }])
            })
            .chain([SessionUpdate::TurnEnded(StopReason::Completed)])
            .collect();
        let mut driver = MockDriver::new(
            RuntimeId("mock".into()),
            vec![session_script("s", busy.clone())],
        );
        let mut log = EventLog::open(test_path("session-wrap")).expect("open log");
        let out = ran(block_on(super::run_session_job(
            &mut driver,
            &mut log,
            &JobId("j".into()),
            session_params(None, Some(std::time::UNIX_EPOCH)),
            released("task"),
            &mut |_| {},
        ))
        .expect("turn"));
        assert!(driver.cancel_fired(), "the wrap-up cancel was sent");
        assert!(out.cancelled_for_wrap_up);
        assert_eq!(out.run.terminal, JobExecutionStatus::Cancelled);

        // Plenty of time: the turn ends on its own and is not marked.
        let mut driver = MockDriver::new(RuntimeId("mock".into()), vec![session_script("s", busy)]);
        let mut log = EventLog::open(test_path("session-nowrap")).expect("open log");
        let out = ran(block_on(super::run_session_job(
            &mut driver,
            &mut log,
            &JobId("j".into()),
            session_params(
                None,
                Some(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)),
            ),
            released("task"),
            &mut |_| {},
        ))
        .expect("turn"));
        assert!(!driver.cancel_fired());
        assert!(!out.cancelled_for_wrap_up);
        assert_eq!(out.run.terminal, JobExecutionStatus::Completed);
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
