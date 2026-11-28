//! `maxplayer seller hook test <entry>` (docs/specs/hook-agents.md §10): drive a configured hook
//! entry through the contract the daemon uses — `ping`, a `job` turn 1 with a sample input file, a
//! `job` turn 2 of the same job, `close`, and an unknown event — and print what each call returned and
//! the verdict the daemon would reach. No relay, no money, no seller boot: the hook runs exactly as
//! the daemon runs it (no shell, its own process group, scrubbed environment, the same cutoffs), in a
//! throwaway job directory that is removed afterwards.
//!
//! Exit codes: 0 every step passed; 1 usage (or no such hook entry); 2 at least one step failed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maxplayer_core::hook::{self, Exit, JobOutcome, PingReason, RunReport};

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const FAILED: i32 = 2;

/// How long a test `job` call may run: the raw deadline is this far ahead; the event states it
/// minus the publish margin, and the cutoff is at that same instant, exactly as for a real job.
const TEST_JOB_SECS: u64 = 300;
const SAMPLE_INPUT: &str = "sample.txt";
const SAMPLE_INPUT_TEXT: &str = "maxplayer seller hook test: a sample input file.\n";
const TURN1_PROMPT: &str = "This is `maxplayer seller hook test`, not a buyer. Read inputs/sample.txt and reply with one short sentence saying what it contains.";
const TURN2_PROMPT: &str =
    "Follow-up from the same test: repeat your previous answer in five words or fewer.";
const SINGLE_TURN_PROMPT: &str =
    "This is `maxplayer seller hook test`, not a buyer. Reply with one short sentence.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    entry: String,
    home: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut iter = args.iter();
    match iter.next().map(String::as_str) {
        Some("test") => {}
        Some(other) => return Err(format!("unknown `seller hook` command: {other}")),
        None => return Err("missing command: `seller hook test <entry>`".into()),
    }
    let mut entry = None;
    let mut home = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--home" => {
                home = Some(PathBuf::from(
                    iter.next().ok_or("missing value for --home")?,
                ))
            }
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option for `seller hook test`: {flag}"));
            }
            name if entry.is_none() => entry = Some(name.to_owned()),
            extra => return Err(format!("unexpected argument: {extra}")),
        }
    }
    Ok(Options {
        entry: entry.ok_or("missing <entry>: the name of an [agents.<name>] hook entry")?,
        home,
    })
}

fn usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n  maxplayer seller hook test <entry> [--home <dir>]\n\n\
         Run the [agents.<entry>] hook the way the seller daemon runs it, without a relay or money:\n\
         ping (boot), a job turn 1 with a sample input file, a job turn 2 of the same job, close,\n\
         and an unknown event (which must exit 64). Prints each call's result and the daemon's\n\
         verdict on it. The job directory is a temporary one and is removed afterwards.\n\n\
         Exit codes: 0 every step passed; 1 usage or no such hook entry; 2 a step failed."
    );
}

/// Entry from `sell::run` for `maxplayer seller hook ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if crate::cli::is_help_request(args) || args.is_empty() {
        usage(if args.is_empty() { err } else { out });
        return if args.is_empty() {
            USAGE_ERROR
        } else {
            SUCCESS
        };
    }
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            usage(err);
            return USAGE_ERROR;
        }
    };
    let root = match options.home.clone() {
        Some(path) => path,
        None => match maxplayer_core::home::default_home_dir() {
            Ok(path) => path,
            Err(error) => {
                let _ = writeln!(err, "seller hook test: {error}");
                return USAGE_ERROR;
            }
        },
    };
    if !root.join("config.toml").is_file() {
        let _ = writeln!(
            err,
            "seller hook test: no maxplayer home at {} (no config.toml)",
            root.display()
        );
        return USAGE_ERROR;
    }
    let home = match maxplayer_core::home::bootstrap(&root) {
        Ok(home) => home,
        Err(error) => {
            let _ = writeln!(err, "seller hook test: {error}");
            return USAGE_ERROR;
        }
    };
    let Some(argv) = home
        .config
        .agents
        .get(&options.entry)
        .and_then(|preset| preset.hook.clone())
    else {
        let hooks: Vec<&str> = home
            .config
            .agents
            .iter()
            .filter(|(_, preset)| preset.is_hook())
            .map(|(name, _)| name.as_str())
            .collect();
        let _ = writeln!(
            err,
            "seller hook test: no hook entry [agents.{}] in {} (hook entries: {})",
            options.entry,
            root.join("config.toml").display(),
            if hooks.is_empty() {
                "none".to_owned()
            } else {
                hooks.join(", ")
            }
        );
        return USAGE_ERROR;
    };
    let _ = writeln!(
        out,
        "seller hook test: [agents.{}] hook = {:?}",
        options.entry, argv
    );
    let (passed, compatible) = run_all_steps(&argv, &home.root, out);
    if passed && compatible {
        let _ = writeln!(out, "\nPASS: the daemon would serve this hook.");
        SUCCESS
    } else if passed {
        let _ = writeln!(
            out,
            "\nFAIL (compatibility): the hook does not exit 64 on an event it does not know. Today's \
             daemon sends no such event, so current jobs are unaffected; a later contract version that \
             adds an event would break this hook."
        );
        FAILED
    } else {
        let _ = writeln!(
            out,
            "\nFAIL: fix the steps marked FAIL; each line says what the daemon would do."
        );
        FAILED
    }
}

/// Every step, in the daemon's order. Returns whether all passed (the unknown-event check included).
#[cfg(test)]
fn run_steps(argv: &[String], home_root: &Path, out: &mut dyn Write) -> bool {
    let (passed, compatible) = run_all_steps(argv, home_root, out);
    passed && compatible
}

/// Every step, in the daemon's order: `(the events the daemon sends all passed, the unknown-event
/// compatibility check passed)`.
fn run_all_steps(argv: &[String], home_root: &Path, out: &mut dyn Write) -> (bool, bool) {
    let mut passed = true;
    // 1. ping (boot): exit 0 within the control budget, stdout ignored (§5.2).
    let started = Instant::now();
    let ping = hook::ping(argv, home_root, PingReason::Boot);
    passed &= step(
        out,
        "ping (boot)",
        started,
        ping.as_ref().err().map(String::as_str),
        "healthy",
    );

    let job_id = random_hex();
    let dir = match job_dir(&job_id) {
        Ok(dir) => dir,
        Err(error) => {
            let _ = writeln!(out, "FAIL  job directory: {error}");
            return (false, false);
        }
    };
    // 2. job, turn 1 of a session, with an input file. 3. job, turn 2 of the same job, a new
    // invocation.
    let inputs = dir.join("inputs");
    for (turn, invocation, prompt) in [
        (1u32, job_id.clone(), TURN1_PROMPT),
        (2, random_hex(), TURN2_PROMPT),
    ] {
        let label = format!("job turn {turn}");
        passed &= job_step(out, argv, &dir, &job_id, &invocation, turn, prompt, true, Some(&inputs), &label);
    }
    let _ = std::fs::remove_dir_all(&dir);
    // 4. a single-turn job (no session, no inputs): only `done` is delivered there.
    let single_id = random_hex();
    match job_dir(&single_id) {
        Ok(single_dir) => {
            passed &= job_step(
                out,
                argv,
                &single_dir,
                &single_id,
                &single_id,
                1,
                SINGLE_TURN_PROMPT,
                false,
                None,
                "job single-turn",
            );
            let _ = std::fs::remove_dir_all(&single_dir);
        }
        Err(error) => {
            let _ = writeln!(out, "FAIL  job directory: {error}");
            passed = false;
        }
    }

    // 5. close: best effort, exit status ignored by the daemon; reported here for the operator.
    let started = Instant::now();
    let report = hook::run_control(
        argv,
        &hook::close_event(&job_id),
        home_root,
        Instant::now() + hook::CONTROL_EVENT_BUDGET,
    );
    let note = match report.exit {
        Exit::Exited { code: Some(0) } => "exit 0".to_owned(),
        ref other => format!(
            "{} (the daemon ignores a close's exit status)",
            describe_exit(other)
        ),
    };
    let _ = writeln!(
        out,
        "ok    close ({} ms): {note}",
        started.elapsed().as_millis()
    );

    // 6. an event this contract version does not define: exit 64 means "unsupported", never a
    // strike. The daemon sends none today, so this is reported as compatibility, not as a verdict.
    let started = Instant::now();
    let unknown = b"{\"v\":1,\"event\":\"maxplayer-hook-test-unknown\"}\n";
    let report = hook::run_control(
        argv,
        unknown,
        home_root,
        Instant::now() + hook::CONTROL_EVENT_BUDGET,
    );
    let failure = match report.exit {
        Exit::Exited { code: Some(64) } => None,
        ref other => Some(format!(
            "{}; a hook must exit 64 on an event it does not know, so later contract versions can add events \
             (compatibility only: today's daemon never sends one)",
            describe_exit(other)
        )),
    };
    let compatible = step(out, "unknown event", started, failure.as_deref(), "exit 64");
    (passed, compatible)
}

#[allow(clippy::too_many_arguments)]
fn job_step(
    out: &mut dyn Write,
    argv: &[String],
    dir: &Path,
    job_id: &str,
    invocation: &str,
    turn: u32,
    prompt: &str,
    session: bool,
    inputs: Option<&Path>,
    label: &str,
) -> bool {
    let cap = maxplayer_core::seller_node::run::INLINE_ANSWER_MAX_BYTES;
    let now = now_unix();
    let deadline = now + TEST_JOB_SECS;
    let event = hook::job_event(
        invocation,
        1,
        cap,
        job_id,
        turn,
        session,
        prompt,
        Some("text/plain"),
        inputs,
        deadline.saturating_sub(hook::PUBLISH_MARGIN_SECS),
    );
    let started = Instant::now();
    let report = hook::run(argv, &event, dir, hook::job_cutoff(deadline, now, started));
    if !report.stderr_tail.trim().is_empty() {
        let _ = writeln!(
            out,
            "      {label} stderr tail: {}",
            report.stderr_tail.trim().replace('\n', " | ")
        );
    }
    let (failure, verdict) = verdict(&report, cap, session);
    step(out, label, started, failure.as_deref(), &verdict)
}

/// The daemon's verdict on a `job` call (§5.1 "Outcomes"): `(None, what it would do)` when it would
/// deliver, `(Some(why), …)` when it would fail the job. `session`: whether the call was a session
/// turn; a single-turn job delivers `done` only (§5.0 rule 3, as `hook_run` decides).
fn verdict(report: &RunReport, cap: usize, session: bool) -> (Option<String>, String) {
    match hook::classify_job(report, cap) {
        JobOutcome::Answer(answer)
            if !session && answer.status != hook::HookStatus::Done =>
        {
            (
                Some(format!(
                    "status {} on a single-turn job: nothing delivered, the job fails delivery_failed, \
                     no strike (a single-turn job has no next turn; answer it with \"done\")",
                    answer.status.as_str()
                )),
                String::new(),
            )
        }
        JobOutcome::Answer(answer) => {
            let preview: String = answer.answer.chars().take(120).collect();
            let more = if answer.answer.chars().count() > 120 {
                "…"
            } else {
                ""
            };
            (
                None,
                format!(
                    "delivered as status {} ({} bytes): {:?}{more}",
                    answer.status.as_str(),
                    answer.answer.len(),
                    preview
                ),
            )
        }
        JobOutcome::CannotDeliver(reason) => (
            Some(format!(
                "cannot_deliver ({reason}): the job fails delivery_failed, no strike"
            )),
            String::new(),
        ),
        JobOutcome::TooLarge { bytes, cap } => (
            Some(format!(
                "answer is {bytes} bytes, over the {cap}-byte cap: refused (delivery_failed), never cut"
            )),
            String::new(),
        ),
        JobOutcome::OutOfTime { .. } => (
            Some(format!(
                "still running at the cutoff ({} s): killed, execution_failed. If the hook hands jobs to \
                 a running agent session, check that the agent answered through the adapter: a reply \
                 in the agent's own chat is never delivered",
                TEST_JOB_SECS - hook::PUBLISH_MARGIN_SECS
            )),
            String::new(),
        ),
        JobOutcome::Interrupted => (Some("interrupted".into()), String::new()),
        JobOutcome::Broken(why) => (
            Some(format!("{why}: execution_failed and a strike")),
            String::new(),
        ),
    }
}

fn step(
    out: &mut dyn Write,
    label: &str,
    started: Instant,
    failure: Option<&str>,
    ok: &str,
) -> bool {
    let ms = started.elapsed().as_millis();
    match failure {
        None => {
            let _ = writeln!(out, "ok    {label} ({ms} ms): {ok}");
            true
        }
        Some(why) => {
            let _ = writeln!(out, "FAIL  {label} ({ms} ms): {why}");
            false
        }
    }
}

fn describe_exit(exit: &Exit) -> String {
    match exit {
        Exit::Exited { code: Some(code) } => format!("exit {code}"),
        Exit::Exited { code: None } => "killed by a signal".into(),
        Exit::CutOff => format!(
            "no answer within {} s",
            hook::CONTROL_EVENT_BUDGET.as_secs()
        ),
        Exit::SpawnFailed(error) => format!("could not start: {error}"),
        Exit::StdoutOverflow => "stdout over the ceiling".into(),
        Exit::NotStarted => "not started".into(),
        Exit::Shutdown => "interrupted".into(),
    }
}

/// A private (0700) throwaway job directory with `inputs/sample.txt`, as the daemon lays one out.
fn job_dir(job_id: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("maxplayer-hook-test-{}", &job_id[..16]));
    std::fs::create_dir_all(dir.join("inputs")).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    std::fs::write(dir.join("inputs").join(SAMPLE_INPUT), SAMPLE_INPUT_TEXT)
        .map_err(|e| e.to_string())?;
    Ok(dir)
}

/// 32 random bytes as hex: a job id and follow-up ids shaped like real ones.
fn random_hex() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .expect("os randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with_hook(tag: &str, script: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mx-hook-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = maxplayer_core::home::bootstrap(&root).expect("home");
        maxplayer_core::home::save_config(&mut home, |config| {
            config.agents.insert(
                "my-hook".into(),
                maxplayer_core::home::AgentPresetConfig::hook(vec![
                    "sh".into(),
                    "-c".into(),
                    script.into(),
                ]),
            );
        })
        .expect("save");
        root
    }

    fn run_test(root: &Path, entry: &str) -> (i32, String, String) {
        let args: Vec<String> = ["test", entry, "--home", root.to_str().unwrap()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    /// A hook that keeps the contract passes every step; the turn-2 call is the same job with a new
    /// invocation, and the input file is where the event says.
    #[test]
    fn a_contract_keeping_hook_passes_every_step() {
        let script = r#"line=$(cat)
case "$line" in
  *'"event":"ping"'*|*'"event":"close"'*) exit 0;;
  *'"event":"job"'*) case "$line" in *'"inputs_dir":"'*)
       dir=$(printf '%s' "$line" | sed 's/.*"inputs_dir":"\([^"]*\)".*/\1/'); test -f "$dir/sample.txt" || exit 3;; esac
     printf '%s' '{"status":"done","answer":"it is a sample"}';;
  *) exit 64;;
esac"#;
        let root = home_with_hook("pass", script);
        let (code, out, err) = run_test(&root, "my-hook");
        assert_eq!(code, SUCCESS, "out: {out}\nerr: {err}");
        for line in [
            "ok    ping (boot)",
            "ok    job turn 1",
            "ok    job turn 2",
            "ok    job single-turn",
            "ok    close",
            "ok    unknown event",
            "PASS",
        ] {
            assert!(out.contains(line), "missing {line:?} in:\n{out}");
        }
        assert!(out.contains("\"it is a sample\""), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The verdicts the daemon would reach: a free-text answer strikes, and an unknown event that is
    /// answered with exit 0 instead of 64 fails the step.
    #[test]
    fn contract_violations_fail_with_the_daemons_verdict() {
        let script = r#"cat >/dev/null; echo "plain text, not the JSON envelope""#;
        let root = home_with_hook("fail", script);
        let (code, out, _) = run_test(&root, "my-hook");
        assert_eq!(code, FAILED, "{out}");
        assert!(
            out.contains("FAIL  job turn 1") && out.contains("a strike"),
            "{out}"
        );
        assert!(
            out.contains("FAIL  unknown event") && out.contains("exit 64"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// codex #1417 r3: a status other than `done` passes on a session turn but fails the single-turn
    /// job, exactly as the daemon refuses it there (no strike).
    #[test]
    fn needs_input_passes_on_a_session_turn_and_fails_a_single_turn_job() {
        let script = r#"line=$(cat)
case "$line" in
  *'"event":"ping"'*|*'"event":"close"'*) exit 0;;
  *'"event":"job"'*) printf '%s' '{"status":"needs_input","answer":"which file?"}';;
  *) exit 64;;
esac"#;
        let root = home_with_hook("needs-input", script);
        let (code, out, _) = run_test(&root, "my-hook");
        assert_eq!(code, FAILED, "{out}");
        assert!(out.contains("ok    job turn 1") && out.contains("ok    job turn 2"), "{out}");
        assert!(
            out.contains("FAIL  job single-turn")
                && out.contains("status needs_input on a single-turn job")
                && out.contains("no strike"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// codex #1417 r3: a hook that serves every event the daemon sends but exits 0 on an unknown one
    /// fails as a compatibility issue, not with a strike-or-drop verdict.
    #[test]
    fn an_unknown_event_failure_is_reported_as_compatibility() {
        let script = r#"line=$(cat)
case "$line" in
  *'"event":"job"'*) printf '%s' '{"status":"done","answer":"fine"}';;
  *) exit 0;;
esac"#;
        let root = home_with_hook("compat", script);
        let (code, out, _) = run_test(&root, "my-hook");
        assert_eq!(code, FAILED, "{out}");
        assert!(out.contains("ok    job single-turn"), "{out}");
        assert!(out.contains("FAIL  unknown event"), "{out}");
        assert!(
            out.contains("FAIL (compatibility)") && !out.contains("strike or drop"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_entry_names_the_hook_entries_there_are() {
        let root = home_with_hook("missing", "exit 0");
        let (code, _, err) = run_test(&root, "nope");
        assert_eq!(code, USAGE_ERROR);
        assert!(
            err.contains("no hook entry [agents.nope]") && err.contains("my-hook"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn examples_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hooks")
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mx-hook-adapter-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn home_with_argv(tag: &str, argv: Vec<String>) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mx-hook-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut home = maxplayer_core::home::bootstrap(&root).expect("home");
        maxplayer_core::home::save_config(&mut home, |config| {
            config.agents.insert(
                "my-hook".into(),
                maxplayer_core::home::AgentPresetConfig::hook(argv),
            );
        })
        .expect("save");
        root
    }

    /// The shipped `spool-file.sh` with a notify command standing in for the scheduled run: every
    /// step passes, each turn's answer is the one written to `answers/`, and `close` empties the
    /// spool of that job.
    #[test]
    fn the_spool_file_adapter_keeps_the_contract() {
        let tmp = scratch("spool");
        let notify = tmp.join("answer-now");
        std::fs::write(
            &notify,
            r#"#!/bin/sh
f="$1"; d=$(dirname "$(dirname "$f")"); k=$(basename "$f" .json)
jq -r '"spool answer turn " + (.turn|tostring)' "$f" > "$d/answers/.$k.tmp"
mv "$d/answers/.$k.tmp" "$d/answers/$k.txt"
"#,
        )
        .unwrap();
        std::fs::set_permissions(
            &notify,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let spool = tmp.join("spool");
        let script = examples_dir().join("spool-file.sh").canonicalize().unwrap();
        let root = home_with_argv(
            "spool",
            vec![
                "env".into(),
                format!("MAXPLAYER_SPOOL_DIR={}", spool.display()),
                format!("MAXPLAYER_SPOOL_NOTIFY={}", notify.display()),
                "MAXPLAYER_SPOOL_POLL_SECS=0.2".into(),
                script.display().to_string(),
            ],
        );
        let (code, out, err) = run_test(&root, "my-hook");
        assert_eq!(code, SUCCESS, "out:\n{out}\nerr:\n{err}");
        assert!(out.contains("spool answer turn 1"), "{out}");
        assert!(out.contains("spool answer turn 2"), "{out}");
        // Nothing is left waiting: both turns were answered, none expired.
        let waiting: Vec<_> = walk(&spool.join("pending"))
            .into_iter()
            .chain(walk(&spool.join("expired")))
            .collect();
        assert!(waiting.is_empty(), "no request left pending or expired: {waiting:?}");
        // `close` removed the session job's two answers; only the single-turn job's answer (it
        // gets no `close`) is left for the weekly sweep on `ping`.
        let answers = walk(&spool.join("answers"));
        assert_eq!(answers.len(), 1, "close emptied the session job: {answers:?}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// One direct call of `spool-file.sh` with `event` on stdin: (exit code, stdout, stderr).
    fn spool_call(spool: &Path, env: &[(&str, String)], event: &serde_json::Value) -> (i32, String, String) {
        use std::io::Write as _;
        let script = examples_dir().join("spool-file.sh").canonicalize().unwrap();
        let mut command = std::process::Command::new(script);
        command
            .env("MAXPLAYER_SPOOL_DIR", spool)
            .env("MAXPLAYER_SPOOL_POLL_SECS", "0.1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(
            path,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
    }

    /// A notify command that fails is retried until it succeeds; a retried turn whose request is
    /// still pending (a daemon restart) notifies again; a session turn can answer `needs_input`;
    /// and `MAXPLAYER_SPOOL_READY` decides `ping`.
    #[test]
    fn the_spool_file_adapter_retries_notify_and_resumes_after_a_restart() {
        let tmp = scratch("spool-retry");
        let spool = tmp.join("spool");
        let calls = tmp.join("calls");
        let notify = tmp.join("notify");
        // Fails the first two calls, then answers for the agent.
        executable(
            &notify,
            &format!(
                r#"#!/bin/sh
n=$(($(cat '{calls}' 2>/dev/null || echo 0) + 1)); echo $n > '{calls}'
[ "$n" -lt 3 ] && exit 7
f="$1"; d=$(dirname "$(dirname "$f")"); k=$(basename "$f" .json)
echo "answered on call $n" > "$d/answers/.$k.tmp" && mv "$d/answers/.$k.tmp" "$d/answers/$k.txt"
"#,
                calls = calls.display()
            ),
        );
        let job = "ab".repeat(32);
        let deadline = now_unix() + 60;
        let event = |turn: u32, session: bool| spool_event(&job, &inv(turn), turn, session, 100, deadline);
        let env = [
            ("MAXPLAYER_SPOOL_NOTIFY", notify.display().to_string()),
            ("MAXPLAYER_SPOOL_NOTIFY_RETRY_SECS", "1".to_owned()),
        ];
        let (code, out, err) = spool_call(&spool, &env, &event(1, false));
        assert_eq!(code, 0, "{err}");
        let answer = |out: &str| serde_json::from_str::<serde_json::Value>(out.trim()).unwrap();
        assert_eq!(answer(&out), serde_json::json!({"status": "done", "answer": "answered on call 3\n"}), "{err}");
        assert_eq!(err.matches("notify failed").count(), 2, "{err}");

        // A restart: turn 2's request is already pending from the earlier attempt, unanswered.
        std::fs::write(spool.join("pending").join(format!("{}.json", spool_key(&job, 2, &inv(2)))), "{}").unwrap();
        let (code, out, err) = spool_call(&spool, &env, &event(2, true));
        assert_eq!(code, 0, "{err}");
        assert_eq!(
            answer(&out),
            serde_json::json!({"status": "done", "answer": "answered on call 4\n"}),
            "the pending request was notified again: {err}"
        );

        // A session turn the agent answers with needs_input.
        std::fs::write(spool.join("answers").join(format!("{}.needs_input", spool_key(&job, 3, &inv(3)))), "which file?").unwrap();
        let (code, out, _) = spool_call(&spool, &[], &event(3, true));
        assert_eq!(code, 0);
        assert_eq!(answer(&out), serde_json::json!({"status": "needs_input", "answer": "which file?"}));

        let ping = serde_json::json!({"v": 1, "event": "ping", "reason": "boot"});
        // Readiness helpers of our own: /bin/true and /bin/false are not there on every platform.
        let (ready_no, ready_yes) = (tmp.join("not-ready"), tmp.join("ready"));
        executable(&ready_no, "#!/bin/sh\nexit 1\n");
        executable(&ready_yes, "#!/bin/sh\nexit 0\n");
        let (code, _, err) = spool_call(&spool, &[("MAXPLAYER_SPOOL_READY", ready_no.display().to_string())], &ping);
        assert_eq!(code, 1, "a failing readiness check fails the ping: {err}");
        assert!(err.contains("readiness check failed"), "{err}");
        let (code, _, _) = spool_call(&spool, &[("MAXPLAYER_SPOOL_READY", ready_yes.display().to_string())], &ping);
        assert_eq!(code, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A 64-hex invocation for test turn `n`.
    fn inv(n: u32) -> String {
        format!("{n:02x}").repeat(32)
    }

    /// The spool key `spool-file.sh` uses: job, turn and the invocation's first 16 hex.
    fn spool_key(job: &str, turn: u32, invocation: &str) -> String {
        format!("{job}-{turn}-{}", &invocation[..16])
    }

    fn spool_event(job: &str, invocation: &str, turn: u32, session: bool, cap: usize, deadline: u64) -> serde_json::Value {
        serde_json::json!({"v": 1, "event": "job", "invocation": invocation, "attempt": 1, "job_id": job,
            "turn": turn, "session": session, "prompt": "p", "answer_max_bytes": cap, "deadline_unix": deadline})
    }

    fn answer_of(out: &str) -> serde_json::Value {
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"))
    }

    /// Advisor #1452 r1: answers the daemon would reject are refused in the spool (empty, too long
    /// after UTF-8 decoding, a session-only status on a single-turn job), with an `.error` and a
    /// new notify, never handed over; an empty `.decline` declines; a new invocation for the same
    /// turn is not poisoned by an older decline; a restart whose answer is already written delivers
    /// without notifying; the deadline moves the request to `expired/`.
    #[test]
    fn the_spool_file_adapter_refuses_bad_answers_and_keys_by_invocation() {
        let tmp = scratch("spool-refuse");
        let spool = tmp.join("spool");
        let answers = spool.join("answers");
        std::fs::create_dir_all(&answers).unwrap();
        let job = "cd".repeat(32);
        let far = now_unix() + 60;
        let write_answer = |key: &str, ext: &str, body: &[u8]| {
            let tmp_file = answers.join(format!(".{key}.tmp"));
            std::fs::write(&tmp_file, body).unwrap();
            std::fs::rename(&tmp_file, answers.join(format!("{key}.{ext}"))).unwrap();
        };

        // Empty answer: refused, the agent is notified again, and its corrected answer is delivered.
        let calls = tmp.join("calls");
        let notify = tmp.join("notify");
        executable(
            &notify,
            &format!(
                r#"#!/bin/sh
n=$(($(cat '{calls}' 2>/dev/null || echo 0) + 1)); echo $n > '{calls}'
f="$1"; d=$(dirname "$(dirname "$f")"); k=$(basename "$f" .json)
if [ "$n" -eq 1 ]; then printf '  
' > "$d/answers/.$k.tmp"; else echo "real answer" > "$d/answers/.$k.tmp"; fi
mv "$d/answers/.$k.tmp" "$d/answers/$k.txt"
"#,
                calls = calls.display()
            ),
        );
        let key1 = spool_key(&job, 1, &inv(1));
        let (code, out, err) = spool_call(
            &spool,
            &[("MAXPLAYER_SPOOL_NOTIFY", notify.display().to_string())],
            &spool_event(&job, &inv(1), 1, false, 100, far),
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(answer_of(&out), serde_json::json!({"status": "done", "answer": "real answer\n"}), "{err}");
        assert!(err.contains("the answer is empty"), "{err}");
        assert!(answers.join(format!("{key1}.txt.refused")).exists());
        assert_eq!(std::fs::read_to_string(&calls).unwrap().trim(), "2", "notified again after the refusal");

        // Restart with the answer already written: delivered at once, the notify command never runs.
        let (code, out, err) = spool_call(
            &spool,
            &[("MAXPLAYER_SPOOL_NOTIFY", tmp.join("missing-notify").display().to_string())],
            &spool_event(&job, &inv(1), 1, false, 100, far),
        );
        assert_eq!((code, answer_of(&out)["answer"].clone()), (0, serde_json::json!("real answer\n")), "{err}");
        assert!(!err.contains("notify"), "{err}");

        // Invalid UTF-8: 4 bytes on disk, 12 once decoded, over a cap of 6. Refused, never handed
        // over; at the deadline the request moves to expired/ and the turn declines.
        let key2 = spool_key(&job, 2, &inv(2));
        write_answer(&key2, "txt", b"\xff\xfe\xfd\xfc");
        let (code, out, err) = spool_call(
            &spool,
            &[("MAXPLAYER_SPOOL_FINISH_SECS", "3".to_owned())],
            &spool_event(&job, &inv(2), 2, true, 6, now_unix() + 5),
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(answer_of(&out), serde_json::json!({"status": "cannot_deliver", "reason": "no answer before the deadline"}));
        let error = std::fs::read_to_string(spool.join("pending").join(format!("{key2}.error"))).unwrap();
        assert!(error.contains("answer is 12 bytes; the limit is 6 bytes"), "{error}");
        assert!(spool.join("expired").join(format!("{key2}.json")).exists());

        // An empty .decline declines (the reason is optional).
        let key3 = spool_key(&job, 3, &inv(3));
        write_answer(&key3, "decline", b"");
        let (code, out, _) = spool_call(&spool, &[], &spool_event(&job, &inv(3), 3, true, 100, far));
        assert_eq!((code, answer_of(&out)), (0, serde_json::json!({"status": "cannot_deliver", "reason": ""})));

        // A new follow-up for the same turn (new invocation) is not answered by that old decline.
        write_answer(&spool_key(&job, 3, &inv(30)), "txt", b"the new follow-up's answer");
        let (code, out, _) = spool_call(&spool, &[], &spool_event(&job, &inv(30), 3, true, 100, far));
        assert_eq!(
            (code, answer_of(&out)),
            (0, serde_json::json!({"status": "done", "answer": "the new follow-up's answer"}))
        );

        // needs_input on a single-turn job is refused with an .error; the agent's decline then counts.
        let key4 = spool_key(&job, 4, &inv(4));
        write_answer(&key4, "needs_input", b"which file?");
        let decline_later = tmp.join("decline-later");
        executable(
            &decline_later,
            &format!(
                "#!/bin/sh\nsleep 1; echo nope > '{a}/.d' && mv '{a}/.d' '{a}/{key4}.decline'\n",
                a = answers.display()
            ),
        );
        let mut writer = std::process::Command::new(&decline_later).spawn().unwrap();
        let (code, out, err) = spool_call(&spool, &[], &spool_event(&job, &inv(4), 4, false, 100, far));
        writer.wait().unwrap();
        assert_eq!((code, answer_of(&out)), (0, serde_json::json!({"status": "cannot_deliver", "reason": "nope"})), "{err}");
        assert!(err.contains("needs_input is for session jobs only"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Advisor #1452 r1: a notify command that hangs (and ignores SIGTERM) is cut so the hook still
    /// declines before the deadline; `ping`'s weekly sweep touches only the spool's own folders and
    /// leaves an existing folder's permissions alone.
    #[test]
    fn the_spool_file_adapter_stays_inside_its_deadline_and_its_folders() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = scratch("spool-bounds");
        let spool = tmp.join("spool");
        let hang = tmp.join("hang");
        executable(&hang, "#!/bin/sh\ntrap '' TERM\nsleep 30\n");
        let job = "ef".repeat(32);
        let deadline = now_unix() + 7;
        let started = std::time::Instant::now();
        let (code, out, err) = spool_call(
            &spool,
            &[("MAXPLAYER_SPOOL_NOTIFY", hang.display().to_string())],
            &spool_event(&job, &inv(1), 1, false, 100, deadline),
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(answer_of(&out)["status"], "cannot_deliver", "{err}");
        assert!(now_unix() < deadline, "declined before the deadline ({:?} elapsed): {err}", started.elapsed());

        // The same with a `timeout` that, like some builds (uutils), signals only its direct child:
        // the hanging notify's own child (its `sleep`) survives that kill, and must not hold the
        // hook's output open past the deadline.
        let bin = tmp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(
            &bin.join("timeout"),
            "#!/bin/sh
if [ \"$1\" = -k ]; then grace=$2; shift 2; else grace=0; fi
secs=$1; shift
\"$@\" & child=$!
( sleep \"$secs\"; kill -TERM $child 2>/dev/null; sleep \"$grace\"; kill -KILL $child 2>/dev/null ) >/dev/null 2>&1 </dev/null &
watcher=$!
wait $child; rc=$?
kill $watcher 2>/dev/null
exit $rc
",
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let job = "fe".repeat(32);
        let deadline = now_unix() + 7;
        let started = std::time::Instant::now();
        let (code, out, err) = spool_call(
            &spool,
            &[("MAXPLAYER_SPOOL_NOTIFY", hang.display().to_string()), ("PATH", path)],
            &spool_event(&job, &inv(2), 1, false, 100, deadline),
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(answer_of(&out)["status"], "cannot_deliver", "{err}");
        assert!(
            now_unix() < deadline,
            "child-only timeout: declined before the deadline ({:?} elapsed): {err}",
            started.elapsed()
        );

        std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o770)).unwrap();
        let operator_file = spool.join("ready.sh");
        let old_answer = spool.join("answers").join("old.txt");
        std::fs::write(&operator_file, "#!/bin/sh\n").unwrap();
        std::fs::write(&old_answer, "old").unwrap();
        let week_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 86_400);
        for path in [&operator_file, &old_answer] {
            std::fs::File::options().write(true).open(path).unwrap().set_modified(week_ago).unwrap();
        }
        let ping = serde_json::json!({"v": 1, "event": "ping", "reason": "boot"});
        let (code, _, err) = spool_call(&spool, &[], &ping);
        assert_eq!(code, 0, "{err}");
        assert!(operator_file.exists(), "an operator's file next to the spool folders is kept");
        assert!(!old_answer.exists(), "a week-old spool file is swept");
        assert_eq!(std::fs::metadata(&spool).unwrap().permissions().mode() & 0o777, 0o770);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    /// The shipped `claude-per-turn.sh` against a fake `claude`: it passes every step, turn 2 resumes
    /// the session turn 1 started, and `close` removes the job's state file.
    #[test]
    fn the_claude_per_turn_adapter_keeps_the_contract_and_resumes_turn_two() {
        let tmp = scratch("per-turn");
        let fake = tmp.join("fake-claude");
        let counter = tmp.join("calls");
        std::fs::write(
            &fake,
            format!(
                r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "0.0.0 (fake)"; exit 0; fi
resume=none; prev=; restricted=no; nohooks=no
for a in "$@"; do
  [ "$prev" = "--resume" ] && resume="$a"
  [ "$a" = "--restricted" ] && restricted=yes
  [ "$prev" = "--settings" ] && [ "$a" = '{{"disableAllHooks":true}}' ] && nohooks=yes
  prev="$a"
done
cat >/dev/null
if [ "$restricted$nohooks" != yesyes ]; then
  jq -nc '{{type:"result",subtype:"success",is_error:false,result:"UNISOLATED: your hooks saw the prompt"}}'; exit 0
fi
echo x >> '{counter}'; n=$(wc -l < '{counter}' | tr -d ' ')
jq -nc --arg r "turn answer (resumed: $resume)" --arg s "sess-$n" '{{type:"result",subtype:"success",is_error:false,result:$r,session_id:$s}}'
"#,
                counter = counter.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &fake,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let state = tmp.join("state");
        let script = examples_dir()
            .join("claude-per-turn.sh")
            .canonicalize()
            .unwrap();
        let root = home_with_argv(
            "per-turn",
            vec![
                "env".into(),
                format!("CLAUDE_BIN={}", fake.display()),
                format!("MAXPLAYER_HOOK_STATE={}", state.display()),
                script.display().to_string(),
            ],
        );
        let (code, out, err) = run_test(&root, "my-hook");
        assert_eq!(code, SUCCESS, "out:\n{out}\nerr:\n{err}");
        assert!(
            out.contains("turn answer (resumed: none)"),
            "turn 1 starts a session:\n{out}"
        );
        assert!(
            out.contains("turn answer (resumed: sess-1)"),
            "turn 2 resumes it:\n{out}"
        );
        assert!(
            !out.contains("UNISOLATED"),
            "claude runs --restricted with hooks off:\n{out}"
        );
        let left: Vec<_> = std::fs::read_dir(&state).unwrap().collect();
        assert!(
            left.is_empty(),
            "close removed the job's state file: {left:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// codex #1417 r3: only one successful result envelope with a string reply is an answer. A reply
    /// that is an object (or an envelope without the success subtype) fails the call: nothing is
    /// printed as an answer and no session state is saved.
    #[test]
    fn the_claude_per_turn_adapter_refuses_an_unexpected_result_envelope() {
        let tmp = scratch("per-turn-bad");
        let fake = tmp.join("fake-claude");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "0.0.0 (fake)"; exit 0; fi
cat >/dev/null
jq -nc '{type:"result",is_error:false,result:{leaked:"object"},session_id:"sess-x"}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(
            &fake,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let state = tmp.join("state");
        let script = examples_dir()
            .join("claude-per-turn.sh")
            .canonicalize()
            .unwrap();
        let root = home_with_argv(
            "per-turn-bad",
            vec![
                "env".into(),
                format!("CLAUDE_BIN={}", fake.display()),
                format!("MAXPLAYER_HOOK_STATE={}", state.display()),
                script.display().to_string(),
            ],
        );
        let (code, out, _) = run_test(&root, "my-hook");
        assert_eq!(code, FAILED, "{out}");
        assert!(out.contains("FAIL  job turn 1") && out.contains("a strike"), "{out}");
        assert!(
            !out.contains("delivered as status"),
            "the object is never delivered as an answer: {out}"
        );
        let left: Vec<_> = std::fs::read_dir(&state)
            .map(|dir| dir.collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "no session state saved: {left:?}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The shipped channel adapter: `serve` driven by a fake Claude (an MCP client over stdio), `push`
    /// as the hook. The fake ignores turn 1 (as a session that answered in its chat would) and answers
    /// it only after the reminder event; it answers turn 2 at once. Every step passes; the answer tool
    /// asks to load upfront; every job names the full tool name; only the ignored turn is reminded;
    /// a late answer for turn 1 that arrives while turn 2 waits is refused and never becomes turn 2's
    /// answer; with no session attached, ping fails.
    #[test]
    fn the_channel_adapter_matches_each_answer_to_its_own_invocation() {
        use std::io::{BufRead, BufReader};
        use std::sync::{Arc, Mutex};
        let tmp = scratch("channel");
        let script = examples_dir()
            .join("claude-channel/maxplayer-channel.mjs")
            .canonicalize()
            .unwrap();
        let argv: Vec<String> = vec![
            "env".into(),
            format!("MAXPLAYER_CHANNEL_DIR={}", tmp.display()),
            "node".into(),
            script.display().to_string(),
            "push".into(),
        ];
        // No serve running: the hook's ping fails (the seat would not take jobs).
        assert!(hook::ping(&argv, &tmp, PingReason::Boot).is_err());

        let mut serve = std::process::Command::new("node")
            .arg(&script)
            .arg("serve")
            .env("MAXPLAYER_CHANNEL_DIR", &tmp)
            .env("MAXPLAYER_CHANNEL_REMIND_SECS", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("node");
        let stdin = Arc::new(Mutex::new(serve.stdin.take().unwrap()));
        let write = {
            let stdin = Arc::clone(&stdin);
            move |value: serde_json::Value| {
                let mut stdin = stdin.lock().unwrap();
                writeln!(stdin, "{value}").unwrap();
                stdin.flush().unwrap();
            }
        };
        write(
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake-claude","version":"0"}}}),
        );
        let replies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let events = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let reader = {
            let (replies, events, write) =
                (Arc::clone(&replies), Arc::clone(&events), write.clone());
            let stdout = BufReader::new(serve.stdout.take().unwrap());
            std::thread::spawn(move || {
                let mut first_invocation: Option<String> = None;
                let mut next_id = 100;
                for line in stdout.lines() {
                    let Ok(line) = line else { break };
                    let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    if message["method"] != "notifications/claude/channel" {
                        replies.lock().unwrap().push(message);
                        continue;
                    }
                    events.lock().unwrap().push(message.clone());
                    let meta = &message["params"]["meta"];
                    let Some(invocation) = meta["invocation"].as_str() else {
                        continue;
                    };
                    let turn = if meta["event"] == "reminder" {
                        "1".to_owned()
                    } else {
                        meta["turn"].as_str().unwrap_or("?").to_owned()
                    };
                    let mut answer = |invocation: &str, text: String| {
                        next_id += 1;
                        write(
                            serde_json::json!({"jsonrpc":"2.0","id":next_id,"method":"tools/call","params":{"name":"answer","arguments":{"invocation":invocation,"status":"done","text":text}}}),
                        );
                    };
                    if turn == "2" {
                        // A late answer for turn 1, arriving while turn 2 waits.
                        if let Some(first) = first_invocation.clone() {
                            answer(&first, "late answer for turn 1".into());
                        }
                    } else if meta["event"] != "reminder" {
                        // Turn 1 "answered in the chat": no tool call until the reminder.
                        first_invocation = Some(invocation.to_owned());
                        continue;
                    }
                    answer(invocation, format!("answer to turn {turn}"));
                }
            })
        };
        // Wait for the initialize reply, then attach the session.
        for _ in 0..100 {
            if replies.lock().unwrap().iter().any(|r| r["id"] == 1)
                && tmp.join("channel.sock").exists()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        write(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        write(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        std::thread::sleep(Duration::from_millis(200));
        let tools = replies
            .lock()
            .unwrap()
            .iter()
            .find(|r| r["id"] == 2)
            .cloned()
            .expect("tools/list reply");
        assert_eq!(
            tools["result"]["tools"][0]["_meta"]["anthropic/alwaysLoad"], true,
            "the answer tool asks Claude Code to load it upfront: {tools}"
        );

        let mut out = Vec::new();
        let passed = run_steps(&argv, &tmp, &mut out);
        let out = String::from_utf8_lossy(&out).into_owned();
        assert!(passed, "{out}");
        assert!(
            out.contains("\"answer to turn 1\"") && out.contains("\"answer to turn 2\""),
            "{out}"
        );
        assert!(
            !out.contains("late answer"),
            "the late turn-1 answer never reaches turn 2:\n{out}"
        );
        let refused = replies.lock().unwrap().iter().any(|r| {
            r["result"]["isError"] == true
                && r["result"]["content"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("no job is waiting"))
        });
        assert!(
            refused,
            "the late answer is refused to Claude: {:?}",
            replies.lock().unwrap()
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e["params"]["meta"]["event"] == "closed"),
            "close reaches the session"
        );
        let events = events.lock().unwrap().clone();
        let jobs: Vec<_> = events
            .iter()
            .filter(|e| e["params"]["meta"]["turn"].is_string())
            .collect();
        assert_eq!(jobs.len(), 3, "session turns 1 and 2, then the single-turn job: {events:?}");
        for job in &jobs {
            let invocation = job["params"]["meta"]["invocation"].as_str().unwrap();
            let content = job["params"]["content"].as_str().unwrap();
            assert!(
                content.starts_with(&format!(
                    "[Maxplayer job. Deliver your answer ONLY by calling the tool mcp__maxplayer__answer with invocation \"{invocation}\"."
                )) && content.contains("never delivered"),
                "each job names the full tool and that a chat reply is not delivered: {content}"
            );
        }
        let reminders: Vec<_> = events
            .iter()
            .filter(|e| e["params"]["meta"]["event"] == "reminder")
            .collect();
        assert_eq!(
            reminders.len(),
            2,
            "only the calls left unanswered (both turn-1 calls) are reminded: {reminders:?}"
        );
        assert_eq!(
            reminders[0]["params"]["meta"]["invocation"],
            jobs[0]["params"]["meta"]["invocation"]
        );
        assert!(reminders[0]["params"]["content"]
            .as_str()
            .unwrap()
            .contains("mcp__maxplayer__answer"));
        let _ = serve.kill();
        let _ = serve.wait();
        drop(reader);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_takes_one_entry_and_home() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse(&args(&["test", "x", "--home", "/h"])).unwrap(),
            Options {
                entry: "x".into(),
                home: Some("/h".into())
            }
        );
        assert!(parse(&args(&["test"])).is_err());
        assert!(parse(&args(&["run", "x"])).is_err());
        assert!(parse(&args(&["test", "x", "y"])).is_err());
    }
}
