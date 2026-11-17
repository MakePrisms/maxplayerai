//! The buyer daemon hosts trade's `serve` (`[trade] serve`, default on), driven through the real
//! `maxplayer` binary. Hermetic: the buyer relay and the trade market relay are dead loopbacks, so
//! nothing leaves the box and no mint is contacted (a home with no trades makes no mint call).
//!
//! - a home that never traded: the daemon creates nothing of trade's (bootstrap rule);
//! - a trading home: the daemon owns `owner.lock`, binds `<home>/trade/serve.sock`, CLI commands
//!   hand off to it with the home's `[trade] relays`, a standalone `maxplayer trade serve` is
//!   refused, and SIGTERM is trade's safe stop (final `exited` line, exit 5, socket removed);
//! - `[trade] serve = false`: the daemon leaves serving to `maxplayer trade serve`.
#![cfg(all(unix, feature = "wallet"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DEAD_RELAY: &str = "ws://127.0.0.1:1";

/// `maxplayer` pinned to `home`, with every inherited `MAXPLAYER_*` variable removed.
fn maxplayer(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_maxplayer"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MAXPLAYER_") {
            command.env_remove(key);
        }
    }
    command
        .env("MAXPLAYER_HOME", home)
        .env("MAXPLAYER_RELAY_URL", DEAD_RELAY)
        .stdin(Stdio::null());
    command
}
fn run(home: &Path, args: &[&str]) -> (Option<i32>, String) {
    let out = maxplayer(home)
        .args(args)
        .output()
        .expect("spawn maxplayer");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}
/// A fresh directory under the system temp dir, removed on drop (no new dev-dependency).
struct TempHome(PathBuf);
impl TempHome {
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("daemon.log"));
    }
}
fn temp_home(label: &str) -> TempHome {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "mp-trade-hosted-{label}-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    TempHome(path)
}
/// A maxplayer home that trades: the first trade command bootstraps it and creates the journal.
/// Its market relay is the dead loopback (`[trade] relays`).
fn trading_home(label: &str, serve: bool) -> TempHome {
    let dir = temp_home(label);
    let (code, stderr) = run(dir.path(), &["trade", "recover"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(dir.path().join("trade/trade.sqlite").exists());
    let cfg = dir.path().join("config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        "\n[trade]\nserve = {serve}\nrelays = [\"{DEAD_RELAY}\"]\n"
    ));
    std::fs::write(&cfg, text).unwrap();
    dir
}
struct Daemon {
    child: Child,
    log: PathBuf,
}
impl Daemon {
    fn start(home: &Path) -> Self {
        Self::start_env(home, &[])
    }
    fn start_env(home: &Path, env: &[(&str, &str)]) -> Self {
        let log = home.with_extension("daemon.log");
        let child = maxplayer(home)
            .envs(env.iter().copied())
            .args(["buyer", "serve"])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let d = Self { child, log };
        // Ready means it owns the home and answers (the host is started by then); "online" is
        // printed before the lock is taken.
        let start = Instant::now();
        while run(home, &["buyer", "status"]).0 != Some(0) {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "daemon not up:\n{}",
                d.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        d
    }
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn until(&self, what: &str, secs: u64, f: impl Fn(&str) -> bool) {
        let start = Instant::now();
        while !f(&self.log()) {
            assert!(
                start.elapsed() < Duration::from_secs(secs),
                "{what}:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn term(&mut self) -> Option<i32> {
        let pid = self.child.id().to_string();
        Command::new("kill")
            .args(["-s", "TERM", &pid])
            .status()
            .unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "daemon did not exit:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn lifecycle(log: &str) -> Vec<serde_json::Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("serve").is_some())
        .collect()
}

/// SAFETY (bootstrap rule, lazy start): a buyer daemon on a home that never traded creates nothing
/// of trade's (no `trade/` directory, journal, `trade.key`, `owner.lock` or socket) and prints
/// nothing about it.
#[test]
fn a_buyer_daemon_on_a_home_that_never_traded_creates_no_trade_state() {
    let dir = temp_home("fresh");
    let mut daemon = Daemon::start(dir.path());
    std::thread::sleep(Duration::from_secs(6)); // past the host's 5 s poll
    assert!(
        !dir.path().join("trade").exists(),
        "SAFETY: the daemon created trade state on a non-trading home:\n{}",
        daemon.log()
    );
    assert!(lifecycle(&daemon.log()).is_empty(), "{}", daemon.log());
    // No session ever started: TERM keeps the daemon's old disposition (killed by the signal).
    assert_eq!(daemon.term(), None);
}

/// Lazy start, re-check: a buyer daemon already running on a home that never traded starts serving
/// once the home's first trade command creates the journal, with no daemon restart.
#[test]
fn a_running_buyer_daemon_starts_serving_once_the_home_starts_trading() {
    let dir = temp_home("later");
    let home = dir.path();
    let mut daemon = Daemon::start_env(home, &[("MAXPLAYER_TRADE__RELAYS", DEAD_RELAY)]);
    std::thread::sleep(Duration::from_secs(6)); // at least one re-check while not trading
    assert!(!home.join("trade").exists(), "{}", daemon.log());
    // The first trading command (it takes owner.lock and creates the journal; the daemon holds
    // nothing of trade's yet, so it is not refused).
    let (code, stderr) = run(home, &["trade", "recover"]);
    assert_eq!(code, Some(0), "{stderr}");
    daemon.until("served without a restart", 30, |log| {
        lifecycle(log)
            .iter()
            .any(|v| v["serve"] == "started" && v["host"] == "buyer daemon")
    });
    let start = Instant::now();
    while !home.join("trade/serve.sock").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "socket:\n{}",
            daemon.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(daemon.term(), Some(5), "{}", daemon.log());
    assert!(!home.join("trade/serve.sock").exists());
}

/// A trading home: the buyer daemon hosts serve (startup line names it), owns `owner.lock`, takes a
/// CLI hand-off using the home's `[trade] relays`, refuses a standalone `serve`, and stops through
/// trade's safe stop on SIGTERM: exactly one final line, exit 5, socket removed, home released.
#[test]
fn the_buyer_daemon_hosts_trade_serve_on_a_trading_home() {
    let dir = trading_home("trading", true);
    let home = dir.path();
    let mut daemon = Daemon::start(home);
    daemon.until("hosted serve socket", 30, |log| {
        log.contains("\"serve_socket\"")
    });
    let started = lifecycle(&daemon.log())
        .into_iter()
        .find(|v| v["serve"] == "started")
        .unwrap();
    assert_eq!(started["host"], "buyer daemon");
    assert_eq!(started["relays"], serde_json::json!([DEAD_RELAY]));
    assert!(home.join("trade/serve.sock").exists());

    let (code, stderr) = run(home, &["trade", "cancel", &"ab".repeat(32)]);
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert!(stderr.contains("unknown local lot"), "{stderr}");
    assert_eq!(code, Some(1), "{stderr}");

    let (code, stderr) = run(home, &["trade", "serve"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("trade serve is already running"),
        "SAFETY: one serve per home: {stderr}"
    );

    assert_eq!(
        daemon.term(),
        Some(5),
        "trade's safe-stop exit code:\n{}",
        daemon.log()
    );
    let log = daemon.log();
    let exited: Vec<_> = lifecycle(&log)
        .into_iter()
        .filter(|v| v["serve"] == "exited")
        .collect();
    assert_eq!(exited.len(), 1, "exactly one final line:\n{log}");
    assert_eq!(exited[0]["exit"], 5, "{log}");
    assert_eq!(exited[0]["reason"], "signal", "{log}");
    assert_eq!(exited[0]["signal"], "SIGTERM", "{log}");
    assert!(!home.join("trade/serve.sock").exists());
    // The home is free again: a writer runs.
    let (code, stderr) = run(home, &["trade", "recover"]);
    assert_eq!(code, Some(0), "home released: {stderr}");
}

/// `[trade] serve = false`: the daemon hosts nothing, even on a trading home.
#[test]
fn trade_serve_false_leaves_serving_to_the_standalone_command() {
    let dir = trading_home("off", false);
    let daemon = Daemon::start(dir.path());
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !dir.path().join("trade/serve.sock").exists(),
        "{}",
        daemon.log()
    );
    assert!(lifecycle(&daemon.log()).is_empty(), "{}", daemon.log());
}

/// The buyer daemon fails right after it started hosting (its `buyer.sock` path is taken by a
/// non-empty directory, so the socket cannot bind): the hosted serve is stopped safely, never left
/// running unattended until the process exits (review of #1334, M1).
#[test]
fn a_buyer_daemon_that_fails_after_starting_the_host_stops_it_safely() {
    let dir = trading_home("bindfail", true);
    let home = dir.path();
    std::fs::create_dir(home.join("buyer.sock")).unwrap();
    std::fs::write(home.join("buyer.sock/keep"), b"x").unwrap();
    let out = maxplayer(home).args(["buyer", "serve"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        !home.join("trade/serve.sock").exists(),
        "SAFETY: socket left behind:\n{stderr}"
    );
    let lines = lifecycle(&stderr);
    let started = lines.iter().filter(|v| v["serve"] == "started").count();
    let exited = lines.iter().filter(|v| v["serve"] == "exited").count();
    assert_eq!(
        started, exited,
        "every started session has its final line:\n{stderr}"
    );
}
