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

/// How long a test `job` call may run: the deadline the event states is this far ahead, and the
/// cutoff is that minus the publish margin, exactly as for a real job.
const TEST_JOB_SECS: u64 = 300;
const SAMPLE_INPUT: &str = "sample.txt";
const SAMPLE_INPUT_TEXT: &str = "maxplayer seller hook test: a sample input file.\n";
const TURN1_PROMPT: &str = "This is `maxplayer seller hook test`, not a buyer. Read inputs/sample.txt and reply with one short sentence saying what it contains.";
const TURN2_PROMPT: &str =
    "Follow-up from the same test: repeat your previous answer in five words or fewer.";

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
    let passed = run_steps(&argv, &home.root, out);
    if passed {
        let _ = writeln!(out, "\nPASS: the daemon would serve this hook.");
        SUCCESS
    } else {
        let _ = writeln!(
            out,
            "\nFAIL: fix the steps marked FAIL; the daemon would strike or drop this entry."
        );
        FAILED
    }
}

/// Every step, in the daemon's order. Returns whether all passed.
fn run_steps(argv: &[String], home_root: &Path, out: &mut dyn Write) -> bool {
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
            return false;
        }
    };
    // 2. job, turn 1, with an input file. 3. job, turn 2 of the same job, a new invocation.
    for (turn, invocation, prompt) in [
        (1u32, job_id.clone(), TURN1_PROMPT),
        (2, random_hex(), TURN2_PROMPT),
    ] {
        passed &= job_step(out, argv, &dir, &job_id, &invocation, turn, prompt);
    }
    let _ = std::fs::remove_dir_all(&dir);

    // 4. close: best effort, exit status ignored by the daemon; reported here for the operator.
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

    // 5. an event this contract version does not define: exit 64 means "unsupported", never a strike.
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
            "{}; a hook must exit 64 on an event it does not know, so later contract versions can add events",
            describe_exit(other)
        )),
    };
    passed &= step(out, "unknown event", started, failure.as_deref(), "exit 64");
    passed
}

fn job_step(
    out: &mut dyn Write,
    argv: &[String],
    dir: &Path,
    job_id: &str,
    invocation: &str,
    turn: u32,
    prompt: &str,
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
        true,
        prompt,
        Some("text/plain"),
        Some(&dir.join("inputs")),
        deadline.saturating_sub(hook::PUBLISH_MARGIN_SECS),
    );
    let started = Instant::now();
    let report = hook::run(argv, &event, dir, hook::job_cutoff(deadline, now, started));
    let label = format!("job turn {turn}");
    if !report.stderr_tail.trim().is_empty() {
        let _ = writeln!(
            out,
            "      {label} stderr tail: {}",
            report.stderr_tail.trim().replace('\n', " | ")
        );
    }
    let (failure, verdict) = verdict(&report, cap);
    step(out, &label, started, failure.as_deref(), &verdict)
}

/// The daemon's verdict on a `job` call (§5.1 "Outcomes"): `(None, what it would do)` when it would
/// deliver, `(Some(why), …)` when it would fail the job.
fn verdict(report: &RunReport, cap: usize) -> (Option<String>, String) {
    match hook::classify_job(report, cap) {
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
                "still running at the cutoff ({} s): killed, execution_failed",
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
  *'"event":"job"'*) dir=$(printf '%s' "$line" | sed 's/.*"inputs_dir":"\([^"]*\)".*/\1/'); test -f "$dir/sample.txt" || exit 3
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
resume=none; prev=
for a in "$@"; do [ "$prev" = "--resume" ] && resume="$a"; prev="$a"; done
cat >/dev/null
echo x >> '{counter}'; n=$(wc -l < '{counter}' | tr -d ' ')
jq -nc --arg r "turn answer (resumed: $resume)" --arg s "sess-$n" '{{type:"result",is_error:false,result:$r,session_id:$s}}'
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
        let left: Vec<_> = std::fs::read_dir(&state).unwrap().collect();
        assert!(
            left.is_empty(),
            "close removed the job's state file: {left:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The shipped channel adapter: `serve` driven by a fake Claude (an MCP client over stdio that
    /// answers every channel event through the `answer` tool), `push` as the hook. Every step passes;
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
                    let turn = meta["turn"].as_str().unwrap_or("?").to_owned();
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
                    } else {
                        first_invocation = Some(invocation.to_owned());
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
        std::thread::sleep(Duration::from_millis(200));

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
