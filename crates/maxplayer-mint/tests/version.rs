//! `maxplayer-mint --version` / `version`: the line the release verifies on every shipped artifact
//! (`scripts/verify-release-version.sh`), so it must keep the `maxplayer` grammar and answer without
//! a home directory (the release's portability check runs it in bare alpine/debian containers).

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_maxplayer-mint"))
        .args(args)
        // No home at all: `version` must not resolve or touch one.
        .env_remove("MAXPLAYER_HOME")
        .env_remove("HOME")
        .output()
        .expect("run maxplayer-mint")
}

#[test]
fn both_version_forms_print_the_release_line_without_a_home() {
    let expected = format!(
        "maxplayer-mint {} ({})\n",
        env!("CARGO_PKG_VERSION"),
        env!("MINT_SIDECAR_BUILD")
    );
    for form in ["--version", "version"] {
        let out = run(&[form]);
        assert!(
            out.status.success(),
            "{form} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{form}");
    }
}

/// `run` prints `<version line> — mint starting, dir <home>/mint` as its FIRST stderr line, before
/// anything that can fail: an operator who started it by bare name with an older binary first on
/// `PATH` reads from the log which build answered. Driven on an uninitialised home, so the start is
/// refused right after the line (no relay needed) and the refusal still follows it.
#[test]
fn run_prints_its_version_as_the_first_stderr_line() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-mint"))
        .arg("run")
        .env("MAXPLAYER_HOME", home.path())
        .output()
        .expect("run maxplayer-mint");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert_eq!(
        stderr.lines().next().unwrap_or_default(),
        format!(
            "maxplayer-mint {} ({}) — mint starting, dir {}",
            env!("CARGO_PKG_VERSION"),
            env!("MINT_SIDECAR_BUILD"),
            home.path().join("mint").display()
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("maxplayer-mint init"),
        "refusal must follow: {stderr}"
    );
}

#[test]
fn an_unknown_subcommand_exits_nonzero() {
    // The release portability check's negative control relies on this.
    // A home is given so the usage arm, not the missing-home error, is what answers.
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-mint"))
        .arg("not-a-subcommand")
        .env("MAXPLAYER_HOME", home.path())
        .output()
        .expect("run maxplayer-mint");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: maxplayer-mint"));
}
