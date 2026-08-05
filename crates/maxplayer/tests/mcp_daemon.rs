//! Connect-or-spawn + daemon-owned money path (the #126/#127/#131 acceptance).
//!
//! A fresh home with ONLY the MCP/CLI configured — zero manual daemon commands — must transparently
//! start the buyer daemon on first use, reuse the same daemon for later sessions, and serve every
//! money op through it (never in-process). These drive the real `maxplayer` binary.
//!
//! Relay is pinned to a dead loopback (`MAXPLAYER_RELAY_URL`) so a routed trade op fails fast and the
//! test stays hermetic — the point under test is the daemon boundary, not a live trade.
#![cfg(all(unix, feature = "wallet"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp_home(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("maxplayer-mcp-daemon-{label}-{}-{id}", std::process::id()))
}

/// A `maxplayer` command pinned to `home` with a dead relay (fast, network-free relay failures).
fn maxplayer(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    command
        .env("MOBEE_HOME", home)
        .env("MAXPLAYER_RELAY_URL", "ws://127.0.0.1:1");
    command
}

/// Run `maxplayer <args>` to completion; returns (exit code, stdout, stderr).
fn run(home: &Path, args: &[&str]) -> (i32, String, String) {
    let output = maxplayer(home).args(args).output().expect("spawn maxplayer");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The daemon's `status` result object, or `None` while no daemon answers.
fn status(home: &Path) -> Option<Value> {
    let (code, stdout, _stderr) = run(home, &["buyer", "status"]);
    if code != 0 {
        return None;
    }
    let response: Value = serde_json::from_str(stdout.trim()).ok()?;
    response.get("result").cloned()
}

/// Poll `buyer status` until a daemon answers or the deadline passes.
fn wait_for_daemon(home: &Path) -> Option<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(result) = status(home) {
            return Some(result);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Terminate a daemon by pid (test teardown; exact-pid, never a broad pattern kill).
fn kill(pid: u64) {
    let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
}

/// No daemon → a routed CLI money op connect-or-spawns one → a later session reuses the SAME daemon,
/// and a direct second `buyer serve` fails closed on the exclusive home lock.
#[test]
fn connect_or_spawn_starts_then_reuses_one_daemon() {
    let home = temp_home("spawn");
    let _ = std::fs::remove_dir_all(&home);

    // Nothing running yet: the thin-client status has no daemon to talk to.
    assert!(status(&home).is_none(), "no daemon should be running before first use");

    // A routed money op (collect) must start the daemon under the hood — zero manual commands. The
    // collect itself fails (bogus job, dead relay), but the daemon must come up to serve it.
    let (collect_code, _out, _err) = run(&home, &["collect", &"a".repeat(64)]);
    assert_ne!(collect_code, 0, "collect on a bogus job must fail");

    let first = wait_for_daemon(&home).expect("connect-or-spawn must start the daemon");
    let pid = first["pid"].as_u64().expect("status carries a pid");
    let pubkey = first["pubkey"].as_str().expect("status carries a pubkey").to_owned();
    assert!(home.join("buyer.sock").exists(), "daemon bound its socket");

    // A second session finds the daemon already serving and reuses it — same pid, same identity.
    let second = wait_for_daemon(&home).expect("daemon still serving");
    assert_eq!(second["pid"].as_u64().unwrap(), pid, "second session reuses the same daemon");
    assert_eq!(second["pubkey"].as_str().unwrap(), pubkey, "same daemon identity");

    // The exclusive home lock holds: a direct second `buyer serve` fails closed rather than becoming
    // a second money owner.
    let mut second_serve = maxplayer(&home)
        .args(["buyer", "serve"])
        .spawn()
        .expect("spawn second buyer serve");
    let exit = wait_child(&mut second_serve, Duration::from_secs(8));
    assert!(
        matches!(exit, Some(status) if !status.success()),
        "a second `buyer serve` on the same home must fail closed on the lock"
    );

    kill(pid);
    let _ = std::fs::remove_dir_all(&home);
}

/// A money op is served by the daemon, never in-process: after a routed collect the daemon owns the
/// home lock + socket, and a collect with no delivery burns nothing (no payment journal / results).
#[test]
fn money_op_is_served_by_the_daemon_never_in_process() {
    let home = temp_home("served");
    let _ = std::fs::remove_dir_all(&home);

    // Before: no daemon artifacts at all.
    assert!(!home.join("buyer.lock").exists(), "no lock before first use");
    assert!(!home.join("buyer.sock").exists(), "no socket before first use");

    // A CLI collect on an undelivered job routes to the daemon and fails (nothing to pay).
    let job = "b".repeat(64);
    let (code, stdout, _stderr) = run(&home, &["collect", &job]);
    assert_ne!(code, 0, "collect with no delivery must fail");

    // It was SERVED BY THE DAEMON, not run in-process: a daemon now owns the home lock + socket. Had
    // collect opened the wallet in-process, no daemon (and no held lock) would exist afterward.
    let result = wait_for_daemon(&home).expect("the money op brought up a daemon to serve it");
    let pid = result["pid"].as_u64().expect("pid");
    assert!(home.join("buyer.lock").exists(), "daemon holds the exclusive home lock");
    assert!(home.join("buyer.sock").exists(), "daemon owns the socket");

    // The failed collect burned nothing: no payment journal, no materialized results.
    assert!(
        !home.join("payment-journal").exists(),
        "a failed collect must write no payment journal (zero spend)"
    );
    assert!(
        !home.join("results").join(&job).exists(),
        "a failed collect must materialize nothing"
    );

    // Never-echo: the buyer secret must not appear on the CLI output.
    let secret_path = home.join("key");
    if let Ok(secret) = std::fs::read_to_string(&secret_path) {
        let secret = secret.trim();
        if !secret.is_empty() {
            assert!(!stdout.contains(secret), "collect output must not echo the secret key");
        }
    }

    kill(pid);
    let _ = std::fs::remove_dir_all(&home);
}

/// Wait up to `timeout` for a child to exit; returns its status, or `None` if still running (then
/// kills it so it never leaks).
fn wait_child(child: &mut std::process::Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}
