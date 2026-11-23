//! Every long-running `maxplayer` command prints its version as the FIRST line of its log (stderr),
//! before anything that can fail: `maxplayer <semver> (<stamp>) — <role> starting, <where>`.
//!
//! Petar (2026-11-19) started a daemon by bare name with an older binary first on `PATH` and could
//! not tell from its output which build was serving. Each start here is made to fail right after the
//! line (the home is a regular file, the reviewer config is missing), so the test needs no relay,
//! mint or harness and still proves the line comes first: a failed start is the case where knowing
//! the binary matters most. A booted buyer daemon is covered in `tests/trade_hosted.rs`.
#![cfg(all(unix, feature = "wallet"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `maxplayer` with every inherited `MAXPLAYER_*` variable removed (the binary refuses unknown ones).
fn maxplayer() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MAXPLAYER_") {
            command.env_remove(key);
        }
    }
    command.stdin(Stdio::null());
    command
}

/// A path that exists as a regular FILE, so bootstrapping a home there fails right away.
struct NotADir(PathBuf);
impl NotADir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mp-start-line-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"not a directory").unwrap();
        Self(path)
    }
}
impl Drop for NotADir {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs `command` and asserts its FIRST stderr line is the start line for `role` at `place`.
fn assert_first_line(mut command: Command, role: &str, place: &str) {
    let out = command.output().expect("run maxplayer");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(0),
        "start must fail here:\n{stderr}"
    );
    let first = stderr.lines().next().unwrap_or_default();
    let prefix = format!("maxplayer {} (", maxplayer_core::version());
    let suffix = format!(") — {role} starting, {place}");
    assert!(
        first.starts_with(&prefix) && first.ends_with(&suffix),
        "first stderr line is not `{prefix}<stamp>{suffix}`:\n{stderr}"
    );
    assert!(
        stderr.lines().count() > 1,
        "the failure must still be reported after the start line:\n{stderr}"
    );
}

fn home_place(path: &Path) -> String {
    format!("home {}", path.display())
}

#[test]
fn buyer_serve_prints_its_version_first() {
    for args in [&["buyer", "serve"][..], &["buyer"][..]] {
        let home = NotADir::new("buyer");
        let mut command = maxplayer();
        command.args(args).env("MAXPLAYER_HOME", &home.0);
        assert_first_line(command, "buyer", &home_place(&home.0));
    }
}

#[cfg(feature = "acp")]
#[test]
fn seller_prints_its_version_first() {
    // `seller serve`, and the deprecated verbless `seller` (its deprecation line comes second).
    for args in [&["seller", "serve"][..], &["seller"][..]] {
        let home = NotADir::new("seller");
        let mut command = maxplayer();
        command.args(args).arg("--home").arg(&home.0);
        assert_first_line(command, "seller", &home_place(&home.0));
    }
}

#[test]
fn reviewer_serve_prints_its_version_first() {
    let missing = std::env::temp_dir().join(format!(
        "mp-start-line-reviewer-{}-missing.json",
        std::process::id()
    ));
    let mut command = maxplayer();
    command.args(["reviewer", "serve"]).arg(&missing);
    assert_first_line(
        command,
        "reviewer",
        &format!("config {}", missing.display()),
    );
}

#[test]
fn mcp_prints_its_version_first_on_stderr_not_on_the_json_rpc_stream() {
    let home = NotADir::new("mcp");
    let out = maxplayer()
        .arg("mcp")
        .env("MAXPLAYER_HOME", &home.0)
        .output()
        .expect("run maxplayer mcp");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("starting"),
        "stdout is the JSON-RPC stream and must not carry the start line"
    );
    let mut command = maxplayer();
    command.arg("mcp").env("MAXPLAYER_HOME", &home.0);
    assert_first_line(command, "mcp", &home_place(&home.0));
}

/// A daemon started from an agent's shell inherited `HTTPS_PROXY=http://<user>:<token>@127.0.0.1:<port>`
/// (the agent's secret-egress proxy); after the agent's run ended every request failed with 407 and
/// nothing said why. The start must warn once, after the version line, and never print the token.
#[test]
fn a_credentialed_local_proxy_is_warned_about_once_without_its_credential() {
    let mut daemons: Vec<(&str, &[&str], bool)> = vec![
        ("buyer", &["buyer", "serve"], false),
        ("mcp", &["mcp"], false),
    ];
    if cfg!(feature = "acp") {
        daemons.push(("seller", &["seller", "serve"], true));
    }
    for (role, args, home_flag) in daemons {
        let home = NotADir::new(role);
        let start = |proxy: Option<&str>| {
            let mut command = maxplayer();
            command.args(args);
            if home_flag {
                command.arg("--home").arg(&home.0);
            } else {
                command.env("MAXPLAYER_HOME", &home.0);
            }
            for name in [
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
            ] {
                command.env_remove(name);
            }
            if let Some(proxy) = proxy {
                command.env("HTTPS_PROXY", proxy);
            }
            let out = command.output().expect("run maxplayer");
            // stdout is `mcp`'s JSON-RPC stream: the warning never goes there.
            assert!(
                !String::from_utf8_lossy(&out.stdout).contains("requires credentials"),
                "{role}: warning on stdout"
            );
            String::from_utf8_lossy(&out.stderr).into_owned()
        };
        let stderr = start(Some("http://openclaw:secret@127.0.0.1:1"));
        assert!(
            !stderr.contains("secret"),
            "{role}: the credential leaked:\n{stderr}"
        );
        let warnings: Vec<_> = stderr
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains("requires credentials"))
            .collect();
        assert_eq!(warnings.len(), 1, "{role}: exactly one warning:\n{stderr}");
        let (at, line) = warnings[0];
        assert!(
            at >= 1,
            "{role}: the version start line stays first:\n{stderr}"
        );
        assert!(
            line.contains("HTTPS_PROXY")
                && line.contains("127.0.0.1:1")
                && line.contains("407")
                && line.contains("NO_PROXY"),
            "{role}: {line}"
        );
        let quiet = start(None);
        assert!(
            !quiet.contains("requires credentials"),
            "{role}: no proxy, no warning:\n{quiet}"
        );
    }
}
