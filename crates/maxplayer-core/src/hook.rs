//! The hook runner: a seller's OWN agent behind a command (docs/specs/hook-agents.md §5).
//!
//! The daemon runs the hook argv directly (no shell), writes one JSON object + `\n` to its stdin,
//! closes stdin, and drains stdout/stderr until the process exits or the cutoff.
//! Everything here is blocking and synchronous on purpose: the caller runs it on a blocking task,
//! and the run loop never waits on a hook.
//!
//! Supervision rules (§5.1 "Supervision"):
//! - the child runs in its own process group, so a kill reaches every descendant still in it;
//! - stdin is written on its own thread while both output pipes drain, so a hook that never reads
//!   stdin, or fills a pipe, cannot wedge the runner; a broken stdin pipe is not itself an error;
//! - stdout is capped (first overflow ⇒ kill, contract violation); stderr keeps only its tail;
//! - at the cutoff: SIGTERM to the group, a grace period, SIGKILL to the group, reap;
//! - after the child exits, the group is SIGKILLed anyway (a hook that wants a long-lived worker
//!   starts it outside its own group, with its stdout pointed elsewhere), and stdout gets a bounded
//!   wait for EOF: a pipe still held open then means stdout is incomplete and the answer is refused.
//!   All pipes are non-blocking and pumped by one thread, so nothing is left blocked afterwards.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The contract version written as `"v"` on every event and as `MAXPLAYER_HOOK_V` in the env.
pub const HOOK_CONTRACT_VERSION: u32 = 1;
/// The one `MAXPLAYER_*` variable a hook sees.
pub const HOOK_VERSION_ENV: &str = "MAXPLAYER_HOOK_V";
/// Stdout ceiling: more than this is a contract violation, and the hook is killed on the overflow.
pub const STDOUT_MAX_BYTES: usize = 1024 * 1024;
/// Stderr is kept only as its last this-many bytes, for the seller log.
pub const STDERR_TAIL_BYTES: usize = 4 * 1024;
/// SIGTERM → SIGKILL grace at the cutoff.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// The fixed budget of a `ping` or `close` call.
pub const CONTROL_EVENT_BUDGET: Duration = Duration::from_secs(30);
/// Seconds before the job deadline at which a `job` call is cut, leaving time to sign + publish.
pub const PUBLISH_MARGIN_SECS: u64 = 30;
/// How long stdout/stderr get to reach EOF after the hook exits. Stdout still open then is incomplete.
const READER_JOIN_BUDGET: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);

/// A turn's status as the hook states it (`done` | `needs_input` | `out_of_time`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookStatus {
    Done,
    NeedsInput,
    OutOfTime,
}

impl HookStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "done" => Some(Self::Done),
            "needs_input" => Some(Self::NeedsInput),
            "out_of_time" => Some(Self::OutOfTime),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::NeedsInput => "needs_input",
            Self::OutOfTime => "out_of_time",
        }
    }
}

/// A validated answer: the decoded bytes exactly as the hook gave them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookAnswer {
    pub status: HookStatus,
    pub answer: String,
}

/// What one `job` call came to, in the terms of the §5.1 outcome table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    /// Exit 0, a valid object, a non-empty answer within the cap: deliver it.
    Answer(HookAnswer),
    /// Exit 0, a valid answer, but over the inline cap. Never truncated. `delivery_failed`, no strike.
    TooLarge { bytes: usize, cap: usize },
    /// Exit 0, `{"status":"cannot_deliver","reason":…}`: the hook's honest refusal (spec §5.0).
    /// `delivery_failed`, no strike. The reason is for the seller log only.
    CannotDeliver(String),
    /// The hook broke the contract (bad stdout, non-zero exit, signal). `execution_failed` + strike.
    Broken(String),
    /// Cut at the cutoff, or the cutoff had already passed. `execution_failed`, NO strike (job clock).
    OutOfTime { started: bool },
}

impl JobOutcome {
    /// Whether this outcome implicates the hook (a roster strike), per the §5.1 table.
    pub fn strikes(&self) -> bool {
        matches!(self, Self::Broken(_))
    }
}

/// How the process itself ended, before any stdout is interpreted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Ran to completion with this status code (`None` = killed by a signal).
    Exited { code: Option<i32> },
    /// Stdout went over [`STDOUT_MAX_BYTES`]; the group was killed.
    StdoutOverflow,
    /// Still running at the cutoff; the group was killed.
    CutOff,
    /// The cutoff had already passed: nothing was spawned.
    NotStarted,
    /// The program could not be spawned at all.
    SpawnFailed(String),
}

/// One supervised run.
#[derive(Clone, Debug)]
pub struct RunReport {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    /// Whether stdout reached EOF without a read error or overflow. A `job` answer is accepted only
    /// from complete stdout: a pipe still open after the exit (held by a process that left the
    /// group) may be a prefix.
    pub stdout_complete: bool,
    /// The last [`STDERR_TAIL_BYTES`] of stderr, lossily decoded. Seller log only, never the buyer.
    pub stderr_tail: String,
    pub wall: Duration,
}

/// The JSON line for a `job` event (§5.1). `inputs_dir`/`output` are `null` when absent.
#[allow(clippy::too_many_arguments)]
pub fn job_event(
    invocation: &str,
    attempt: u32,
    answer_max_bytes: usize,
    job_id: &str,
    turn: u32,
    session: bool,
    prompt: &str,
    output: Option<&str>,
    inputs_dir: Option<&Path>,
    deadline_unix: u64,
) -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({
        "v": HOOK_CONTRACT_VERSION,
        "event": "job",
        "invocation": invocation,
        "attempt": attempt,
        "job_id": job_id,
        "turn": turn,
        "session": session,
        "prompt": prompt,
        "output": output,
        "inputs_dir": inputs_dir.map(|p| p.to_string_lossy().into_owned()),
        "answer_max_bytes": answer_max_bytes,
        "files_delivered": false,
        "deadline_unix": deadline_unix,
    }))
    .expect("a JSON value always serializes");
    line.push(b'\n');
    line
}

/// Why a `ping` is sent (§5.2): `boot` and `restore` should prove the agent can answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingReason {
    Boot,
    Interval,
    Restore,
}

impl PingReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Interval => "interval",
            Self::Restore => "restore",
        }
    }
}

/// The JSON line for a `ping` event (§5.2).
pub fn ping_event(reason: PingReason) -> Vec<u8> {
    let mut line = serde_json::to_vec(
        &serde_json::json!({"v": HOOK_CONTRACT_VERSION, "event": "ping", "reason": reason.as_str()}),
    )
    .expect("serializes");
    line.push(b'\n');
    line
}

/// The JSON line for a `close` event (§5.3).
pub fn close_event(job_id: &str) -> Vec<u8> {
    let mut line = serde_json::to_vec(
        &serde_json::json!({"v": HOOK_CONTRACT_VERSION, "event": "close", "job_id": job_id}),
    )
    .expect("serializes");
    line.push(b'\n');
    line
}

/// The cutoff of a `job` call: `deadline_unix − PUBLISH_MARGIN_SECS`, as a monotonic instant.
/// A deadline already inside the margin yields `now` (the call is refused without spawning).
pub fn job_cutoff(deadline_unix: u64, now_unix: u64, now: Instant) -> Instant {
    let remaining = deadline_unix
        .saturating_sub(PUBLISH_MARGIN_SECS)
        .saturating_sub(now_unix);
    now + Duration::from_secs(remaining)
}

/// The answer object as the hook wrote it: a JSON object with no duplicate key at all (known or
/// unknown), decoded strictly (§5.1 "Answer").
struct WireAnswer {
    status: String,
    answer: Option<String>,
    reason: Option<String>,
}

/// A top-level JSON object that refuses any repeated key — serde_json's own map keeps the last.
struct StrictObject(serde_json::Map<String, serde_json::Value>);

impl<'de> serde::Deserialize<'de> for StrictObject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictObject;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<StrictObject, A::Error> {
                let mut out = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if out.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
                    }
                    out.insert(key, value);
                }
                Ok(StrictObject(out))
            }
        }
        // `deserialize_map`, not `deserialize_any`: an array (a positional struct) is refused.
        deserializer.deserialize_map(Visitor)
    }
}

impl WireAnswer {
    fn decode(stdout: &[u8]) -> Result<Self, String> {
        let StrictObject(map) = serde_json::from_slice(stdout).map_err(|e| e.to_string())?;
        // A present field must have its type; `null` is not "absent".
        let field = |name: &str| -> Result<Option<String>, String> {
            match map.get(name) {
                None => Ok(None),
                Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
                Some(other) => Err(format!("{name:?} must be a string, not {other}")),
            }
        };
        Ok(Self {
            status: field("status")?.ok_or("missing \"status\"")?,
            answer: field("answer")?,
            reason: field("reason")?,
        })
    }
}

/// Interpret a finished run as a `job` outcome (§5.1). `cap` is the inline answer cap in bytes.
pub fn classify_job(report: &RunReport, cap: usize) -> JobOutcome {
    match &report.exit {
        Exit::NotStarted => return JobOutcome::OutOfTime { started: false },
        Exit::CutOff => return JobOutcome::OutOfTime { started: true },
        Exit::StdoutOverflow => {
            return JobOutcome::Broken(format!("stdout over {STDOUT_MAX_BYTES} bytes"));
        }
        Exit::SpawnFailed(error) => return JobOutcome::Broken(format!("could not start: {error}")),
        Exit::Exited { code: Some(0) } => {}
        Exit::Exited { code: Some(code) } => return JobOutcome::Broken(format!("exit status {code}")),
        Exit::Exited { code: None } => return JobOutcome::Broken("killed by a signal".into()),
    }
    if !report.stdout_complete {
        return JobOutcome::Broken(format!(
            "stdout not complete: still open {}s after the hook exited (a process outside its group kept it), or unreadable",
            READER_JOIN_BUDGET.as_secs()
        ));
    }
    // serde_json refuses invalid UTF-8 and trailing non-whitespace; `decode` refuses a non-object,
    // any duplicate key and a wrongly typed (or null) known field. Unknown fields are tolerated.
    let wire = match WireAnswer::decode(&report.stdout) {
        Ok(wire) => wire,
        Err(error) => return JobOutcome::Broken(format!("stdout is not the answer object: {error}")),
    };
    if wire.status == "cannot_deliver" {
        return JobOutcome::CannotDeliver(wire.reason.unwrap_or_default());
    }
    let Some(status) = HookStatus::parse(&wire.status) else {
        return JobOutcome::Broken(format!("unknown status {:?}", wire.status));
    };
    let Some(answer) = wire.answer else {
        return JobOutcome::Broken("no answer".into());
    };
    if answer.trim().is_empty() {
        return JobOutcome::Broken("empty answer".into());
    }
    if answer.len() > cap {
        return JobOutcome::TooLarge { bytes: answer.len(), cap };
    }
    JobOutcome::Answer(HookAnswer { status, answer })
}

/// Send one `ping` and say whether the hook is healthy. Blocking; bounded by [`CONTROL_EVENT_BUDGET`].
pub fn ping(argv: &[String], cwd: &Path, reason: PingReason) -> Result<(), String> {
    let report = run_control(argv, &ping_event(reason), cwd, Instant::now() + CONTROL_EVENT_BUDGET);
    if ping_healthy(&report) {
        Ok(())
    } else {
        Err(match &report.exit {
            Exit::Exited { code: Some(code) } => format!("ping exit status {code}"),
            Exit::Exited { code: None } => "ping killed by a signal".into(),
            Exit::CutOff => format!("ping did not answer within {}s", CONTROL_EVENT_BUDGET.as_secs()),
            Exit::SpawnFailed(error) => format!("ping could not start: {error}"),
            Exit::StdoutOverflow => "ping stdout over the ceiling".into(),
            Exit::NotStarted => "ping not started".into(),
        })
    }
}

/// Send `close` for `job_id`, best effort: exit status ignored (§5.3).
pub fn close(argv: &[String], cwd: &Path, job_id: &str) {
    let _ = run_control(argv, &close_event(job_id), cwd, Instant::now() + CONTROL_EVENT_BUDGET);
}

/// Whether a finished `ping` run means healthy: exit 0 before the cutoff. Stdout is ignored.
pub fn ping_healthy(report: &RunReport) -> bool {
    matches!(report.exit, Exit::Exited { code: Some(0) })
}

/// What a call does with the hook's stdout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StdoutMode {
    /// A `job`: stdout is the answer, kept up to [`STDOUT_MAX_BYTES`] (overflow ⇒ kill).
    Keep,
    /// A control event (`ping`, `close`): stdout is drained and discarded, never a violation.
    Discard,
}

/// Run `argv` once under the supervision rules, feeding `stdin`, in `cwd`, until `cutoff`. The
/// child gets the daemon's environment minus every `MAXPLAYER_*` variable, plus
/// [`HOOK_VERSION_ENV`].
pub fn run(argv: &[String], stdin: &[u8], cwd: &Path, cutoff: Instant) -> RunReport {
    supervise(argv, stdin, cwd, cutoff, std::env::vars_os(), StdoutMode::Keep, None)
}

/// A started hook group's identity, journaled BEFORE the hook is allowed to run (§5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    /// The process-group id (= the leader's pid).
    pub pgid: u32,
    /// The leader's identity, `<boot id>/<start time>` ([`process_identity`]), so a later daemon can
    /// tell our leader from a recycled pid or a previous boot. `None` where it cannot be read.
    pub start: Option<String>,
}

/// The gate the hook is exec'd through: it waits for one `go` line on stdin and only then becomes
/// the hook (same pid, same group). If the daemon dies before sending it, stdin closes, `read`
/// fails, and the gate exits without the hook ever running.
const GATE_SCRIPT: &str = r#"IFS= read -r go && [ "$go" = go ] && exec "$@"; exit 125"#;
const GATE_GO: &[u8] = b"go\n";

/// [`run`] for a `job` call, made idempotent across a daemon crash (§5.1): the hook is spawned
/// behind a gate, `journal` records its group identity, and only after `journal` returns `Ok` is
/// the gate released. So either the hook never ran, or its group is on disk for the next boot to
/// reap. A `journal` error kills the gated group (the hook never ran) and reports `SpawnFailed`.
pub fn run_gated(
    argv: &[String],
    stdin: &[u8],
    cwd: &Path,
    cutoff: Instant,
    journal: &mut dyn FnMut(&Started) -> Result<(), String>,
) -> RunReport {
    if argv.is_empty() {
        return supervise(argv, stdin, cwd, cutoff, std::env::vars_os(), StdoutMode::Keep, None);
    }
    let mut gated: Vec<String> =
        vec!["/bin/sh".into(), "-c".into(), GATE_SCRIPT.into(), "maxplayer-hook-gate".into()];
    gated.extend(argv.iter().cloned());
    let mut payload = GATE_GO.to_vec();
    payload.extend_from_slice(stdin);
    supervise(&gated, &payload, cwd, cutoff, std::env::vars_os(), StdoutMode::Keep, Some(journal))
}

/// [`run`] for a control event: the same supervision, with stdout discarded (§5.2: ignored).
pub fn run_control(argv: &[String], stdin: &[u8], cwd: &Path, cutoff: Instant) -> RunReport {
    supervise(argv, stdin, cwd, cutoff, std::env::vars_os(), StdoutMode::Discard, None)
}

/// [`run`] with the parent environment passed in, so the scrub is testable without mutating the
/// process environment (which every concurrently running test shares).
pub fn run_with_env(
    argv: &[String],
    stdin: &[u8],
    cwd: &Path,
    cutoff: Instant,
    parent_env: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> RunReport {
    supervise(argv, stdin, cwd, cutoff, parent_env, StdoutMode::Keep, None)
}

/// The one supervisor. Single-threaded on purpose: all three pipes are non-blocking and pumped
/// from this loop, so nothing can be left blocked on a pipe after it returns — every pipe is
/// closed (dropped) before the report is built.
///
/// Order of resolution, decided once:
/// 1. stdout overflow (Keep mode) ⇒ kill the group, [`Exit::StdoutOverflow`];
/// 2. the leader has exited ⇒ if that is first observed at or after the cutoff, [`Exit::CutOff`]
///    (the deadline wins over a late exit); else its status;
/// 3. the cutoff ⇒ SIGTERM, grace, SIGKILL, [`Exit::CutOff`].
///
/// Exit is detected WITHOUT reaping (`waitid(WNOWAIT)`): the zombie leader keeps its pid — the group id —
/// reserved while the rest of the group is SIGKILLed, so the signal cannot reach a recycled group.
/// Only then is the leader reaped. After the exit, stdout gets [`READER_JOIN_BUDGET`] to reach EOF; a
/// pipe still open then (a process outside the group kept it) means stdout is NOT complete.
fn supervise(
    argv: &[String],
    stdin: &[u8],
    cwd: &Path,
    cutoff: Instant,
    parent_env: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    mode: StdoutMode,
    journal: Option<&mut dyn FnMut(&Started) -> Result<(), String>>,
) -> RunReport {
    let started = Instant::now();
    let report = |exit, stdout, stdout_complete, stderr_tail| RunReport {
        exit,
        stdout,
        stdout_complete,
        stderr_tail,
        wall: started.elapsed(),
    };
    if Instant::now() >= cutoff {
        return report(Exit::NotStarted, Vec::new(), false, String::new());
    }
    let Some((program, args)) = argv.split_first() else {
        return report(Exit::SpawnFailed("empty argv".into()), Vec::new(), false, String::new());
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.env_clear();
    for (key, value) in parent_env {
        if !key.to_string_lossy().starts_with("MAXPLAYER_") {
            command.env(key, value);
        }
    }
    command.env(HOOK_VERSION_ENV, HOOK_CONTRACT_VERSION.to_string());
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return report(Exit::SpawnFailed(error.to_string()), Vec::new(), false, String::new()),
    };
    let pgid = child.id();
    active_groups().insert(pgid);
    if let Some(journal) = journal {
        // Nothing has been written to stdin yet, so a gated hook is still parked in the gate.
        let started = Started { pgid, start: process_identity(pgid) };
        if let Err(error) = journal(&started) {
            terminate_group(pgid, &mut child);
            return report(Exit::SpawnFailed(format!("journal: {error}")), Vec::new(), false, String::new());
        }
    }
    let mut stdin_pipe = child.stdin.take();
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    {
        use std::os::fd::AsRawFd;
        let fds = [
            stdin_pipe.as_ref().map(|p| p.as_raw_fd()),
            stdout_pipe.as_ref().map(|p| p.as_raw_fd()),
            stderr_pipe.as_ref().map(|p| p.as_raw_fd()),
        ];
        if let Some(error) = fds.into_iter().flatten().find_map(|fd| set_nonblocking(fd).err()) {
            terminate_group(pgid, &mut child);
            return report(Exit::SpawnFailed(format!("pipe setup failed: {error}")), Vec::new(), false, String::new());
        }
    }

    let mut io = Pumped::default();
    let exit = loop {
        let progressed = io.pump(&mut stdin_pipe, &mut stdout_pipe, &mut stderr_pipe, stdin, mode);
        if io.overflow {
            terminate_group(pgid, &mut child);
            break Exit::StdoutOverflow;
        }
        match leader_exited(pgid) {
            Ok(true) => {
                let late = Instant::now() >= cutoff;
                // The leader is a zombie, so its id (the group id) cannot have been reused yet.
                signal_group(pgid, SIGKILL);
                active_groups().remove(&pgid);
                let status = child.wait();
                if late {
                    break Exit::CutOff;
                }
                break match status {
                    Ok(status) => Exit::Exited { code: status.code() },
                    Err(error) => Exit::SpawnFailed(format!("wait failed: {error}")),
                };
            }
            Ok(false) => {}
            Err(error) => {
                terminate_group(pgid, &mut child);
                break Exit::SpawnFailed(format!("wait failed: {error}"));
            }
        }
        if Instant::now() >= cutoff {
            terminate_group(pgid, &mut child);
            break Exit::CutOff;
        }
        if !progressed {
            std::thread::sleep(POLL);
        }
    };
    // Stdin has nobody left to read it.
    drop(stdin_pipe.take());

    // The group is gone; give the read ends a bounded time to reach EOF.
    let drain_by = Instant::now() + READER_JOIN_BUDGET;
    while (stdout_pipe.is_some() || stderr_pipe.is_some()) && Instant::now() < drain_by && !io.overflow {
        if !io.pump(&mut stdin_pipe, &mut stdout_pipe, &mut stderr_pipe, stdin, mode) {
            std::thread::sleep(POLL);
        }
    }
    let stdout_complete = stdout_pipe.is_none() && !io.stdout_failed && !io.overflow;
    // Close every read end now: nothing outlives this call.
    drop(stdout_pipe);
    drop(stderr_pipe);
    // An overflow that raced the exit still counts.
    let exit = if io.overflow { Exit::StdoutOverflow } else { exit };
    let stderr_tail = String::from_utf8_lossy(&io.stderr).into_owned();
    report(exit, io.stdout, stdout_complete, stderr_tail)
}

/// What the supervisor has moved so far.
#[derive(Default)]
struct Pumped {
    written: usize,
    stdout: Vec<u8>,
    stdout_failed: bool,
    overflow: bool,
    stderr: Vec<u8>,
}

impl Pumped {
    /// One non-blocking pass over all three pipes. Returns whether any byte moved.
    fn pump(
        &mut self,
        stdin_pipe: &mut Option<std::process::ChildStdin>,
        stdout_pipe: &mut Option<std::process::ChildStdout>,
        stderr_pipe: &mut Option<std::process::ChildStderr>,
        payload: &[u8],
        mode: StdoutMode,
    ) -> bool {
        let mut progressed = pump_write(stdin_pipe, payload, &mut self.written);
        let (stdout, overflow) = (&mut self.stdout, &mut self.overflow);
        match pump_read(stdout_pipe, |chunk| {
            if mode == StdoutMode::Keep && !*overflow {
                if stdout.len() + chunk.len() > STDOUT_MAX_BYTES {
                    *overflow = true;
                } else {
                    stdout.extend_from_slice(chunk);
                }
            }
        }) {
            Ok(moved) => progressed |= moved,
            Err(()) => self.stdout_failed = true,
        }
        let stderr = &mut self.stderr;
        if let Ok(moved) = pump_read(stderr_pipe, |chunk| {
            stderr.extend_from_slice(chunk);
            let excess = stderr.len().saturating_sub(STDERR_TAIL_BYTES);
            if excess > 0 {
                stderr.drain(..excess);
            }
        }) {
            progressed |= moved;
        }
        progressed
    }
}

/// The most chunks one pump reads from one pipe before the loop checks the exit and the cutoff
/// again, so an endless writer cannot pin the supervisor inside a read loop.
const PUMP_CHUNKS: usize = 16;

/// Read what is available now. `Ok(progressed)`; EOF closes the pipe (sets it to `None`); a read
/// error closes it and is reported as `Err(())` (stdout is then not complete).
fn pump_read<R: Read>(pipe: &mut Option<R>, mut sink: impl FnMut(&[u8])) -> Result<bool, ()> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(false);
    };
    let mut chunk = [0u8; 8192];
    let mut progressed = false;
    for _ in 0..PUMP_CHUNKS {
        match reader.read(&mut chunk) {
            Ok(0) => {
                *pipe = None;
                return Ok(true);
            }
            Ok(n) => {
                sink(&chunk[..n]);
                progressed = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(progressed),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                *pipe = None;
                return Err(());
            }
        }
    }
    Ok(progressed)
}

/// Write what the pipe takes now; close stdin once everything is written. A broken pipe (the hook
/// exited or closed stdin early) just closes it: the exit status decides, not the write.
fn pump_write(pipe: &mut Option<std::process::ChildStdin>, payload: &[u8], written: &mut usize) -> bool {
    let Some(writer) = pipe.as_mut() else {
        return false;
    };
    let mut progressed = false;
    while *written < payload.len() {
        match writer.write(&payload[*written..]) {
            Ok(0) => {
                *pipe = None;
                return true;
            }
            Ok(n) => {
                *written += n;
                progressed = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return progressed,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                *pipe = None;
                return true;
            }
        }
    }
    *pipe = None;
    true
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor owned by a live Child pipe for the duration of the call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Whether child `pid` has exited, WITHOUT reaping it (`WNOWAIT`): until it is reaped its pid, and
/// so the group id, stays reserved.
fn leader_exited(pid: u32) -> std::io::Result<bool> {
    // SAFETY: an all-zero siginfo_t is a valid value; waitid only writes into it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid on our own child with a valid out-pointer.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // With WNOHANG and no state change the structure is left zeroed (POSIX: si_pid 0); a change
    // fills it in with SIGCHLD.
    Ok(info.si_signo != 0)
}

const SIGTERM: i32 = libc::SIGTERM;
const SIGKILL: i32 = libc::SIGKILL;

/// SIGTERM the group, wait out the grace, SIGKILL the group, then reap the leader. The leader is
/// observed without reaping throughout, so both signals address a group whose id is still ours.
fn terminate_group(pgid: u32, child: &mut std::process::Child) {
    signal_group(pgid, SIGTERM);
    let grace_until = Instant::now() + KILL_GRACE;
    while Instant::now() < grace_until {
        if !matches!(leader_exited(pgid), Ok(false)) {
            break;
        }
        std::thread::sleep(POLL);
    }
    signal_group(pgid, SIGKILL);
    active_groups().remove(&pgid);
    let _ = child.wait();
}

/// The hook groups this process is supervising right now. A group is removed only while its
/// leader is still an unreaped zombie, so [`terminate_all`] (which signals under this lock) can
/// never reach a recycled id.
fn active_groups() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<u32>> {
    static ACTIVE: std::sync::Mutex<std::collections::BTreeSet<u32>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Daemon shutdown (§5.1): SIGTERM every running hook group, wait out the grace, SIGKILL them.
/// Blocking; returns at once when nothing runs.
pub fn terminate_all() {
    {
        let active = active_groups();
        if active.is_empty() {
            return;
        }
        for pgid in active.iter() {
            signal_group(*pgid, SIGTERM);
        }
    }
    std::thread::sleep(KILL_GRACE);
    let active = active_groups();
    for pgid in active.iter() {
        signal_group(*pgid, SIGKILL);
    }
}

/// What `/proc` says about a pid right now (Linux).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcState {
    /// A process has this pid: its start time (`stat` field 22), its process group, and whether it
    /// is a zombie (exited, not yet reaped: it still holds its pid and keeps its group id reserved).
    Present { start: String, pgrp: u32, zombie: bool },
    /// No process has this pid.
    Absent,
    /// It could not be read or parsed: neither of the above may be assumed.
    Unreadable(String),
}

fn proc_state(pid: u32) -> ProcState {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => return ProcState::Absent,
        Err(error) => return ProcState::Unreadable(error.to_string()),
    };
    // `comm` (field 2) is parenthesised and may contain spaces: count from the LAST ')'.
    let Some(close) = stat.rfind(')') else {
        return ProcState::Unreadable("malformed stat".into());
    };
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // After `comm`: [0] state (field 3), [2] pgrp (field 5), [19] starttime (field 22).
    match (fields.first(), fields.get(2).and_then(|pgrp| pgrp.parse::<u32>().ok()), fields.get(19)) {
        (Some(state), Some(pgrp), Some(start)) => ProcState::Present {
            start: (*start).to_owned(),
            pgrp,
            zombie: matches!(*state, "Z" | "X" | "x"),
        },
        _ => ProcState::Unreadable("malformed stat".into()),
    }
}

/// This boot's id (Linux): start times count from boot, so they identify a process only within one.
fn boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// A process's identity as a later daemon can check it: `<boot id>/<start time>`, so neither a
/// recycled pid (another start time) nor a reboot (another boot id) can pass for it. Linux only;
/// `None` elsewhere, or when it cannot be read.
pub fn process_identity(pid: u32) -> Option<String> {
    let boot = boot_id()?;
    match proc_state(pid) {
        ProcState::Present { start, .. } => Some(format!("{boot}/{start}")),
        _ => None,
    }
}

/// What [`reap_stale_group`] established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StaleReap {
    /// Our old group was found and every live process in it was stopped (seen gone afterwards).
    Killed,
    /// Nothing of our old group is left: the host rebooted since, the leader's pid now names another
    /// process, or neither the leader nor any process of its group exists.
    Gone,
    /// It could not be established that the group is ours, or that it stopped. Nothing unverified
    /// was signalled. The caller keeps the journal entry and must neither re-run the invocation nor
    /// reuse its directory until a later attempt resolves it.
    Unresolved(String),
}

/// How long SIGKILLed members of a stale group get to disappear before the reap gives up.
const STALE_KILL_SETTLE: Duration = Duration::from_secs(2);

/// Stop a hook group a previous daemon journaled (§5.1), never signalling a process that is not
/// provably ours:
/// - another boot id ⇒ the reboot ended it: [`StaleReap::Gone`];
/// - the leader pid names a process with another start time ⇒ the pid was recycled, which the
///   kernel does only once no process of our group is left: [`StaleReap::Gone`];
/// - no leader and no process in its group ⇒ [`StaleReap::Gone`]; no leader but processes left in
///   the group ⇒ they can no longer be proven ours: [`StaleReap::Unresolved`], nothing signalled;
/// - the leader is ours ⇒ every live member is signalled ONE BY ONE through a pidfd that pins it
///   (no numeric group signal), SIGTERM, the grace, then SIGKILL until none is left.
///
/// Membership is accepted only while a process already proven ours still exists after the listing
/// (it keeps the group id from being reused), so a member listed in between belongs to OUR group.
/// Unreadable state, a host without `/proc`, or members that outlive SIGKILL are `Unresolved`.
pub fn reap_stale_group(pgid: u32, recorded: Option<&str>) -> StaleReap {
    use StaleReap::Unresolved;
    if !cfg!(target_os = "linux") {
        return Unresolved("this host has no /proc to verify a process group with".into());
    }
    let Some(recorded) = recorded else {
        return Unresolved("no process identity was recorded for it".into());
    };
    let Some((boot, start)) = recorded.split_once('/') else {
        return Unresolved(format!("unrecognised identity record {recorded:?}"));
    };
    let Some(boot_now) = boot_id() else {
        return Unresolved("the boot id is unreadable".into());
    };
    if boot_now != boot {
        return StaleReap::Gone;
    }
    match proc_state(pgid) {
        ProcState::Present { start: now, .. } if now != start => return StaleReap::Gone,
        ProcState::Present { .. } => {}
        ProcState::Unreadable(error) => return Unresolved(format!("its leader is unreadable: {error}")),
        ProcState::Absent => {
            return match live_group_members(pgid) {
                Ok(members) if members.is_empty() => StaleReap::Gone,
                Ok(members) => Unresolved(format!(
                    "its leader has exited but {} process(es) remain in group {pgid}; they cannot be proven to be ours, so none was signalled",
                    members.len()
                )),
                Err(error) => Unresolved(error),
            };
        }
    }
    let mut anchors = vec![(pgid, start.to_owned())];
    let term_until = Instant::now() + KILL_GRACE;
    let kill_until = term_until + STALE_KILL_SETTLE;
    let mut termed = false;
    loop {
        let killing = Instant::now() >= term_until;
        let signal = if killing {
            Some(SIGKILL)
        } else if termed {
            None
        } else {
            Some(SIGTERM)
        };
        termed = true;
        match sweep_verified_group(pgid, &mut anchors, signal) {
            Ok(Sweep::Empty) => return StaleReap::Killed,
            Ok(Sweep::Live) => {}
            Ok(Sweep::Unanchored(left)) => {
                return Unresolved(format!(
                    "{left} process(es) remain in group {pgid} but none of ours is left to prove they are ours"
                ));
            }
            Err(error) => return Unresolved(error),
        }
        if killing && Instant::now() >= kill_until {
            return Unresolved(format!("processes of group {pgid} survived SIGKILL"));
        }
        std::thread::sleep(POLL);
    }
}

/// One pass of [`reap_stale_group`].
enum Sweep {
    /// No live process is left in the group.
    Empty,
    /// Live members remain; all were proven ours (and signalled, if asked).
    Live,
    /// Live members remain but no process already proven ours still exists: nothing signalled.
    Unanchored(usize),
}

/// List the live members of `pgid`, then accept them only if one of `anchors` (processes already
/// proven ours, the leader first) still exists in the group afterwards. Accepted members become
/// anchors and, with `signal`, are each signalled through a pinned pidfd.
fn sweep_verified_group(pgid: u32, anchors: &mut Vec<(u32, String)>, signal: Option<i32>) -> Result<Sweep, String> {
    let members = live_group_members(pgid)?;
    let mut still = Vec::with_capacity(anchors.len());
    for (pid, start) in anchors.drain(..) {
        match proc_state(pid) {
            ProcState::Present { start: now, pgrp, .. } if now == start && (pgrp == pgid || pid == pgid) => {
                still.push((pid, start));
            }
            ProcState::Unreadable(error) => return Err(format!("process {pid}: {error}")),
            _ => {}
        }
    }
    *anchors = still;
    if members.is_empty() {
        return Ok(Sweep::Empty);
    }
    if anchors.is_empty() {
        return Ok(Sweep::Unanchored(members.len()));
    }
    for (pid, start) in &members {
        if !anchors.iter().any(|(p, s)| p == pid && s == start) {
            anchors.push((*pid, start.clone()));
        }
        if let Some(sig) = signal {
            signal_pinned(*pid, start, pgid, sig)?;
        }
    }
    Ok(Sweep::Live)
}

/// The live (non-zombie) processes in group `pgid`, as `(pid, start)`. An unreadable process of
/// this user might be a member, so it is an error; other users' processes are skipped.
fn live_group_members(pgid: u32) -> Result<Vec<(u32, String)>, String> {
    let mut members = Vec::new();
    let entries = std::fs::read_dir("/proc").map_err(|e| format!("/proc: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("/proc: {e}"))?;
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        match proc_state(pid) {
            ProcState::Present { start, pgrp, zombie } => {
                if pgrp == pgid && !zombie {
                    members.push((pid, start));
                }
            }
            ProcState::Absent => {}
            ProcState::Unreadable(error) => {
                use std::os::unix::fs::MetadataExt;
                // SAFETY: geteuid has no preconditions and cannot fail.
                let me = unsafe { libc::geteuid() };
                if entry.metadata().map(|m| m.uid() == me).unwrap_or(true) {
                    return Err(format!("process {pid}: {error}"));
                }
            }
        }
    }
    Ok(members)
}

/// Send `sig` to `pid` only while it is still the process `(pid, start)` in group `pgid`: the pid
/// is pinned with a pidfd FIRST and checked after, so the signal cannot reach a process that took
/// the pid over in between. `Ok(false)`: it was no longer there.
#[cfg(target_os = "linux")]
fn signal_pinned(pid: u32, start: &str, pgid: u32, sig: i32) -> Result<bool, String> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
        return Ok(false);
    };
    // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, 0) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) { Ok(false) } else { Err(format!("pidfd_open: {error}")) };
    }
    // SAFETY: the descriptor was just returned to us; OwnedFd closes it exactly once.
    let fd = unsafe { OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) };
    match proc_state(pid) {
        ProcState::Present { start: now, pgrp, .. } if now == start && pgrp == pgid => {}
        ProcState::Unreadable(error) => return Err(format!("process {pid}: {error}")),
        _ => return Ok(false),
    }
    // SAFETY: a valid pidfd, a signal number, no siginfo, no flags.
    let rc = unsafe {
        libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0)
    };
    if rc == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) { Ok(false) } else { Err(format!("pidfd_send_signal: {error}")) }
}

#[cfg(not(target_os = "linux"))]
fn signal_pinned(_pid: u32, _start: &str, _pgid: u32, _sig: i32) -> Result<bool, String> {
    Err("no pidfd on this host".into())
}

/// `kill(-pgid, sig)`, refusing the widening ids 0 and 1. Errors (ESRCH: the group is gone) are
/// ignored: there is nothing left to stop.
fn signal_group(pgid: u32, sig: i32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    if pgid <= 1 {
        return;
    }
    // SAFETY: kill(2) takes two ints and touches no memory of ours.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn run_sh(script: &str, stdin: &[u8], budget: Duration) -> RunReport {
        let dir = tempfile::tempdir().expect("tmp");
        run(&sh(script), stdin, dir.path(), Instant::now() + budget)
    }

    const CAP: usize = 4000;

    #[test]
    fn a_done_answer_is_delivered_byte_for_byte() {
        let report = run_sh(
            r#"cat >/dev/null; printf '  {"status":"done","answer":"héllo\\nworld","extra":1}\n'"#,
            b"{}\n",
            Duration::from_secs(10),
        );
        assert_eq!(
            classify_job(&report, CAP),
            JobOutcome::Answer(HookAnswer { status: HookStatus::Done, answer: "héllo\nworld".into() })
        );
    }

    #[test]
    fn each_status_parses_and_an_unknown_one_breaks_the_contract() {
        for status in ["done", "needs_input", "out_of_time"] {
            let report = run_sh(
                &format!(r#"printf '{{"status":"{status}","answer":"a"}}'"#),
                b"",
                Duration::from_secs(10),
            );
            match classify_job(&report, CAP) {
                JobOutcome::Answer(answer) => assert_eq!(answer.status.as_str(), status),
                other => panic!("{status}: {other:?}"),
            }
        }
        let report = run_sh(r#"printf '{"status":"maybe","answer":"a"}'"#, b"", Duration::from_secs(10));
        assert!(matches!(classify_job(&report, CAP), JobOutcome::Broken(_)));
    }

    #[test]
    fn malformed_stdout_breaks_the_contract_and_strikes() {
        for script in [
            r#"echo hello"#,
            r#"printf '{"status":"done","answer":"  \n"}'"#,
            r#"printf '{"status":"done","answer":"a","answer":"b"}'"#,
            r#"printf '{"status":"done","answer":"a"} trailing'"#,
            r#"printf '{"status":"done"}'"#,
            r#"printf '\377\376'"#,
            // Not an object: a positional array.
            r#"printf '["done","a"]'"#,
            // A duplicate UNKNOWN key is a duplicate too.
            r#"printf '{"status":"done","answer":"a","x":1,"x":2}'"#,
            // A present known field must have its type; null is not absent.
            r#"printf '{"status":"done","answer":null}'"#,
            r#"printf '{"status":"done","answer":7}'"#,
            r#"printf '{"status":null,"answer":"a"}'"#,
            r#"printf '{"status":"cannot_deliver","reason":null}'"#,
        ] {
            let outcome = classify_job(&run_sh(script, b"", Duration::from_secs(10)), CAP);
            assert!(matches!(outcome, JobOutcome::Broken(_)), "{script}: {outcome:?}");
            assert!(outcome.strikes(), "{script}");
        }
    }

    #[test]
    fn cannot_deliver_is_an_honest_refusal_and_never_a_strike() {
        let report = run_sh(
            r#"printf '{"status":"cannot_deliver","reason":"needs files"}'"#,
            b"",
            Duration::from_secs(10),
        );
        let outcome = classify_job(&report, CAP);
        assert_eq!(outcome, JobOutcome::CannotDeliver("needs files".into()));
        assert!(!outcome.strikes());
    }

    #[test]
    fn a_non_zero_exit_breaks_and_keeps_only_the_stderr_tail() {
        let report = run_sh(
            r#"head -c 10000 /dev/zero | tr '\0' x >&2; echo TAILMARK >&2; exit 3"#,
            b"",
            Duration::from_secs(10),
        );
        assert_eq!(report.exit, Exit::Exited { code: Some(3) });
        assert!(report.stderr_tail.len() <= STDERR_TAIL_BYTES);
        assert!(report.stderr_tail.contains("TAILMARK"));
        assert!(classify_job(&report, CAP).strikes());
    }

    #[test]
    fn an_over_cap_answer_is_too_large_never_truncated_and_never_a_strike() {
        let exact = "é".repeat(CAP / 2); // 2 bytes each: exactly CAP bytes
        let report = run_sh(
            &format!(r#"printf '{{"status":"done","answer":"{exact}"}}'"#),
            b"",
            Duration::from_secs(10),
        );
        assert!(matches!(classify_job(&report, CAP), JobOutcome::Answer(a) if a.answer == exact));
        let over = format!("{exact}x");
        let report = run_sh(
            &format!(r#"printf '{{"status":"done","answer":"{over}"}}'"#),
            b"",
            Duration::from_secs(10),
        );
        let outcome = classify_job(&report, CAP);
        assert_eq!(outcome, JobOutcome::TooLarge { bytes: CAP + 1, cap: CAP });
        assert!(!outcome.strikes());
    }

    #[test]
    fn stdout_over_the_ceiling_kills_the_hook() {
        let report = run_sh(r#"cat /dev/zero"#, b"", Duration::from_secs(20));
        assert_eq!(report.exit, Exit::StdoutOverflow);
        assert!(report.stdout.len() <= STDOUT_MAX_BYTES);
        assert!(classify_job(&report, CAP).strikes());
    }

    #[test]
    fn a_hook_that_never_reads_stdin_and_floods_stderr_does_not_wedge() {
        let big = vec![b'x'; 4 * 1024 * 1024];
        let started = Instant::now();
        let report = run_sh(
            r#"head -c 3000000 /dev/zero >&2; printf '{"status":"done","answer":"ok"}'"#,
            &big,
            Duration::from_secs(20),
        );
        assert!(started.elapsed() < Duration::from_secs(15));
        assert!(matches!(classify_job(&report, CAP), JobOutcome::Answer(_)), "{:?}", report.exit);
    }

    #[test]
    fn the_cutoff_kills_the_whole_group_including_a_grandchild_and_is_no_strike() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("grandchild-alive");
        let script = format!(
            "(sleep 3; touch {}) & sleep 30",
            marker.display()
        );
        let started = Instant::now();
        let report = run(&sh(&script), b"", dir.path(), Instant::now() + Duration::from_millis(300));
        assert_eq!(report.exit, Exit::CutOff);
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "the grandchild must die with the group");
        let outcome = classify_job(&report, CAP);
        assert_eq!(outcome, JobOutcome::OutOfTime { started: true });
        assert!(!outcome.strikes());
    }

    #[test]
    fn a_passed_cutoff_never_spawns() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("ran");
        let report = run(
            &sh(&format!("touch {}", marker.display())),
            b"",
            dir.path(),
            Instant::now() - Duration::from_secs(1),
        );
        assert_eq!(report.exit, Exit::NotStarted);
        assert!(!marker.exists());
        assert_eq!(classify_job(&report, CAP), JobOutcome::OutOfTime { started: false });
    }

    #[test]
    fn a_worker_that_left_the_group_holding_stdout_neither_holds_the_runner_nor_passes_a_prefix() {
        let started = Instant::now();
        // The worker leaves the group (setsid) and keeps the pipe open. The runner must return,
        // and must NOT accept what it read so far: more bytes could still arrive on that pipe.
        let report = run_sh(
            // The pause lets the worker reach setsid() before the post-exit group kill.
            r#"setsid sh -c 'sleep 20' & sleep 0.5; printf '{"status":"done","answer":"ok"}'"#,
            b"",
            Duration::from_secs(20),
        );
        assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
        assert_eq!(report.exit, Exit::Exited { code: Some(0) });
        assert!(!report.stdout_complete);
        let outcome = classify_job(&report, CAP);
        assert!(matches!(&outcome, JobOutcome::Broken(why) if why.contains("not complete")), "{outcome:?}");
        assert!(outcome.strikes());
    }

    #[test]
    fn a_worker_that_left_the_group_with_its_own_stdout_is_fine() {
        let report = run_sh(
            r#"setsid sh -c 'sleep 20' >/dev/null 2>&1 </dev/null & sleep 0.5; printf '{"status":"done","answer":"ok"}'"#,
            b"",
            Duration::from_secs(20),
        );
        assert!(report.stdout_complete);
        assert!(matches!(classify_job(&report, CAP), JobOutcome::Answer(_)), "{report:?}");
    }

    #[test]
    fn the_group_is_killed_when_the_hook_exits_and_the_answer_still_counts() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("grandchild-alive");
        // The grandchild stays in the group and inherits stdout; the post-exit group kill must
        // stop it (and so close the pipe) before it can write.
        let script = format!(
            r#"(sleep 2; touch {}) & printf '{{"status":"done","answer":"ok"}}'"#,
            marker.display()
        );
        let report = run(&sh(&script), b"", dir.path(), Instant::now() + Duration::from_secs(20));
        assert!(report.stdout_complete);
        assert!(matches!(classify_job(&report, CAP), JobOutcome::Answer(_)), "{report:?}");
        std::thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "the grandchild must die with the group");
    }

    #[test]
    fn stdin_carries_the_event_and_the_env_is_scrubbed() {
        // The parent env is injected (never the process env: tests share it).
        let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
            .filter(|(k, _)| !k.to_string_lossy().starts_with("MAXPLAYER_"))
            .collect();
        env.push(("MAXPLAYER_SECRETISH_TEST".into(), "leak".into()));
        env.push(("MAXPLAYER_HOME".into(), "/home/seller/.maxplayer".into()));
        let event = job_event("inv1", 1, 4076, "job1", 2, true, "the task, verbatim \"quoted\"", None, None, 99);
        let dir = tempfile::tempdir().expect("tmp");
        let report = run_with_env(&sh(
            r#"in=$(cat); env | grep '^MAXPLAYER_' | sort > /dev/stderr; printf '%s' "$in" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps({"status":"done","answer":d["prompt"]+"|"+d["invocation"]+"|"+str(d["turn"])+"|"+str(d["output"])+"|"+str(d["inputs_dir"])}))'"#,
), &event, dir.path(), Instant::now() + Duration::from_secs(10), env);
        assert_eq!(report.stderr_tail.trim(), "MAXPLAYER_HOOK_V=1");
        match classify_job(&report, CAP) {
            JobOutcome::Answer(answer) => {
                assert_eq!(answer.answer, "the task, verbatim \"quoted\"|inv1|2|None|None")
            }
            other => panic!("{other:?} stderr={}", report.stderr_tail),
        }
    }

    #[test]
    fn the_job_cutoff_leaves_the_publish_margin() {
        let now = Instant::now();
        assert_eq!(job_cutoff(1_000, 900, now), now + Duration::from_secs(70));
        assert_eq!(job_cutoff(1_000, 980, now), now, "inside the margin: no time left");
    }

    #[test]
    fn ping_is_healthy_only_on_exit_zero() {
        assert!(ping_healthy(&run_sh("cat >/dev/null; exit 0", &ping_event(PingReason::Boot), Duration::from_secs(10))));
        assert!(!ping_healthy(&run_sh("exit 1", &ping_event(PingReason::Boot), Duration::from_secs(10))));
        assert!(!ping_healthy(&run_sh("sleep 5", &ping_event(PingReason::Boot), Duration::from_millis(200))));
    }

    #[test]
    fn a_gated_job_runs_only_after_its_group_is_journaled_and_sees_the_event_intact() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut journaled = Vec::new();
        let report = run_gated(
            &sh(r#"in=$(cat); printf '{"status":"done","answer":"%s"}' "$in""#),
            b"EVENT",
            dir.path(),
            Instant::now() + Duration::from_secs(10),
            &mut |started| {
                journaled.push(started.clone());
                Ok(())
            },
        );
        assert_eq!(journaled.len(), 1);
        assert!(journaled[0].pgid > 1);
        if cfg!(target_os = "linux") {
            assert!(journaled[0].start.is_some(), "the leader's start time is recorded on Linux");
        }
        // The gate consumed exactly its `go` line: the hook read the event and nothing else.
        match classify_job(&report, CAP) {
            JobOutcome::Answer(answer) => assert_eq!(answer.answer, "EVENT"),
            other => panic!("{other:?} stderr={}", report.stderr_tail),
        }
    }

    #[test]
    fn a_failed_journal_means_the_hook_never_ran() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("ran");
        let report = run_gated(
            &sh(&format!("touch {}", marker.display())),
            b"{}",
            dir.path(),
            Instant::now() + Duration::from_secs(10),
            &mut |_| Err("disk full".into()),
        );
        assert!(matches!(&report.exit, Exit::SpawnFailed(why) if why.contains("journal")), "{:?}", report.exit);
        std::thread::sleep(Duration::from_millis(300));
        assert!(!marker.exists(), "the gate must never release a hook whose group was not journaled");
    }

    #[test]
    fn a_gate_whose_daemon_died_before_go_exits_without_running_the_hook() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("ran");
        let mut child = Command::new("/bin/sh")
            .args(["-c", GATE_SCRIPT, "maxplayer-hook-gate", "touch"])
            .arg(&marker)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn gate");
        drop(child.stdin.take()); // the daemon is gone: stdin closes with no `go`
        let status = child.wait().expect("wait");
        assert_eq!(status.code(), Some(125));
        assert!(!marker.exists());
    }

    #[cfg(target_os = "linux")]
    fn spawn_group(script: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        Command::new("sh").args(["-c", script]).process_group(0).spawn().expect("spawn")
    }

    #[cfg(target_os = "linux")]
    fn group_alive(pgid: u32) -> bool {
        !live_group_members(pgid).expect("scan").is_empty()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stale_group_is_killed_only_when_its_leader_is_verified() {
        let boot = boot_id().expect("boot id");
        // A recycled pid (same boot, another start time): never signalled.
        let mut other = spawn_group("sleep 30");
        assert_eq!(reap_stale_group(other.id(), Some(&format!("{boot}/1"))), StaleReap::Gone);
        assert!(other.try_wait().expect("try_wait").is_none(), "an unverified group must be left alone");
        // Another boot: whatever runs under that pid now is not ours.
        let start = process_identity(other.id()).expect("identity");
        let (_, ticks) = start.split_once('/').expect("boot/start");
        assert_eq!(reap_stale_group(other.id(), Some(&format!("not-this-boot/{ticks}"))), StaleReap::Gone);
        assert!(other.try_wait().expect("try_wait").is_none(), "a previous boot's record must not reach this boot's process");
        let _ = other.kill();
        let _ = other.wait();
        // No recorded identity: unresolved, left alone.
        assert!(matches!(reap_stale_group(1, None), StaleReap::Unresolved(_)));
        // Our leader (identity matches): the whole group dies, a member that ignores SIGTERM included.
        let mut ours = spawn_group("(trap '' TERM; sleep 30) & sleep 30");
        let pgid = ours.id();
        let identity = process_identity(pgid).expect("identity");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(reap_stale_group(pgid, Some(&identity)), StaleReap::Killed);
        let status = ours.wait().expect("wait");
        assert!(status.code().is_none(), "killed by a signal: {status:?}");
        assert!(!group_alive(pgid), "Killed means nothing live is left in the group");
    }

    /// codex r2 #1: once the leader is gone, a process still carrying the group id can no longer be
    /// proven ours (the id could have been reused), so nothing is signalled and the reap is
    /// unresolved; once the group is empty it is gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stale_group_without_its_leader_is_never_signalled() {
        let mut leader = spawn_group("sleep 30 & exit 0");
        let pgid = leader.id();
        let identity = process_identity(pgid).expect("identity");
        let _ = leader.wait(); // the leader exits and is reaped; its sleep stays in the group
        assert!(group_alive(pgid), "the member is still running");
        assert!(matches!(reap_stale_group(pgid, Some(&identity)), StaleReap::Unresolved(_)));
        std::thread::sleep(Duration::from_millis(100));
        assert!(group_alive(pgid), "an unprovable member must not be signalled");
        signal_group(pgid, SIGKILL);
        let gone_by = Instant::now() + Duration::from_secs(5);
        while group_alive(pgid) && Instant::now() < gone_by {
            std::thread::sleep(POLL);
        }
        assert_eq!(reap_stale_group(pgid, Some(&identity)), StaleReap::Gone);
    }

    #[test]
    fn a_noisy_ping_is_still_healthy_because_control_stdout_is_discarded() {
        let dir = tempfile::tempdir().expect("tmp");
        let noisy = sh("head -c 3000000 /dev/zero; exit 0");
        let report = run_control(&noisy, &ping_event(PingReason::Interval), dir.path(), Instant::now() + Duration::from_secs(20));
        assert_eq!(report.exit, Exit::Exited { code: Some(0) });
        assert!(report.stdout.is_empty(), "control stdout is never kept");
        assert!(ping(&noisy, dir.path(), PingReason::Interval).is_ok());
    }
}
