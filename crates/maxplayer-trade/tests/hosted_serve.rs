//! Trade serve hosted inside a daemon (`maxplayer_trade::host`, used by `maxplayer buyer` and
//! `maxplayer seller`). Every rule the PR settles has a test here that fails when the rule goes:
//!
//! - a home that does not trade gets nothing created (Petar's 2026-11-12 bootstrap rule), and a
//!   legacy or mixed home is never served;
//! - one serve per home: the host holds `owner.lock`; a standalone `serve` is refused with the
//!   reason; a host that finds the lock held stands by and takes over once it is free;
//! - `list`/`take`/`cancel` hand off to the hosted serve over `serve.sock` (protocol v5), and
//!   without `--relay` they use the home's `[trade] relays`, which the host serves on;
//! - an error ends the session, not the host: it restarts after a backoff;
//! - the daemon's shutdown keeps trade's safe stop: a money step at the mint finishes and is
//!   journaled before the host lets the daemon exit, and the home is released afterwards.
//!
//! Fake mints, a loopback relay and disposable homes only. The shutdown state is process-wide, so
//! the tests take [`SERIAL`] and reset it.
mod support;
use fs2::FileExt;
#[cfg(not(feature = "lab"))]
use maxplayer_trade::coordinator::{self, Swap};
use maxplayer_trade::{host, journal::Journal, shutdown, wallet};
#[cfg(not(feature = "lab"))]
use std::sync::atomic::Ordering::SeqCst;
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use support::*;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_maxplayer-trade");
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn options(relays: &[&str]) -> host::Options {
    let mut o = host::Options::new(
        "test",
        relays.iter().map(|r| r.to_string()).collect(),
        vec![],
    );
    o.poll = Duration::from_millis(100);
    o.restart_min = Duration::from_millis(100);
    o.restart_max = Duration::from_secs(1);
    o.signals = false;
    o
}
fn cmd(home: &Path, relay: Option<&str>, args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.arg("--home").arg(home);
    if let Some(relay) = relay {
        c.args(["--relay", relay]);
    }
    c.args(args).stdin(Stdio::null()).kill_on_drop(true);
    c
}
async fn output(mut c: Command) -> (Option<i32>, String) {
    let o = tokio::time::timeout(Duration::from_secs(120), c.output())
        .await
        .expect("command finished")
        .unwrap();
    (
        o.status.code(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}
async fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
fn socket(home: &Path) -> std::path::PathBuf {
    wallet::state_dir(home).join("serve.sock")
}
/// Whether another process (or open file) could take `owner.lock` right now.
fn lock_free(home: &Path) -> bool {
    let Ok(f) = std::fs::File::open(wallet::state_dir(home).join("owner.lock")) else {
        return true;
    };
    f.try_lock_exclusive().is_ok()
}

/// SAFETY (bootstrap rule): a maxplayer home that never traded (no `trade/trade.sqlite`; with or
/// without an empty `trade/` directory, which a refused trade command can leave), a legacy
/// standalone home and a mixed home get nothing created by a host, however long it runs: no
/// `trade/` directory, journal, `owner.lock` or socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_home_that_does_not_trade_gets_nothing_created() {
    let _g = SERIAL.lock().await;
    shutdown::reset();
    let fresh = maxplayer_home();
    let empty_state = maxplayer_home();
    std::fs::create_dir(empty_state.path().join("trade")).unwrap();
    let legacy = tempfile::tempdir().unwrap();
    std::fs::write(legacy.path().join("wallet.seed"), b"x").unwrap();
    let mixed = maxplayer_home();
    std::fs::write(mixed.path().join("owner.lock"), b"").unwrap();
    let before = |p: &Path| {
        let mut v: Vec<_> = std::fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let empty_trade_dir = empty_state.path().join("trade");
    let homes = [
        fresh.path(),
        empty_state.path(),
        empty_trade_dir.as_path(),
        legacy.path(),
        mixed.path(),
    ];
    let snapshot: Vec<_> = homes.iter().map(|p| before(p)).collect();
    let hosts: Vec<_> = [
        fresh.path(),
        empty_state.path(),
        legacy.path(),
        mixed.path(),
    ]
    .iter()
    .map(|p| host::spawn(p, options(&["ws://127.0.0.1:1"])).unwrap())
    .collect();
    // Many polls (100 ms each).
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for (i, p) in homes.iter().enumerate() {
        assert_eq!(
            before(p),
            snapshot[i],
            "SAFETY: a host created files in a home that does not trade: {}",
            p.display()
        );
    }
    for h in &hosts {
        assert!(!h.serving());
        assert_eq!(h.sessions(), 0);
    }
    assert!(!fresh.path().join("trade").exists());
    for h in hosts {
        h.shutdown("test").await;
    }
    // A home that starts trading later (its first trade command creates the journal) is served.
    shutdown::reset();
    let h = host::spawn(fresh.path(), options(&["ws://127.0.0.1:1"])).unwrap();
    drop(Journal::open(fresh.path()).await.unwrap());
    until("served once trading", 30, || h.serving()).await;
    until("socket", 30, || socket(fresh.path()).exists()).await;
    h.shutdown("test").await;
    shutdown::reset();
}

/// One serve per home: the host holds `owner.lock` and answers on `serve.sock`; a CLI `cancel`
/// hands off to it (exit and error from the hosted serve), a standalone `serve` is refused with the
/// reason and the old "home is already in use" phrase, and the host's shutdown removes the socket
/// and frees the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_serve_owns_the_home_and_takes_hand_offs() {
    let _g = SERIAL.lock().await;
    shutdown::reset();
    let f = Fixture::new(0).await;
    let h = host::spawn(&f.maker, options(&[&f.relay_url])).unwrap();
    until("socket", 30, || socket(&f.maker).exists()).await;
    assert!(h.serving());
    assert!(!lock_free(&f.maker), "SAFETY: the host holds owner.lock");
    let lot = "ab".repeat(32);
    let (code, stderr) = output(cmd(&f.maker, Some(&f.relay_url), &["cancel", &lot])).await;
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("unknown local lot"), "{stderr}");
    let (code, stderr) = output(cmd(&f.maker, Some(&f.relay_url), &["serve"])).await;
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("trade serve is already running")
            && stderr.contains("home is already in use"),
        "SAFETY: a second serve on the home is refused with the reason: {stderr}"
    );
    assert!(h.serving(), "the refused serve changed nothing");
    h.shutdown("test").await;
    assert!(!socket(&f.maker).exists(), "socket removed");
    assert!(lock_free(&f.maker), "home released");
    shutdown::reset();
}

/// Without `--relay`, a command on a maxplayer home uses its `[trade] relays`, the set a hosted
/// serve uses, so the hand-off is accepted instead of refused for a relay mismatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_without_relay_flags_use_the_homes_trade_relays() {
    let _g = SERIAL.lock().await;
    shutdown::reset();
    let f = Fixture::new(0).await;
    let cfg = f.maker.join("config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[trade]\nrelays = [{:?}]\n", f.relay_url));
    std::fs::write(&cfg, text).unwrap();
    let h = host::spawn(&f.maker, options(&[&f.relay_url])).unwrap();
    until("socket", 30, || socket(&f.maker).exists()).await;
    let (code, stderr) = output(cmd(&f.maker, None, &["cancel", &"cd".repeat(32)])).await;
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert!(
        !stderr.contains("serve uses relays"),
        "the default relays come from [trade] relays: {stderr}"
    );
    assert!(stderr.contains("unknown local lot"), "{stderr}");
    assert_eq!(code, Some(1));
    h.shutdown("test").await;
    shutdown::reset();
}

/// A host that finds `owner.lock` held (here by a standalone `serve`; the other daemon on the home
/// is the same case) stands by, creating nothing new and never binding the socket, and takes over
/// once the holder exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_stands_by_while_another_serve_owns_the_home_then_takes_over() {
    let _g = SERIAL.lock().await;
    shutdown::reset();
    let f = Fixture::new(0).await;
    let log = f.root.path().join("standalone.log");
    let mut standalone = cmd(&f.maker, Some(&f.relay_url), &["serve"]);
    standalone
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    let mut standalone = standalone.spawn().unwrap();
    until("standalone socket", 30, || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("{\"serve_socket\":\"")
    })
    .await;
    let h = host::spawn(&f.maker, options(&[&f.relay_url])).unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!h.serving(), "SAFETY: never a second serve on one home");
    assert_eq!(h.sessions(), 0);
    let pid = standalone.id().unwrap().to_string();
    std::process::Command::new("kill")
        .args(["-s", "TERM", &pid])
        .status()
        .unwrap();
    let code = tokio::time::timeout(Duration::from_secs(30), standalone.wait())
        .await
        .unwrap()
        .unwrap()
        .code();
    assert_eq!(code, Some(5));
    until("host takes over", 30, || h.serving()).await;
    until("socket", 30, || socket(&f.maker).exists()).await;
    let (code, stderr) = output(cmd(
        &f.maker,
        Some(&f.relay_url),
        &["cancel", &"ef".repeat(32)],
    ))
    .await;
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert_eq!(code, Some(1));
    h.shutdown("test").await;
    shutdown::reset();
}

/// An error that ends serve (here an unreadable journal row in startup recovery) never stops the
/// host: the session ends and releases the home, and a new one starts after the backoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_error_restarts_serve_instead_of_ending_the_host() {
    let _g = SERIAL.lock().await;
    shutdown::reset();
    let f = Fixture::new(0).await;
    let home = maxplayer_home();
    {
        let j = Journal::open(home.path()).await.unwrap();
        j.put("swap", "broken", &serde_json::json!({"not":"a swap"}))
            .await
            .unwrap();
    }
    let h = host::spawn(home.path(), options(&[&f.relay_url])).unwrap();
    until("restarted", 60, || h.sessions() >= 3).await;
    assert!(h.running(), "SAFETY: the host outlives a failed session");
    let j = Journal::open(home.path()).await.unwrap();
    assert_eq!(
        j.get::<serde_json::Value>("swap", "broken").await.unwrap(),
        Some(serde_json::json!({"not":"a swap"})),
        "journal kept"
    );
    h.shutdown("test").await;
    assert!(lock_free(home.path()), "home released");
    shutdown::reset();
}

/// SAFETY: the daemon's shutdown keeps trade's safe stop. With the taker's lock swap held at mint
/// b, `shutdown` does not return: the step finishes once the mint answers and is journaled
/// (`first_locked`, not terminal, not released) before the host lets the daemon go; then the
/// socket is gone and the home is free for the next start, which resumes from the journal.
#[cfg(not(feature = "lab"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_lets_the_money_step_in_progress_finish() {
    let _g = SERIAL.lock().await;
    shutdown::reset();
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let h = host::spawn(&f.taker, options(&[&f.relay_url])).unwrap();
    until("socket", 30, || socket(&f.taker).exists()).await;
    f.b.faults.hold_swap.store(true, SeqCst);
    let mut client = cmd(
        &f.taker,
        Some(&f.relay_url),
        &["take", &lot, "--max-give", "40", "--min-receive", "32"],
    );
    client.stdout(Stdio::null()).stderr(Stdio::null());
    let mut client = client.spawn().unwrap();
    let start = Instant::now();
    loop {
        f.step(true).await;
        if tokio::time::timeout(
            Duration::from_millis(50),
            f.b.faults.swap_entered.notified(),
        )
        .await
        .is_ok()
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "lock never sent"
        );
    }
    let id = f.jt.all::<Swap>("swap").await.unwrap().pop().unwrap().id;
    let stopping = tokio::spawn(h.shutdown("test"));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !stopping.is_finished(),
        "SAFETY: shutdown returned while a money step was at the mint"
    );
    f.b.faults.swap_release.notify_one();
    f.b.faults.swap_finished.notified().await;
    tokio::time::timeout(Duration::from_secs(60), stopping)
        .await
        .expect("shutdown finished after the step")
        .unwrap();
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert_eq!(
        s.state, "first_locked",
        "SAFETY: the running step finished and was journaled"
    );
    assert!(!coordinator::terminal(&s));
    assert!(!socket(&f.taker).exists());
    assert!(lock_free(&f.taker), "home released for the next start");
    let _ = client.kill().await;
    shutdown::reset();
}
