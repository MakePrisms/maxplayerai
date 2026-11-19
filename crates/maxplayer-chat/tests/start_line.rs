//! `maxplayer-chat watch` prints `maxplayer-chat <version> — chat starting, home <dir>` as its FIRST
//! stderr line, before the home is opened, so an operator reads from the log which build is running
//! even when the start fails. Driven on a home that is a regular file: the open fails right after the
//! line, with no relay needed.

use std::process::Command;

#[test]
fn watch_prints_its_version_as_the_first_stderr_line() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("not-a-dir");
    std::fs::write(&home, b"file").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-chat"))
        .arg("--home")
        .arg(&home)
        .arg("watch")
        .output()
        .expect("run maxplayer-chat");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    let mut lines = stderr.lines();
    assert_eq!(
        lines.next().unwrap_or_default(),
        format!(
            "maxplayer-chat {} — chat starting, home {}",
            env!("CARGO_PKG_VERSION"),
            home.display()
        ),
        "{stderr}"
    );
    assert!(
        lines
            .next()
            .is_some_and(|l| l.starts_with("maxplayer-chat: ")),
        "the failure must follow the start line: {stderr}"
    );
}

#[test]
fn one_shot_commands_print_no_start_line() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-chat"))
        .arg("--home")
        .arg(dir.path())
        .arg("whoami")
        .output()
        .expect("run maxplayer-chat");
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("starting"),
        "only `watch` is a daemon"
    );
}
