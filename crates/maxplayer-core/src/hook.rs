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
/// The `MAXPLAYER_*` variable every hook call sees.
pub const HOOK_VERSION_ENV: &str = "MAXPLAYER_HOOK_V";
/// The tag a `job` call carries in its environment: `<invocation>/<attempt>` ([`run_tag`]). Every
/// process the hook starts inherits it, so a later daemon can recognise the call's leftover
/// processes after a crash, even once the leader is gone (§5.1).
pub const HOOK_RUN_ENV: &str = "MAXPLAYER_HOOK_RUN";

/// The [`HOOK_RUN_ENV`] value of one `job` call.
pub fn run_tag(invocation: &str, attempt: u32) -> String {
    format!("{invocation}/{attempt}")
}

/// Whether this host can inspect processes well enough to verify a stale hook group (§5.1).
const STALE_REAP_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));
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
    /// Daemon shutdown interrupted it: no verdict, no strike, the job is NOT failed. Its journal
    /// stays, and the next start re-drives it (§5.1).
    Interrupted,
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
    /// Daemon shutdown stopped it, or latched before it was released or registered: no verdict on
    /// the hook. Never `Exited { code: Some(0) }`: an answer finished before the stop still counts.
    Shutdown,
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
        Exit::Shutdown => return JobOutcome::Interrupted,
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
            Exit::Shutdown => "ping interrupted by daemon shutdown".into(),
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
    supervise(&SUPERVISION, argv, stdin, cwd, cutoff, std::env::vars_os(), StdoutMode::Keep, None, None)
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
/// `tag` ([`run_tag`]) is set as [`HOOK_RUN_ENV`] in the hook's environment.
pub fn run_gated(
    argv: &[String],
    stdin: &[u8],
    cwd: &Path,
    cutoff: Instant,
    tag: &str,
    journal: &mut dyn FnMut(&Started) -> Result<(), String>,
) -> RunReport {
    run_gated_in(&SUPERVISION, argv, stdin, cwd, cutoff, tag, journal)
}

fn run_gated_in(
    supervision: &Supervision,
    argv: &[String],
    stdin: &[u8],
    cwd: &Path,
    cutoff: Instant,
    tag: &str,
    journal: &mut dyn FnMut(&Started) -> Result<(), String>,
) -> RunReport {
    if argv.is_empty() {
        return supervise(supervision, argv, stdin, cwd, cutoff, std::env::vars_os(), StdoutMode::Keep, Some(tag), None);
    }
    let mut gated: Vec<String> =
        vec!["/bin/sh".into(), "-c".into(), GATE_SCRIPT.into(), "maxplayer-hook-gate".into()];
    gated.extend(argv.iter().cloned());
    // `go` is written by the supervisor itself, under the shutdown lock, once the group is journaled.
    supervise(supervision, &gated, stdin, cwd, cutoff, std::env::vars_os(), StdoutMode::Keep, Some(tag), Some(journal))
}

/// [`run`] for a control event: the same supervision, with stdout discarded (§5.2: ignored).
pub fn run_control(argv: &[String], stdin: &[u8], cwd: &Path, cutoff: Instant) -> RunReport {
    supervise(&SUPERVISION, argv, stdin, cwd, cutoff, std::env::vars_os(), StdoutMode::Discard, None, None)
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
    supervise(&SUPERVISION, argv, stdin, cwd, cutoff, parent_env, StdoutMode::Keep, None, None)
}

/// The one supervisor. Single-threaded on purpose: all three pipes are non-blocking and pumped
/// from this loop, so nothing can be left blocked on a pipe after it returns — every pipe is
/// closed (dropped) before the report is built.
///
/// A gated call (`journal` set) releases its hook only after the journal returned `Ok`, only before
/// the cutoff, and only under the [`Supervision`] lock while shutdown has not latched.
///
/// Order of resolution, decided once, at the point it is observed:
/// 1. stdout overflow (Keep mode) ⇒ kill the group; [`Exit::StdoutOverflow`], unless first seen at
///    or after the cutoff: then [`Exit::CutOff`] (the deadline wins over a late violation);
/// 2. the leader has exited ⇒ if that is first observed at or after the cutoff, [`Exit::CutOff`]
///    (the deadline wins over a late exit); a non-zero end while shutdown has latched is
///    [`Exit::Shutdown`]; else its status;
/// 3. the cutoff ⇒ SIGTERM, grace, SIGKILL, [`Exit::CutOff`].
///
/// An overflow seen while draining after the exit changes only an ordinary exit status: a resolved
/// cutoff or shutdown stands.
///
/// Exit is detected WITHOUT reaping (`waitid(WNOWAIT)`): the zombie leader keeps its pid — the group id —
/// reserved while the rest of the group is SIGKILLed, so the signal cannot reach a recycled group.
/// Only then is the leader reaped. After the exit, stdout gets [`READER_JOIN_BUDGET`] to reach EOF; a
/// pipe still open then (a process outside the group kept it) means stdout is NOT complete.
#[allow(clippy::too_many_arguments)]
fn supervise(
    supervision: &Supervision,
    argv: &[String],
    stdin: &[u8],
    cwd: &Path,
    cutoff: Instant,
    parent_env: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    mode: StdoutMode,
    tag: Option<&str>,
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
    if let Some(tag) = tag {
        command.env(HOOK_RUN_ENV, tag);
    }
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return report(Exit::SpawnFailed(error.to_string()), Vec::new(), false, String::new()),
    };
    let pgid = child.id();
    {
        // Registered under the lock shutdown latches under: after the latch nothing is registered
        // (so nothing escapes its signals), and this launch stops here.
        let mut state = supervision.lock();
        if state.shutting_down {
            drop(state);
            terminate_group(supervision, pgid, &mut child);
            return report(Exit::Shutdown, Vec::new(), false, String::new());
        }
        state.groups.insert(pgid);
    }
    let gated = journal.is_some();
    if let Some(journal) = journal {
        // Nothing has been written to stdin yet, so a gated hook is still parked in the gate.
        let started = Started { pgid, start: process_identity(pgid) };
        if let Err(error) = journal(&started) {
            terminate_group(supervision, pgid, &mut child);
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
            terminate_group(supervision, pgid, &mut child);
            return report(Exit::SpawnFailed(format!("pipe setup failed: {error}")), Vec::new(), false, String::new());
        }
    }
    if gated {
        // The journal may have blocked: a hook released now would start with its window closed.
        if Instant::now() >= cutoff {
            terminate_group(supervision, pgid, &mut child);
            return report(Exit::NotStarted, Vec::new(), false, String::new());
        }
        let state = supervision.lock();
        if state.shutting_down {
            drop(state);
            terminate_group(supervision, pgid, &mut child);
            return report(Exit::Shutdown, Vec::new(), false, String::new());
        }
        // Released under the lock: shutdown either latched first (then never released) or comes
        // after and signals this registered group. `go` is 3 bytes into an empty pipe: one write.
        let released = stdin_pipe.as_mut().map(|pipe| pipe.write(GATE_GO));
        drop(state);
        if !matches!(released, Some(Ok(n)) if n == GATE_GO.len()) {
            terminate_group(supervision, pgid, &mut child);
            return report(Exit::SpawnFailed("gate release failed".into()), Vec::new(), false, String::new());
        }
    }

    let mut io = Pumped::default();
    let exit = loop {
        let progressed = io.pump(&mut stdin_pipe, &mut stdout_pipe, &mut stderr_pipe, stdin, mode);
        if io.overflow {
            let late = Instant::now() >= cutoff;
            terminate_group(supervision, pgid, &mut child);
            break if late { Exit::CutOff } else { Exit::StdoutOverflow };
        }
        match leader_exited(pgid) {
            Ok(true) => {
                let late = Instant::now() >= cutoff;
                // The leader is a zombie, so its id (the group id) cannot have been reused yet.
                signal_group(pgid, SIGKILL);
                let shutting_down = {
                    let mut state = supervision.lock();
                    state.groups.remove(&pgid);
                    state.shutting_down
                };
                let status = child.wait();
                if late {
                    break Exit::CutOff;
                }
                break match status {
                    Ok(status) if shutting_down && status.code() != Some(0) => Exit::Shutdown,
                    Ok(status) => Exit::Exited { code: status.code() },
                    Err(error) => Exit::SpawnFailed(format!("wait failed: {error}")),
                };
            }
            Ok(false) => {}
            Err(error) => {
                terminate_group(supervision, pgid, &mut child);
                break Exit::SpawnFailed(format!("wait failed: {error}"));
            }
        }
        if Instant::now() >= cutoff {
            terminate_group(supervision, pgid, &mut child);
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
    // An overflow that raced an ordinary exit still counts; a resolved cutoff or shutdown stands.
    let exit = if io.overflow && matches!(exit, Exit::Exited { .. }) { Exit::StdoutOverflow } else { exit };
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
/// The grace belongs to the whole group: it ends early only once the leader has exited AND (on
/// Linux, where members can be listed) no other live process is left in the group.
fn terminate_group(supervision: &Supervision, pgid: u32, child: &mut std::process::Child) {
    signal_group(pgid, SIGTERM);
    let grace_until = Instant::now() + KILL_GRACE;
    while Instant::now() < grace_until {
        if !matches!(leader_exited(pgid), Ok(false)) && !group_has_live_members(pgid) {
            break;
        }
        std::thread::sleep(POLL);
    }
    signal_group(pgid, SIGKILL);
    supervision.lock().groups.remove(&pgid);
    let _ = child.wait();
}

/// Whether a live (non-zombie) process is still in `pgid`. Linux and macOS; elsewhere `false`.
fn group_has_live_members(pgid: u32) -> bool {
    STALE_REAP_SUPPORTED && live_group_members(pgid).map(|members| !members.is_empty()).unwrap_or(false)
}

/// The hook groups this process supervises, and whether daemon shutdown has latched (§5.1). One
/// lock covers registration, gate release and the latch, so a launch is either registered and
/// released before shutdown (and then signalled by it) or stopped without its hook ever running.
/// A group leaves the set only while its leader is still an unreaped zombie, so a signal sent
/// under this lock can never reach a recycled id.
struct Supervision(std::sync::Mutex<SupervisionState>);

struct SupervisionState {
    groups: std::collections::BTreeSet<u32>,
    shutting_down: bool,
}

impl Supervision {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(SupervisionState { groups: std::collections::BTreeSet::new(), shutting_down: false }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SupervisionState> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

static SUPERVISION: Supervision = Supervision::new();

/// How long shutdown waits, after SIGKILL, for the supervisors to reap their groups.
const SHUTDOWN_SETTLE: Duration = Duration::from_secs(2);

/// Daemon shutdown (§5.1): latch shutdown (nothing is registered or released after this), SIGTERM
/// every running group, wait out the grace (ending early once every supervisor has reaped its
/// group), SIGKILL what is left, then wait (bounded) for the supervisors to finish their teardown.
/// Blocking; returns at once when nothing runs.
pub fn terminate_all() {
    terminate_all_in(&SUPERVISION);
}

fn terminate_all_in(supervision: &Supervision) {
    {
        let mut state = supervision.lock();
        state.shutting_down = true;
        for pgid in &state.groups {
            signal_group(*pgid, SIGTERM);
        }
    }
    let settled = |until: Instant| loop {
        if supervision.lock().groups.is_empty() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(POLL);
    };
    if settled(Instant::now() + KILL_GRACE) {
        return;
    }
    {
        let state = supervision.lock();
        for pgid in &state.groups {
            signal_group(*pgid, SIGKILL);
        }
    }
    settled(Instant::now() + SHUTDOWN_SETTLE);
}

/// What the OS says about a pid right now (Linux: `/proc`; macOS: `proc_pidinfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcState {
    /// A process has this pid: its start time (Linux: `stat` field 22; macOS: `sec.usec`), its
    /// process group, and whether it is a zombie (exited, not yet reaped: it still holds its pid and
    /// keeps its group id reserved).
    Present { start: String, pgrp: u32, zombie: bool },
    /// No process has this pid.
    Absent,
    /// It could not be read or parsed: neither of the above may be assumed.
    Unreadable(String),
}

/// A process's identity as a later daemon can check it: `<boot id>/<start time>`, so neither a
/// recycled pid (another start time) nor a reboot (another boot id) can pass for it. Linux and
/// macOS; `None` elsewhere, or when it cannot be read.
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
/// provably ours. `recorded` is the leader's [`process_identity`]; `tag` the call's [`run_tag`].
/// - another boot id ⇒ the reboot ended it: [`StaleReap::Gone`];
/// - the leader pid names a process with another start time ⇒ the pid was recycled, which the
///   kernel does only once no process of our group is left: [`StaleReap::Gone`];
/// - the leader is ours (start time matches) ⇒ the group is ours;
/// - otherwise (leader gone, or no identity recorded) a process in the group that carries `tag`
///   in its environment ([`HOOK_RUN_ENV`]) proves the group ours; with no process left in the
///   group it is [`StaleReap::Gone`], and with processes left but none tagged (a hook that cleared
///   its environment, or a run from before the tag) it is [`StaleReap::Unresolved`]: nothing is
///   signalled and the job waits.
///
/// A group proven ours has every live member signalled one by one (pinned by a pidfd on Linux; no
/// numeric group signal): SIGTERM, the grace, then SIGKILL until none is left. Membership is
/// accepted only while a process already proven ours still exists after the listing (it keeps the
/// group id from being reused), so a member listed in between belongs to OUR group. Unreadable
/// state, an unsupported host, or members that outlive SIGKILL are `Unresolved`.
pub fn reap_stale_group(pgid: u32, recorded: Option<&str>, tag: Option<&str>) -> StaleReap {
    use StaleReap::Unresolved;
    if !STALE_REAP_SUPPORTED {
        return Unresolved("this host has no process table to verify a process group with".into());
    }
    let mut anchors = Vec::new();
    match recorded {
        Some(recorded) => {
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
                ProcState::Present { .. } => anchors.push((pgid, start.to_owned())),
                ProcState::Unreadable(error) => return Unresolved(format!("its leader is unreadable: {error}")),
                ProcState::Absent => {}
            }
        }
        None if tag.is_none() => return Unresolved("no process identity was recorded for it".into()),
        None => {}
    }
    reap_with(pgid, anchors, tag, &live_group_members)
}

/// [`reap_stale_group`] once the leader check is done, listing members with `list` (a test seam).
fn reap_with(
    pgid: u32,
    mut anchors: Vec<(u32, String)>,
    tag: Option<&str>,
    list: &dyn Fn(u32) -> Result<Vec<(u32, String)>, String>,
) -> StaleReap {
    use StaleReap::Unresolved;
    let term_until = Instant::now() + KILL_GRACE;
    let kill_until = term_until + STALE_KILL_SETTLE;
    // Members already sent SIGTERM: one that joins the group during the grace gets its own.
    let mut termed = Vec::new();
    let mut signalled = false;
    // The previous sweep listed no live member while the group still existed.
    let mut listed_empty = false;
    loop {
        let killing = Instant::now() >= term_until;
        let signal = if killing { SIGKILL } else { SIGTERM };
        match sweep_verified_group(pgid, &mut anchors, tag, signal, &mut termed, list) {
            // An empty LISTING is not an empty group: a member forked while the list was taken, by
            // a process that then exited, is in neither (codex #1403 H3). The group is over when the
            // kernel has no process with its id (`kill(-pgid, 0)`, which delivers nothing), or when
            // a second listing is empty too (only zombies are left, and they run nothing).
            Ok(Sweep::Empty) if listed_empty || !group_exists(pgid) => {
                return if signalled { StaleReap::Killed } else { StaleReap::Gone };
            }
            Ok(Sweep::Empty) => listed_empty = true,
            Ok(Sweep::Live) => {
                listed_empty = false;
                signalled = true;
            }
            Ok(Sweep::Unanchored(left)) if signalled => {
                return Unresolved(format!(
                    "{left} process(es) remain in group {pgid} but none of ours is left to prove they are ours"
                ));
            }
            Ok(Sweep::Unanchored(left)) => {
                return Unresolved(format!(
                    "{left} process(es) remain in group {pgid} but its leader is gone and none carries this run's {HOOK_RUN_ENV} tag, so none can be proven ours or was signalled"
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
    /// The listing found no live process in the group ([`reap_with`] confirms it is over).
    Empty,
    /// Live members remain; all were proven ours (and signalled, if asked).
    Live,
    /// Live members remain but no process proven ours still exists: nothing signalled.
    Unanchored(usize),
}

/// List the live members of `pgid` (with `list`), then accept them only if one of `anchors`
/// (processes already proven ours, the leader first) still exists in the group afterwards. With no
/// anchor left, a member that carries `tag` (read AFTER the listing, and still the same process
/// afterwards) becomes one. Accepted members become anchors and are each sent `signal`; SIGTERM
/// only once per member (`termed`), so a member that joined during the grace still gets one.
/// One unreadable process or one failed signal does not stop the others from being checked and
/// signalled (codex #1403 M4); it makes the sweep an error only if nothing else proves the group,
/// or after every member was tried.
fn sweep_verified_group(
    pgid: u32,
    anchors: &mut Vec<(u32, String)>,
    tag: Option<&str>,
    signal: i32,
    termed: &mut Vec<(u32, String)>,
    list: &dyn Fn(u32) -> Result<Vec<(u32, String)>, String>,
) -> Result<Sweep, String> {
    let members = list(pgid)?;
    let mut unreadable = Vec::new();
    let mut still = Vec::with_capacity(anchors.len());
    for (pid, start) in anchors.drain(..) {
        match proc_state(pid) {
            ProcState::Present { start: now, pgrp, .. } if now == start && (pgrp == pgid || pid == pgid) => {
                still.push((pid, start));
            }
            ProcState::Unreadable(error) => unreadable.push(format!("process {pid}: {error}")),
            _ => {}
        }
    }
    *anchors = still;
    if members.is_empty() {
        return Ok(Sweep::Empty);
    }
    if anchors.is_empty() {
        if let Some(tag) = tag {
            for (pid, start) in &members {
                if process_tag(*pid).as_deref() != Some(tag) {
                    continue;
                }
                // Still the process that was listed (same start time, same group) after its tag
                // was read: the tag is its own, and the group id was not reused in between.
                match proc_state(*pid) {
                    ProcState::Present { start: now, pgrp, .. } if now == *start && pgrp == pgid => {
                        anchors.push((*pid, start.clone()));
                    }
                    ProcState::Unreadable(error) => unreadable.push(format!("process {pid}: {error}")),
                    _ => {}
                }
            }
        }
        if anchors.is_empty() {
            // Nothing proved the group ours; an unreadable process might have.
            return if unreadable.is_empty() { Ok(Sweep::Unanchored(members.len())) } else { Err(unreadable.join("; ")) };
        }
    }
    let mut failed = Vec::new();
    for (pid, start) in &members {
        if !anchors.iter().any(|(p, s)| p == pid && s == start) {
            anchors.push((*pid, start.clone()));
        }
        if signal == SIGTERM && termed.iter().any(|(p, s)| p == pid && s == start) {
            continue;
        }
        match signal_pinned(*pid, start, pgid, signal) {
            Ok(_) if signal == SIGTERM => termed.push((*pid, start.clone())),
            Ok(_) => {}
            Err(error) => failed.push(format!("process {pid}: {error}")),
        }
    }
    if !failed.is_empty() {
        return Err(failed.join("; "));
    }
    Ok(Sweep::Live)
}

/// The [`HOOK_RUN_ENV`] value among `entries` (an environment block's `NAME=value` entries).
fn tag_in_environ<'a>(entries: impl IntoIterator<Item = &'a [u8]>) -> Option<String> {
    let prefix = format!("{HOOK_RUN_ENV}=");
    entries
        .into_iter()
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .and_then(|value| std::str::from_utf8(value).ok())
        .map(str::to_owned)
}

#[cfg(target_os = "linux")]
fn proc_state(pid: u32) -> ProcState {
    // Bytes, not UTF-8: `comm` is whatever the process was named (codex #1403 Low), and only the
    // numeric fields after it are read.
    let stat = match std::fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => String::from_utf8_lossy(&stat).into_owned(),
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
#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// `pid`'s [`HOOK_RUN_ENV`], from `/proc/<pid>/environ`. `None` when it has none, or when its
/// environment cannot be read (it then proves nothing).
#[cfg(target_os = "linux")]
fn process_tag(pid: u32) -> Option<String> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    tag_in_environ(environ.split(|byte| *byte == 0))
}

/// The live (non-zombie) processes in group `pgid`, as `(pid, start)`. An unreadable process of
/// this user might be a member, so it is an error; other users' processes are skipped.
#[cfg(target_os = "linux")]
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

/// macOS `pbi_status` of a zombie (`SZOMB`, <sys/proc.h>).
#[cfg(target_os = "macos")]
const MACOS_SZOMB: u32 = 5;

#[cfg(target_os = "macos")]
fn proc_state(pid: u32) -> ProcState {
    let Ok(raw_pid) = libc::c_int::try_from(pid) else {
        return ProcState::Absent;
    };
    // SAFETY: proc_bsdinfo is plain old data; all-zero is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: errno is this thread's; cleared so a failure is not read from an earlier call.
    unsafe { *libc::__error() = 0 };
    // SAFETY: the buffer is a proc_bsdinfo of exactly `size` bytes, owned by this frame.
    let read = unsafe {
        libc::proc_pidinfo(raw_pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
    };
    if read == size {
        return ProcState::Present {
            start: format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec),
            pgrp: info.pbi_pgid,
            zombie: info.pbi_status == MACOS_SZOMB,
        };
    }
    // A partial record is unreadable whatever errno says (codex #1403 M2); errno only explains a
    // failure (a return of 0 or less).
    if read > 0 {
        return ProcState::Unreadable(format!("proc_pidinfo: short read ({read} of {size} bytes)"));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        // proc_pidinfo does not report exited pids (zombies included): nothing live is there.
        Some(libc::ESRCH) => ProcState::Absent,
        Some(0) | None => ProcState::Unreadable("proc_pidinfo: no data and no error".into()),
        _ => ProcState::Unreadable(format!("proc_pidinfo: {error}")),
    }
}

/// This boot's id (macOS `kern.bootsessionuuid`): start times identify a process only within one.
#[cfg(target_os = "macos")]
fn boot_id() -> Option<String> {
    let mut buf = [0u8; 128];
    let mut len = buf.len();
    // SAFETY: a NUL-terminated name, a writable buffer and its length; no new value.
    let rc = unsafe {
        libc::sysctlbyname(c"kern.bootsessionuuid".as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0)
    };
    if rc != 0 {
        return None;
    }
    let id = std::str::from_utf8(buf.get(..len)?).ok()?.trim_end_matches('\0').trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// `pid`'s [`HOOK_RUN_ENV`], from `KERN_PROCARGS2`. `None` when it has none, or when its
/// environment cannot be read (it then proves nothing).
#[cfg(target_os = "macos")]
fn process_tag(pid: u32) -> Option<String> {
    let raw_pid = libc::c_int::try_from(pid).ok()?;
    let mut argmax: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: a NUL-terminated name and a c_int-sized output buffer; no new value.
    let rc = unsafe {
        libc::sysctlbyname(c"kern.argmax".as_ptr(), (&mut argmax as *mut libc::c_int).cast(), &mut len, std::ptr::null_mut(), 0)
    };
    if rc != 0 || argmax <= 0 {
        return None;
    }
    let mut buf = vec![0u8; usize::try_from(argmax).ok()?];
    let mut size = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, raw_pid];
    // SAFETY: a three-element MIB, a writable buffer of `size` bytes; no new value.
    let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
    if rc != 0 {
        return None;
    }
    buf.truncate(size);
    tag_in_environ(procargs2_environ(&buf)?)
}

/// The environment entries of a `KERN_PROCARGS2` buffer: `argc` (i32), the exec path, NUL padding,
/// `argc` argument strings, then the environment strings up to an empty one.
#[cfg(any(target_os = "macos", test))]
fn procargs2_environ(buf: &[u8]) -> Option<Vec<&[u8]>> {
    let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
    let rest = buf.get(4..)?;
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let rest = &rest[path_end..];
    let strings_start = rest.iter().position(|byte| *byte != 0)?;
    let mut strings = rest[strings_start..].split(|byte| *byte == 0);
    for _ in 0..argc {
        strings.next()?;
    }
    Some(strings.take_while(|entry| !entry.is_empty()).collect())
}

/// The live (non-zombie) processes in group `pgid`, as `(pid, start)`. libproc reports a failure
/// of `proc_listpgrppids` as 0 with errno set, so errno is cleared first and read on 0 (codex #1403
/// M2): an error is never an empty group. A full buffer may have cut the list short: it is listed
/// again into a bigger one. Zero-filled slots are no pid (0 is never in a user group), so the
/// count's unit (pids or bytes) does not matter.
#[cfg(target_os = "macos")]
fn live_group_members(pgid: u32) -> Result<Vec<(u32, String)>, String> {
    let raw_pgid = libc::pid_t::try_from(pgid).map_err(|_| format!("group id {pgid} out of range"))?;
    // SAFETY: a null buffer asks only for the size.
    let estimate = unsafe { libc::proc_listpgrppids(raw_pgid, std::ptr::null_mut(), 0) };
    let mut slots = usize::try_from(estimate).unwrap_or(0).max(64) * 2;
    for _ in 0..4 {
        let mut pids = vec![0 as libc::pid_t; slots];
        let bytes =
            libc::c_int::try_from(slots * std::mem::size_of::<libc::pid_t>()).map_err(|_| "too many pids".to_owned())?;
        // SAFETY: errno is this thread's; cleared so a failure is not read from an earlier call.
        unsafe { *libc::__error() = 0 };
        // SAFETY: the buffer holds exactly `bytes` bytes of pid_t slots, owned by this frame.
        let rc = unsafe { libc::proc_listpgrppids(raw_pgid, pids.as_mut_ptr().cast(), bytes) };
        let error = std::io::Error::last_os_error();
        if rc < 0 || (rc == 0 && !matches!(error.raw_os_error(), Some(0) | None)) {
            return Err(format!("proc_listpgrppids: {error}"));
        }
        if pids.last().is_some_and(|pid| *pid != 0) {
            slots *= 4;
            continue;
        }
        let mut members = Vec::new();
        for pid in pids.into_iter().filter_map(|pid| u32::try_from(pid).ok()).filter(|pid| *pid != 0) {
            match proc_state(pid) {
                ProcState::Present { start, pgrp, zombie } if pgrp == pgid && !zombie => members.push((pid, start)),
                ProcState::Present { .. } | ProcState::Absent => {}
                ProcState::Unreadable(error) => return Err(format!("process {pid}: {error}")),
            }
        }
        return Ok(members);
    }
    Err(format!("proc_listpgrppids: group {pgid} kept outgrowing the buffer"))
}

/// Send `sig` to `pid` only while it is still the process `(pid, start)` in group `pgid`. macOS has
/// no pidfd: the identity is checked immediately before `kill`, leaving only the instant between
/// the two for the pid to be recycled. `Ok(false)`: it was no longer there.
#[cfg(target_os = "macos")]
fn signal_pinned(pid: u32, start: &str, pgid: u32, sig: i32) -> Result<bool, String> {
    let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
        return Ok(false);
    };
    match proc_state(pid) {
        ProcState::Present { start: now, pgrp, .. } if now == start && pgrp == pgid => {}
        ProcState::Unreadable(error) => return Err(format!("process {pid}: {error}")),
        _ => return Ok(false),
    }
    // SAFETY: kill(2) takes two ints and touches no memory of ours.
    if unsafe { libc::kill(raw_pid, sig) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) { Ok(false) } else { Err(format!("kill: {error}")) }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn proc_state(_pid: u32) -> ProcState {
    ProcState::Unreadable("no process table on this host".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn boot_id() -> Option<String> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_tag(_pid: u32) -> Option<String> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn live_group_members(_pgid: u32) -> Result<Vec<(u32, String)>, String> {
    Err("no process table on this host".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn signal_pinned(_pid: u32, _start: &str, _pgid: u32, _sig: i32) -> Result<bool, String> {
    Err("no way to signal a verified process on this host".into())
}

/// Whether the kernel still has any process (a zombie included) with group id `pgid`: one
/// `kill(-pgid, 0)`, which checks and delivers nothing. EPERM means one exists that we may not
/// signal. Ids 0 and 1 are never treated as empty.
fn group_exists(pgid: u32) -> bool {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return false;
    };
    if pgid <= 1 {
        return true;
    }
    // SAFETY: kill(2) takes two ints and touches no memory of ours; signal 0 is not delivered.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
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
            "t/1",
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
            "t/1",
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
        assert_eq!(reap_stale_group(other.id(), Some(&format!("{boot}/1")), None), StaleReap::Gone);
        assert!(other.try_wait().expect("try_wait").is_none(), "an unverified group must be left alone");
        // Another boot: whatever runs under that pid now is not ours.
        let start = process_identity(other.id()).expect("identity");
        let (_, ticks) = start.split_once('/').expect("boot/start");
        assert_eq!(reap_stale_group(other.id(), Some(&format!("not-this-boot/{ticks}")), None), StaleReap::Gone);
        assert!(other.try_wait().expect("try_wait").is_none(), "a previous boot's record must not reach this boot's process");
        let _ = other.kill();
        let _ = other.wait();
        // No recorded identity: unresolved, left alone.
        assert!(matches!(reap_stale_group(1, None, None), StaleReap::Unresolved(_)));
        // Our leader (identity matches): the whole group dies, a member that ignores SIGTERM included.
        let mut ours = spawn_group("(trap '' TERM; sleep 30) & sleep 30");
        let pgid = ours.id();
        let identity = process_identity(pgid).expect("identity");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(reap_stale_group(pgid, Some(&identity), None), StaleReap::Killed);
        let status = ours.wait().expect("wait");
        assert!(status.code().is_none(), "killed by a signal: {status:?}");
        assert!(!group_alive(pgid), "Killed means nothing live is left in the group");
    }

    #[cfg(target_os = "linux")]
    fn spawn_tagged_group(script: &str, tag: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        Command::new("sh").args(["-c", script]).env(HOOK_RUN_ENV, tag).process_group(0).spawn().expect("spawn")
    }

    /// codex r2 #1: once the leader is gone, a process still carrying the group id is proven ours
    /// only by this run's tag. Without it (cleared, or another run's) nothing is signalled and the
    /// reap is unresolved: the job waits; once the group is empty it is gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stale_group_without_its_leader_or_its_tag_is_never_signalled() {
        let mut leader = spawn_group("sleep 30 & exit 0");
        let pgid = leader.id();
        let identity = process_identity(pgid).expect("identity");
        let _ = leader.wait(); // the leader exits and is reaped; its sleep stays in the group
        assert!(group_alive(pgid), "the member is still running");
        assert!(matches!(reap_stale_group(pgid, Some(&identity), None), StaleReap::Unresolved(_)));
        assert!(
            matches!(reap_stale_group(pgid, Some(&identity), Some("job-t/1")), StaleReap::Unresolved(_)),
            "a member with no tag is not proven ours"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(group_alive(pgid), "an unprovable member must not be signalled");
        signal_group(pgid, SIGKILL);
        let gone_by = Instant::now() + Duration::from_secs(5);
        while group_alive(pgid) && Instant::now() < gone_by {
            std::thread::sleep(POLL);
        }
        assert_eq!(reap_stale_group(pgid, Some(&identity), None), StaleReap::Gone);

        // Another run's tag (the previous attempt) proves nothing about this one.
        let mut leader = spawn_tagged_group("sleep 30 & exit 0", "job-t/2");
        let pgid = leader.id();
        let identity = process_identity(pgid).expect("identity");
        let _ = leader.wait();
        assert!(matches!(reap_stale_group(pgid, Some(&identity), Some("job-t/1")), StaleReap::Unresolved(_)));
        std::thread::sleep(Duration::from_millis(100));
        assert!(group_alive(pgid), "another run's member must not be signalled");
        signal_group(pgid, SIGKILL);
    }

    /// The leader is gone but a member carries this run's `MAXPLAYER_HOOK_RUN`: the tag proves the
    /// group ours, so every member (one that ignores SIGTERM included) is stopped and the job can be
    /// re-run instead of waiting for its deadline.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stale_group_without_its_leader_is_reaped_by_its_run_tag() {
        let mut leader = spawn_tagged_group("(trap '' TERM; sleep 30) & sleep 30 & exit 0", "job-t/1");
        let pgid = leader.id();
        let identity = process_identity(pgid).expect("identity");
        let _ = leader.wait();
        std::thread::sleep(Duration::from_millis(100));
        assert!(group_alive(pgid), "the members are still running");
        assert_eq!(reap_stale_group(pgid, Some(&identity), Some("job-t/1")), StaleReap::Killed);
        assert!(!group_alive(pgid), "Killed means nothing live is left in the group");
        assert_eq!(reap_stale_group(pgid, Some(&identity), Some("job-t/1")), StaleReap::Gone);
    }

    /// No identity could be recorded for the leader: the tag alone proves the group ours.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stale_group_with_no_recorded_identity_is_reaped_by_its_run_tag() {
        let mut ours = spawn_tagged_group("sleep 30", "job-n/1");
        let pgid = ours.id();
        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(reap_stale_group(pgid, None, Some("job-n/9")), StaleReap::Unresolved(_)));
        assert!(ours.try_wait().expect("try_wait").is_none(), "a wrong tag signals nothing");
        assert_eq!(reap_stale_group(pgid, None, Some("job-n/1")), StaleReap::Killed);
        let status = ours.wait().expect("wait");
        assert!(status.code().is_none(), "killed by a signal: {status:?}");
    }

    /// codex #1403 H3: one empty listing does not end the reap while the kernel still has the group
    /// (a member forked during the listing, by a process that then exited, is in no listing yet).
    #[cfg(target_os = "linux")]
    #[test]
    fn one_empty_listing_is_not_an_empty_group() {
        let mut ours = spawn_tagged_group("sleep 30", "job-e/1");
        let pgid = ours.id();
        std::thread::sleep(Duration::from_millis(50));
        let calls = std::cell::Cell::new(0);
        let missed_once = |pgid: u32| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 { Ok(Vec::new()) } else { live_group_members(pgid) }
        };
        assert_eq!(reap_with(pgid, Vec::new(), Some("job-e/1"), &missed_once), StaleReap::Killed);
        let status = ours.wait().expect("wait");
        assert!(status.code().is_none(), "the member the first listing missed was stopped: {status:?}");
    }

    /// The other side of H3: a group whose only process is a zombie (nobody reaped it yet) is over.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_group_of_only_zombies_is_gone() {
        let mut leader = spawn_tagged_group("exit 0", "job-z/1");
        let pgid = leader.id();
        let by = Instant::now() + Duration::from_secs(5);
        while !live_group_members(pgid).expect("list").is_empty() && Instant::now() < by {
            std::thread::sleep(POLL);
        }
        assert!(group_exists(pgid), "the unreaped leader still holds the group id");
        assert_eq!(reap_stale_group(pgid, None, Some("job-z/1")), StaleReap::Gone);
        let _ = leader.wait();
        assert!(!group_exists(pgid), "reaped: the kernel has no process with that group id");
    }

    /// codex #1403 M4: a member that joins the group after the first SIGTERM gets its own SIGTERM
    /// during the grace, not only the SIGKILL at its end.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_member_that_joins_during_the_grace_still_gets_sigterm() {
        let dir = tempfile::tempdir().expect("tmp");
        let mark = dir.path().join("termed");
        let script = format!(
            "trap : TERM; sleep 0.3; (trap 'echo got > {m}; exit 0' TERM; while :; do sleep 0.05; done) & while :; do sleep 0.05; done",
            m = mark.display()
        );
        let mut ours = spawn_group(&script);
        let pgid = ours.id();
        let identity = process_identity(pgid).expect("identity");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(reap_stale_group(pgid, Some(&identity), None), StaleReap::Killed);
        let _ = ours.wait();
        assert!(mark.exists(), "the member started after the first SIGTERM was sent one too");
    }

    /// codex #1403 Low: `comm` can hold any bytes; the process is still readable.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_with_a_non_utf8_name_is_readable() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().expect("tmp");
        let sleep = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .expect("sleep");
        let link = dir.path().join(std::ffi::OsStr::from_bytes(b"sl\xffp"));
        std::os::unix::fs::symlink(sleep, &link).expect("link");
        let mut child = Command::new(&link).arg("30").spawn().expect("spawn");
        std::thread::sleep(Duration::from_millis(100));
        let state = proc_state(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(matches!(state, ProcState::Present { .. }), "{state:?}");
    }

    #[test]
    fn a_gated_job_sees_its_run_tag_and_no_other_maxplayer_variable() {
        let dir = tempfile::tempdir().expect("tmp");
        let report = run_gated(
            &sh(r#"cat >/dev/null; env | grep '^MAXPLAYER_' | sort | tr '\n' ' ' > env.txt; printf '{"status":"done","answer":"x"}'"#),
            b"{}
",
            dir.path(),
            Instant::now() + Duration::from_secs(10),
            &run_tag("job-e", 3),
            &mut |_| Ok(()),
        );
        assert!(matches!(classify_job(&report, CAP), JobOutcome::Answer(_)), "{report:?}");
        let env = std::fs::read_to_string(dir.path().join("env.txt")).expect("env");
        assert_eq!(env, "MAXPLAYER_HOOK_RUN=job-e/3 MAXPLAYER_HOOK_V=1 ");
    }

    #[test]
    fn the_run_tag_is_found_in_a_procargs2_environment() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sh\0\0\0\0sh\0-c\0A=1\0MAXPLAYER_HOOK_RUN=job-m/2\0B=2\0\0junk=MAXPLAYER_HOOK_RUN=x\0");
        let environ = procargs2_environ(&buf).expect("parsed");
        assert_eq!(environ, vec![&b"A=1"[..], b"MAXPLAYER_HOOK_RUN=job-m/2", b"B=2"]);
        assert_eq!(tag_in_environ(environ).as_deref(), Some("job-m/2"));
        // An argument that looks like the variable is not the environment.
        let mut args = 1i32.to_ne_bytes().to_vec();
        args.extend_from_slice(b"/bin/x\0MAXPLAYER_HOOK_RUN=fake\0A=1\0\0");
        assert_eq!(tag_in_environ(procargs2_environ(&args).expect("parsed")), None);
        assert_eq!(procargs2_environ(b"\x01\0"), None, "truncated");
    }

    /// codex r2 #4: a journal write that outlasts the cutoff must not release the hook: it would
    /// start with its window already closed.
    #[test]
    fn a_journal_that_outlasts_the_cutoff_never_releases_the_hook() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("ran");
        let argv = sh(&format!("touch '{}'; cat >/dev/null; sleep 30", marker.display()));
        let report = run_gated(&argv, b"{}\n", dir.path(), Instant::now() + Duration::from_millis(300), "t/1", &mut |_| {
            std::thread::sleep(Duration::from_millis(600));
            Ok(())
        });
        assert_eq!(report.exit, Exit::NotStarted);
        assert_eq!(classify_job(&report, CAP), JobOutcome::OutOfTime { started: false });
        std::thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists(), "the hook never ran");
    }

    /// codex r2 #4: an overflow first seen while draining after the cutoff (here from a worker that
    /// left the group) leaves the no-strike cutoff standing.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_overflow_seen_after_the_cutoff_keeps_the_cutoff() {
        let report = run_sh(
            "setsid sh -c 'sleep 0.6; head -c 3000000 /dev/zero' & cat >/dev/null; sleep 30",
            b"",
            Duration::from_millis(300),
        );
        assert_eq!(report.exit, Exit::CutOff);
        assert_eq!(classify_job(&report, CAP), JobOutcome::OutOfTime { started: true });
    }

    /// codex r2 #4: the grace belongs to the whole group. A member still finishing its SIGTERM
    /// handling after the leader exited gets the rest of the grace, not an immediate SIGKILL.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_cutoff_grace_covers_members_that_outlive_the_leader() {
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("finished");
        let script = format!(
            r#"(trap 'sleep 0.5; touch "{}"; exit 0' TERM; while :; do sleep 0.1; done) & trap 'exit 0' TERM; while :; do sleep 0.1; done"#,
            marker.display()
        );
        let report = run(&sh(&script), b"", dir.path(), Instant::now() + Duration::from_millis(400));
        assert_eq!(report.exit, Exit::CutOff);
        assert!(marker.exists(), "the member finished its SIGTERM handling within the grace");
    }

    /// codex r2 #5: once shutdown has latched, a launch never releases its hook.
    #[test]
    fn after_shutdown_latched_a_launch_never_releases_its_hook() {
        let supervision = Supervision::new();
        terminate_all_in(&supervision);
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("ran");
        let argv = sh(&format!("touch '{}'", marker.display()));
        let report =
            run_gated_in(&supervision, &argv, b"{}\n", dir.path(), Instant::now() + Duration::from_secs(10), "t/1", &mut |_| Ok(()));
        assert_eq!(report.exit, Exit::Shutdown);
        assert_eq!(classify_job(&report, CAP), JobOutcome::Interrupted);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists(), "the hook never ran");
        assert!(supervision.lock().groups.is_empty());
    }

    /// codex r2 #5: shutdown latching while the launch is still journaling: the gate is never
    /// released, and shutdown waits for that supervisor's teardown.
    #[test]
    fn shutdown_during_the_journal_write_never_releases_the_hook() {
        let supervision = Supervision::new();
        let dir = tempfile::tempdir().expect("tmp");
        let marker = dir.path().join("ran");
        let argv = sh(&format!("touch '{}'", marker.display()));
        std::thread::scope(|scope| {
            let report = run_gated_in(&supervision, &argv, b"{}\n", dir.path(), Instant::now() + Duration::from_secs(10), "t/1", &mut |_| {
                scope.spawn(|| terminate_all_in(&supervision));
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            });
            assert_eq!(report.exit, Exit::Shutdown);
        });
        assert!(!marker.exists(), "the hook never ran");
        assert!(supervision.lock().groups.is_empty());
    }

    /// codex r2 #5: a hook stopped by daemon shutdown is interrupted, not broken: no strike, and the
    /// job is not failed (the next start re-drives it).
    #[test]
    fn a_hook_stopped_by_shutdown_is_interrupted_not_broken() {
        let supervision = Supervision::new();
        let dir = tempfile::tempdir().expect("tmp");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                terminate_all_in(&supervision);
            });
            let report = run_gated_in(
                &supervision,
                &sh("cat >/dev/null; sleep 30"),
                b"{}\n",
                dir.path(),
                Instant::now() + Duration::from_secs(20),
                "t/1",
                &mut |_| Ok(()),
            );
            assert_eq!(report.exit, Exit::Shutdown);
            assert_eq!(classify_job(&report, CAP), JobOutcome::Interrupted);
            assert!(!classify_job(&report, CAP).strikes());
        });
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
