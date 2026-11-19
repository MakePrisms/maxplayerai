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

/// `serve` (and its deprecated alias `run`) prints `<version line> — mint starting, dir <home>/mint`
/// as its FIRST stderr line, before anything that can fail: an operator who started it by bare name
/// with an older binary first on `PATH` reads from the log which build answered. Driven on an
/// uninitialised home, so the start is refused right after the line (no relay needed) and the
/// refusal still follows it.
#[test]
fn serve_prints_its_version_as_the_first_stderr_line() {
    let home = tempfile::tempdir().unwrap();
    for verb in ["serve", "run"] {
        let out = Command::new(env!("CARGO_BIN_EXE_maxplayer-mint"))
            .arg(verb)
            .env("MAXPLAYER_HOME", home.path())
            .output()
            .expect("run maxplayer-mint");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{verb}: {stderr}");
        assert_eq!(
            stderr.lines().next().unwrap_or_default(),
            format!(
                "maxplayer-mint {} ({}) — mint starting, dir {}",
                env!("CARGO_PKG_VERSION"),
                env!("MINT_SIDECAR_BUILD"),
                home.path().join("mint").display()
            ),
            "{verb}: {stderr}"
        );
        assert!(
            stderr.contains("maxplayer-mint init"),
            "{verb}: refusal must follow: {stderr}"
        );
    }
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

/// `serve` is the start verb and `run` its deprecated alias: both reach the SAME serving path (here
/// it stops at the missing mint, with the same error), `run` prints exactly one deprecation line
/// naming `maxplayer-mint serve`, right after the version start line (#1354: the version is always
/// first), and `serve` prints none. The usage lists `serve` as the command and `run` only as the
/// alias.
#[test]
fn serve_is_the_start_verb_and_run_a_deprecated_alias() {
    let home = tempfile::tempdir().unwrap();
    let start = |verb: &str| {
        Command::new(env!("CARGO_BIN_EXE_maxplayer-mint"))
            .arg(verb)
            .env("MAXPLAYER_HOME", home.path())
            .output()
            .expect("run maxplayer-mint")
    };
    let serve = start("serve");
    let run = start("run");
    let serve_err = String::from_utf8_lossy(&serve.stderr).into_owned();
    let run_err = String::from_utf8_lossy(&run.stderr).into_owned();
    // No <home>/mint/: both fail the same way inside `serve`, not at the usage arm.
    assert!(!serve.status.success() && !run.status.success());
    assert!(
        !serve_err.contains("usage:"),
        "serve reached the usage arm: {serve_err}"
    );
    assert!(
        !run_err.contains("usage:"),
        "run reached the usage arm: {run_err}"
    );
    assert!(serve_err.contains("maxplayer-mint init"), "{serve_err}");
    assert!(!serve_err.contains("deprecated"), "{serve_err}");
    // Version start line first (the same for both), the deprecation second.
    assert_eq!(
        run_err.lines().next(),
        serve_err.lines().next(),
        "{run_err}"
    );
    assert!(
        run_err
            .lines()
            .next()
            .is_some_and(|l| l.contains(" — mint starting, dir ")),
        "{run_err}"
    );
    assert_eq!(
        run_err.lines().nth(1),
        Some(
            "maxplayer-mint: `maxplayer-mint run` is deprecated and still works; use `maxplayer-mint serve`"
        ),
        "{run_err}"
    );
    assert_eq!(
        run_err.lines().filter(|l| l.contains("deprecated")).count(),
        1,
        "{run_err}"
    );
    // Without that one line, `run` says exactly what `serve` says.
    assert_eq!(
        run_err
            .lines()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, l)| l)
            .collect::<Vec<_>>(),
        serve_err.lines().collect::<Vec<_>>()
    );

    let usage = Command::new(env!("CARGO_BIN_EXE_maxplayer-mint"))
        .arg("not-a-subcommand")
        .env("MAXPLAYER_HOME", home.path())
        .output()
        .unwrap();
    let usage = String::from_utf8_lossy(&usage.stderr);
    assert!(
        usage.contains("|serve|") && usage.contains("\n  serve "),
        "{usage}"
    );
    assert!(
        usage.contains("\n  run             deprecated alias for `serve`"),
        "{usage}"
    );
}
