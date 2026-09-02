use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use maxplayer_core::Envelope;
use maxplayer_core::driver::{ContentBlock, ScriptedSession, SessionUpdate, StopReason};
use maxplayer_core::event::{Event, JobExecutionStatus, JobId, RuntimeId};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn mock_run_then_log_replay_round_trips_envelope_payloads() {
    let script = test_path("e2e-script");
    let log = test_path("e2e-log");
    write_script(
        &script,
        ScriptedSession {
            session_id: "session-1".into(),
            updates: vec![
                SessionUpdate::AgentMessage(vec![ContentBlock::Text {
                    text: "working".into(),
                }]),
                SessionUpdate::TurnEnded(StopReason::Completed),
            ],
            artifacts: Vec::new(),
        },
    );

    let run_output = Command::new(env!("CARGO_BIN_EXE_maxplayer"))
        .args([
            "mock",
            "run",
            "--script",
            script.to_str().unwrap(),
            "--log",
            log.to_str().unwrap(),
        ])
        .output()
        .expect("run maxplayer mock");
    assert!(
        run_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&run_output.stderr)
    );

    let replay_output = Command::new(env!("CARGO_BIN_EXE_maxplayer"))
        .args(["log", "replay", log.to_str().unwrap()])
        .output()
        .expect("run maxplayer replay");
    assert!(
        replay_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&replay_output.stderr)
    );

    let envelopes = String::from_utf8(replay_output.stdout)
        .expect("stdout utf8")
        .lines()
        .map(|line| serde_json::from_str::<Envelope>(line).expect("envelope"))
        .collect::<Vec<_>>();
    assert_eq!(
        envelopes
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
            Event::AgentMessage {
                job_id: JobId("job-1".into()),
                text: "working".into()
            },
            Event::JobExecutionChanged {
                job_id: JobId("job-1".into()),
                status: JobExecutionStatus::Completed
            },
        ]
    );
}

/// The exit codes a stranger meets first. Asserted against the real executable because the
/// process exit status is what scripts probe, and `cli::run`'s return value only becomes one in
/// `main`.
#[test]
fn help_and_version_flags_exit_zero_on_the_real_binary() {
    let help = Command::new(env!("CARGO_BIN_EXE_maxplayer"))
        .arg("--help")
        .output()
        .expect("run maxplayer --help");
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));

    let version = Command::new(env!("CARGO_BIN_EXE_maxplayer"))
        .arg("--version")
        .output()
        .expect("run maxplayer --version");
    assert_eq!(version.status.code(), Some(0));
    // #818: `maxplayer <version> (<stamp>)`. This is the only assertion in the repo that measures
    // the BUILT ARTIFACT's version line rather than the library path, which makes it the one that
    // can see a build script that failed to stamp — so it holds the stamp's shape, not merely the
    // program name. The stamp itself is not compared to a literal: an integration test links the
    // library, not the binary crate, so it cannot reach `build_stamp::build_commit()`, and the
    // build script legitimately answers `unknown` where no `.git` and no `MAXPLAYER_BUILD_COMMIT`
    // exist. 40 lowercase hex or `unknown`, and nothing else — the class #818 measured (a plausible
    // 40-hex string, or the `0000111122223333…` noise beside it) cannot come from a padded, zeroed
    // or truncated value. That the sha RESOLVES to a commit is `scripts/verify-release-version.sh`.
    let reported = String::from_utf8_lossy(&version.stdout);
    let stamp = reported
        .strip_prefix(&format!("maxplayer {} (", maxplayer_core::version()))
        .and_then(|rest| rest.strip_suffix(")\n"))
        .unwrap_or_else(|| panic!("`{reported}` is not `maxplayer <version> (<stamp>)`"));
    assert!(
        stamp == "unknown"
            || (stamp.len() == 40
                && stamp
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))),
        "build stamp `{stamp}` is neither `unknown` nor a 40-character lowercase-hex sha"
    );

    let no_args = Command::new(env!("CARGO_BIN_EXE_maxplayer"))
        .output()
        .expect("run maxplayer");
    assert_eq!(no_args.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&no_args.stderr).contains("Usage:"));
}

fn write_script(path: &Path, script: ScriptedSession) {
    fs::write(path, serde_json::to_vec(&script).expect("encode script")).expect("write script");
}

fn test_path(name: &str) -> PathBuf {
    let id = NEXT_TEST_ID.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "maxplayer-cli-{name}-{}-{id}.jsonl",
        std::process::id()
    ))
}
