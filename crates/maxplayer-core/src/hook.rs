//! The hook runner: a seller's OWN agent behind a command (docs/specs/hook-agents.md §5).
//!
//! The daemon runs the hook argv directly (no shell), writes one JSON object + `\n` to its stdin,
//! closes stdin, and drains stdout/stderr concurrently until the process exits or the cutoff.
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
//!   starts it outside its own group), and reader threads get a bounded wait, so a descendant that
//!   escaped the group and kept a pipe open cannot hold the runner.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
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
/// How long reader threads get after the reap before their buffers are read as-is.
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

#[derive(serde::Deserialize)]
struct WireAnswer {
    status: String,
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    reason: Option<String>,
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
    // serde_json refuses invalid UTF-8, trailing non-whitespace and (for a derived struct) a
    // duplicate field; unknown extra fields are tolerated.
    let wire: WireAnswer = match serde_json::from_slice(&report.stdout) {
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
    let report = run(argv, &ping_event(reason), cwd, Instant::now() + CONTROL_EVENT_BUDGET);
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
    let _ = run(argv, &close_event(job_id), cwd, Instant::now() + CONTROL_EVENT_BUDGET);
}

/// Whether a finished `ping` run means healthy: exit 0 before the cutoff. Stdout is ignored.
pub fn ping_healthy(report: &RunReport) -> bool {
    matches!(report.exit, Exit::Exited { code: Some(0) })
}

/// Run `argv` once under the supervision rules, feeding `stdin`, in `cwd`, until `cutoff`.
pub fn run(argv: &[String], stdin: &[u8], cwd: &Path, cutoff: Instant) -> RunReport {
    let started = Instant::now();
    let report = |exit, stdout, stderr_tail| RunReport {
        exit,
        stdout,
        stderr_tail,
        wall: started.elapsed(),
    };
    if Instant::now() >= cutoff {
        return report(Exit::NotStarted, Vec::new(), String::new());
    }
    let Some((program, args)) = argv.split_first() else {
        return report(Exit::SpawnFailed("empty argv".into()), Vec::new(), String::new());
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MAXPLAYER_") {
            command.env_remove(&key);
        }
    }
    command.env(HOOK_VERSION_ENV, HOOK_CONTRACT_VERSION.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return report(Exit::SpawnFailed(error.to_string()), Vec::new(), String::new()),
    };
    let pgid = child.id();

    // Stdin on its own thread: a hook that never reads it must not block the runner. EPIPE (the
    // hook exited or closed stdin early) is not an error; the exit status decides.
    if let Some(mut pipe) = child.stdin.take() {
        let payload = stdin.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&payload);
            // Dropping the pipe closes stdin.
        });
    }

    let stdout = Arc::new(Mutex::new(Vec::new()));
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stdout_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(mut pipe) = child.stdout.take() {
        let (buffer, overflow, done) = (Arc::clone(&stdout), Arc::clone(&overflow), Arc::clone(&stdout_done));
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut buffer = buffer.lock().expect("stdout buffer");
                        if buffer.len() + n > STDOUT_MAX_BYTES {
                            overflow.store(true, std::sync::atomic::Ordering::SeqCst);
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                    }
                }
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    } else {
        stdout_done.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let stderr_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(mut pipe) = child.stderr.take() {
        let (buffer, done) = (Arc::clone(&stderr), Arc::clone(&stderr_done));
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut buffer = buffer.lock().expect("stderr buffer");
                        buffer.extend_from_slice(&chunk[..n]);
                        let excess = buffer.len().saturating_sub(STDERR_TAIL_BYTES);
                        if excess > 0 {
                            buffer.drain(..excess);
                        }
                    }
                }
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    } else {
        stderr_done.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    let exit = loop {
        if overflow.load(std::sync::atomic::Ordering::SeqCst) {
            terminate_group(pgid, &mut child);
            break Exit::StdoutOverflow;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Exit::Exited { code: status.code() },
            Ok(None) => {}
            Err(error) => {
                terminate_group(pgid, &mut child);
                break Exit::SpawnFailed(format!("wait failed: {error}"));
            }
        }
        if Instant::now() >= cutoff {
            terminate_group(pgid, &mut child);
            break Exit::CutOff;
        }
        std::thread::sleep(POLL);
    };
    // Whatever is left of the group goes now (the child itself is already reaped).
    signal_group(pgid, SIGKILL);

    let join_by = Instant::now() + READER_JOIN_BUDGET;
    while Instant::now() < join_by
        && !(stdout_done.load(std::sync::atomic::Ordering::SeqCst)
            && stderr_done.load(std::sync::atomic::Ordering::SeqCst))
    {
        std::thread::sleep(POLL);
    }
    // An overflow that raced the exit still counts.
    let exit = if overflow.load(std::sync::atomic::Ordering::SeqCst) {
        Exit::StdoutOverflow
    } else {
        exit
    };
    let stdout = std::mem::take(&mut *stdout.lock().expect("stdout buffer"));
    let stderr_tail = String::from_utf8_lossy(&stderr.lock().expect("stderr buffer")).into_owned();
    report(exit, stdout, stderr_tail)
}

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// SIGTERM the group, wait out the grace, SIGKILL the group, reap the child.
fn terminate_group(pgid: u32, child: &mut std::process::Child) {
    signal_group(pgid, SIGTERM);
    let grace_until = Instant::now() + KILL_GRACE;
    while Instant::now() < grace_until {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(POLL);
    }
    signal_group(pgid, SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

/// `kill(-pgid, sig)`, refusing the widening ids 0 and 1. Errors (ESRCH: the group is gone) are
/// ignored: there is nothing left to stop.
fn signal_group(pgid: u32, sig: i32) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: std::ffi::c_int, sig: std::ffi::c_int) -> std::ffi::c_int;
        }
        let Ok(pgid) = std::ffi::c_int::try_from(pgid) else {
            return;
        };
        if pgid <= 1 {
            return;
        }
        // SAFETY: kill(2) takes two ints and touches no memory of ours.
        unsafe {
            kill(-pgid, sig);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, sig);
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
    fn a_background_worker_holding_stdout_does_not_hold_the_runner() {
        let started = Instant::now();
        // The worker leaves the group (setsid) and keeps the pipe open; the runner must still return.
        let report = run_sh(
            r#"setsid sh -c 'sleep 20' & printf '{"status":"done","answer":"ok"}'"#,
            b"",
            Duration::from_secs(20),
        );
        assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
        assert!(matches!(classify_job(&report, CAP), JobOutcome::Answer(_)), "{:?}", report);
    }

    #[test]
    fn stdin_carries_the_event_and_the_env_is_scrubbed() {
        // SAFETY: test-only; no other test reads this variable.
        unsafe { std::env::set_var("MAXPLAYER_SECRETISH_TEST", "leak") };
        let event = job_event("inv1", 1, 4076, "job1", 2, true, "the task, verbatim \"quoted\"", None, None, 99);
        let report = run_sh(
            r#"in=$(cat); env | grep '^MAXPLAYER_' | sort > /dev/stderr; printf '%s' "$in" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps({"status":"done","answer":d["prompt"]+"|"+d["invocation"]+"|"+str(d["turn"])+"|"+str(d["output"])+"|"+str(d["inputs_dir"])}))'"#,
            &event,
            Duration::from_secs(10),
        );
        unsafe { std::env::remove_var("MAXPLAYER_SECRETISH_TEST") };
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
}
