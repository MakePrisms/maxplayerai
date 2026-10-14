//! `serve` lifecycle (T4): startup and final lines, SIGTERM/SIGINT safe stop with exit 5, the
//! journal kept intact across a stop in the middle of a swap step, error exit codes unchanged,
//! and a closed market inbox reported instead of spun on. Fake mints, a loopback relay and
//! disposable homes only. Every SAFETY assertion comes first in its test.
mod support;
#[cfg(not(feature = "lab"))]
use maxplayer_trade::mint;
use maxplayer_trade::{
    coordinator::{self, Swap},
    journal::Journal,
    market::Market,
};
use std::{path::Path, process::Stdio, sync::atomic::Ordering::SeqCst, time::Duration};
use support::*;
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_maxplayer-trade");

fn cmd(home: &Path, relay: &str, args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.arg("--home")
        .arg(home)
        .args(["--relay", relay])
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    c
}
async fn start_serve(home: &Path, relay: &str, log: &Path) -> Child {
    let mut c = cmd(home, relay, &["serve"]);
    c.stdout(Stdio::null())
        .stderr(std::fs::File::create(log).unwrap());
    let child = c.spawn().unwrap();
    wait_log(log, "{\"serve_socket\":\"", 30).await;
    child
}
async fn wait_log(log: &Path, needle: &str, secs: u64) {
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(log)
        .unwrap_or_default()
        .contains(needle)
    {
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "{needle} not logged:\n{}",
            std::fs::read_to_string(log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
fn kill(child: &Child, sig: &str) {
    let pid = child.id().unwrap().to_string();
    assert!(
        std::process::Command::new("kill")
            .args(["-s", sig, &pid])
            .status()
            .unwrap()
            .success()
    );
}
async fn exit_code(child: &mut Child, secs: u64) -> Option<i32> {
    tokio::time::timeout(Duration::from_secs(secs), child.wait())
        .await
        .expect("serve did not exit")
        .unwrap()
        .code()
}
/// The JSON lifecycle lines (`"serve": …`) serve printed, in order.
fn lifecycle(log: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("serve").is_some())
        .collect()
}
fn utc(v: &serde_json::Value) -> bool {
    let s = v.as_str().unwrap_or_default();
    s.len() == 20 && s.ends_with('Z') && &s[10..11] == "T" && s.starts_with("20")
}

/// TERM and INT on an idle serve: one startup line with what an operator needs (UTC time,
/// version, commit, pid, home, relays), one `stopping` line and exactly one final `exited`
/// line (reason signal, exit 5, nothing unfinished), exit status 5, socket removed and the home
/// lock released (a CLI writer runs right after).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn term_and_int_on_idle_serve_exit_5_with_one_final_line() {
    let f = Fixture::new(0).await;
    for sig in ["TERM", "INT"] {
        let log = f.root.path().join(format!("serve-{sig}.log"));
        let mut serve = start_serve(&f.maker, &f.relay_url, &log).await;
        let pid = serve.id().unwrap();
        kill(&serve, sig);
        let code = exit_code(&mut serve, 30).await;
        let lines = lifecycle(&log);
        let raw = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            code,
            Some(5),
            "SAFETY: a signal stop has its own exit code\n{raw}"
        );
        let exited: Vec<_> = lines.iter().filter(|v| v["serve"] == "exited").collect();
        assert_eq!(exited.len(), 1, "exactly one final line\n{raw}");
        let end = exited[0];
        assert_eq!(end["reason"], "signal", "{raw}");
        assert_eq!(end["signal"], format!("SIG{sig}"), "{raw}");
        assert_eq!(end["exit"], 5);
        assert_eq!(end["pid"], pid);
        assert!(end["forced"].is_null(), "{raw}");
        assert!(utc(&end["at"]), "{raw}");
        assert_eq!(end["unfinished"]["swaps"], serde_json::json!([]), "{raw}");
        assert_eq!(end["socket_removed"], true, "{raw}");
        let start = &lines[0];
        assert_eq!(start["serve"], "started", "{raw}");
        assert!(utc(&start["at"]), "{raw}");
        assert_eq!(start["version"], env!("CARGO_PKG_VERSION"));
        let commit = start["commit"].as_str().unwrap();
        assert!(
            commit == "unknown"
                || commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
            "{commit}"
        );
        assert_eq!(start["pid"], pid);
        assert_eq!(start["home"], f.maker.to_str().unwrap());
        assert_eq!(start["relays"], serde_json::json!([f.relay_url]));
        assert!(
            lines
                .iter()
                .any(|v| v["serve"] == "stopping" && v["signal"] == format!("SIG{sig}")),
            "{raw}"
        );
        assert!(!f.maker.join("serve.sock").exists());
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            cmd(&f.maker, &f.relay_url, &["recover"]).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            out.status.success(),
            "home lock released: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(not(feature = "lab"))]
/// Hand a take to serve and step the in-process maker until the taker's lock swap reaches mint
/// b, where the fake mint holds it. Returns (serve, take client, swap id).
async fn take_until_lock_held(f: &mut Fixture, log: &Path) -> (Child, Child, String) {
    let lot = f.list(false).await;
    let serve = start_serve(&f.taker, &f.relay_url, log).await;
    f.b.faults.hold_swap.store(true, SeqCst);
    let client = cmd(
        &f.taker,
        &f.relay_url,
        &["take", &lot, "--max-give", "40", "--min-receive", "32"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let start = std::time::Instant::now();
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
    (serve, client, id)
}
#[cfg(not(feature = "lab"))]
/// Restart serve on the taker home and step the maker until the swap settles; then TERM it.
async fn restart_and_settle(f: &mut Fixture, id: &str, log: &Path) {
    let mut serve = start_serve(&f.taker, &f.relay_url, log).await;
    let start = std::time::Instant::now();
    let mut next = std::time::Instant::now();
    while f.state(false, id).await.as_deref() != Some("complete") {
        f.step(true).await;
        if std::time::Instant::now() >= next {
            let _ = coordinator::recover(&f.maker, &f.jm, &f.mm).await;
            next = std::time::Instant::now() + Duration::from_secs(3);
        }
        assert!(
            serve.try_wait().unwrap().is_none(),
            "serve died: {}",
            std::fs::read_to_string(log).unwrap()
        );
        assert!(
            start.elapsed() < Duration::from_secs(180),
            "restart did not settle: {:?}\n{}",
            f.state(false, id).await,
            std::fs::read_to_string(log).unwrap()
        );
    }
    assert_eq!(balance(&f.taker, &f.a.url).await, 32, "taker received");
    kill(&serve, "TERM");
    assert_eq!(exit_code(&mut serve, 60).await, Some(5));
    let end = lifecycle(log).pop().unwrap();
    assert_eq!(end["serve"], "exited");
    assert_eq!(end["unfinished"]["swaps"], serde_json::json!([]), "{end}");
}

/// SAFETY: TERM while serve's lock swap is at the mint lets that step finish: its result is
/// journaled (`first_locked`) before serve exits 5, a request handed off meanwhile is refused as
/// not started, and the final line lists the swap as unfinished with its refund eligibility.
/// A restarted serve settles the swap. Production deadlines (the lab suite's 16 s ones would
/// expire during the held step).
#[cfg(not(feature = "lab"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn term_during_a_swap_step_lets_it_finish_and_restart_settles() {
    let mut f = Fixture::new(0).await;
    let log = f.root.path().join("serve.log");
    let (mut serve, mut client, id) = Box::pin(take_until_lock_held(&mut f, &log)).await;
    kill(&serve, "TERM");
    wait_log(&log, "\"serve\":\"stopping\"", 10).await;
    // Admits nothing new: a hand-off now is answered "not started" once the step ends.
    let late = cmd(&f.taker, &f.relay_url, &["cancel", &"ab".repeat(32)])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    f.b.faults.swap_release.notify_one();
    f.b.faults.swap_finished.notified().await;
    let code = exit_code(&mut serve, 60).await;
    let raw = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("first_locked"),
        "SAFETY: the running step finished and was journaled\n{raw}"
    );
    assert_eq!(code, Some(5), "{raw}");
    let late = late.wait_with_output().await.unwrap();
    let late_err = String::from_utf8_lossy(&late.stderr);
    assert_eq!(late.status.code(), Some(1), "{late_err}");
    assert!(late_err.contains("serve is stopping"), "{late_err}");
    let end = lifecycle(&log).pop().unwrap();
    assert_eq!(end["serve"], "exited", "{raw}");
    assert_eq!(end["reason"], "signal");
    assert!(end["forced"].is_null(), "graceful: {end}");
    let swaps = end["unfinished"]["swaps"].as_array().unwrap();
    assert_eq!(swaps.len(), 1, "{end}");
    assert_eq!(swaps[0]["swap_id"], id.as_str());
    assert_eq!(swaps[0]["state"], "first_locked");
    assert!(utc(&swaps[0]["refund_earliest"]), "{end}");
    let _ = client.kill().await;
    let log2 = f.root.path().join("serve2.log");
    Box::pin(restart_and_settle(&mut f, &id, &log2)).await;
}

/// SAFETY: a second signal while the lock swap is still at the mint cancels the step at its
/// wait (exit 5, `forced`). The journal keeps the step's attempt row and the swap is neither
/// released nor expired; the mint completes the held swap afterwards, and a restarted serve
/// reconciles it from the journal and settles.
#[cfg(not(feature = "lab"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_signal_cancels_the_step_with_the_journal_intact() {
    let mut f = Fixture::new(0).await;
    let log = f.root.path().join("serve.log");
    let (mut serve, mut client, id) = Box::pin(take_until_lock_held(&mut f, &log)).await;
    kill(&serve, "TERM");
    wait_log(&log, "\"serve\":\"stopping\"", 10).await;
    kill(&serve, "INT");
    let code = exit_code(&mut serve, 30).await;
    let raw = std::fs::read_to_string(&log).unwrap();
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert!(
        !coordinator::terminal(&s),
        "SAFETY: a cancelled step never ends the swap: {}\n{raw}",
        s.state
    );
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .is_some(),
        "SAFETY: the in-flight lock stays journaled\n{raw}"
    );
    assert_eq!(code, Some(5), "{raw}");
    let end = lifecycle(&log).pop().unwrap();
    assert_eq!(end["reason"], "signal", "{raw}");
    assert_eq!(end["signal"], "SIGTERM");
    assert_eq!(end["forced"], "SIGINT");
    assert!(
        lifecycle(&log)
            .iter()
            .any(|v| v["serve"] == "stop_forced" && v["by"] == "SIGINT"),
        "{raw}"
    );
    assert_eq!(end["unfinished"]["swaps"][0]["swap_id"], id.as_str());
    let _ = client.kill().await;
    // The mint finishes the swap it was holding; only the journal knows about it now.
    f.b.faults.swap_release.notify_one();
    f.b.faults.swap_finished.notified().await;
    let log2 = f.root.path().join("serve2.log");
    Box::pin(restart_and_settle(&mut f, &id, &log2)).await;
}

/// An error in serve's startup recovery keeps exit 1 (codes 1/3/4 unchanged) and its final line
/// says `error`, with the unreadable row counted as unfinished. The journal is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_recover_error_keeps_exit_1_and_reports_error() {
    let f = Fixture::new(0).await;
    let home = f.root.path().join("broken");
    std::fs::create_dir_all(&home).unwrap();
    {
        let j = Journal::open(&home).await.unwrap();
        j.put("swap", "broken", &serde_json::json!({"not":"a swap"}))
            .await
            .unwrap();
    }
    let log = f.root.path().join("broken.log");
    let mut c = cmd(&home, &f.relay_url, &["serve"]);
    c.stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    let mut serve = c.spawn().unwrap();
    let code = exit_code(&mut serve, 60).await;
    let raw = std::fs::read_to_string(&log).unwrap();
    let j = Journal::open(&home).await.unwrap();
    assert_eq!(
        j.get::<serde_json::Value>("swap", "broken").await.unwrap(),
        Some(serde_json::json!({"not":"a swap"})),
        "SAFETY: journal kept"
    );
    assert_eq!(code, Some(1), "{raw}");
    let lines = lifecycle(&log);
    assert_eq!(lines[0]["serve"], "started", "{raw}");
    let exited: Vec<_> = lines.iter().filter(|v| v["serve"] == "exited").collect();
    assert_eq!(exited.len(), 1, "{raw}");
    assert_eq!(exited[0]["reason"], "error");
    assert_eq!(exited[0]["exit"], 1);
    assert!(exited[0]["signal"].is_null());
    assert_eq!(exited[0]["unfinished"]["swaps"][0]["state"], "unreadable");
}

/// A closed market inbox (every relay client stopped) ends serve's loop with `InboxClosed`
/// instead of spinning on an empty receive; a `take` loop keeps recovering on its ticks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_inbox_ends_serve_loop() {
    let f = Fixture::new(0).await;
    let home = f.root.path().join("inbox");
    std::fs::create_dir_all(&home).unwrap();
    let j = Journal::open(&home).await.unwrap();
    let mut m = Market::connect(
        maxplayer_trade::wallet::identity(&home).unwrap(),
        std::slice::from_ref(&f.relay_url),
    )
    .await
    .unwrap();
    m.inbox = tokio::sync::mpsc::unbounded_channel().1;
    let r = tokio::time::timeout(
        Duration::from_secs(30),
        coordinator::run_with(&home, &j, &mut m, None, None),
    )
    .await
    .expect("SAFETY: serve spun on a closed inbox");
    let e = r.unwrap_err();
    assert!(e.is::<maxplayer_trade::shutdown::InboxClosed>(), "{e:#}");
}

/// T2 through serve: while mint b answers the taker's lock swap with 503, serve prints a
/// timestamped `mint_attempt` line for that swap and forwards it to the `take` client that
/// handed the swap off (stderr only; stdout keeps its machine-readable state lines). No token
/// or proof text reaches either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_is_forwarded_to_the_following_take() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let log = f.root.path().join("serve.log");
    let mut serve = start_serve(&f.taker, &f.relay_url, &log).await;
    f.b.faults.reject_swap.store(true, SeqCst);
    let mut client = cmd(
        &f.taker,
        &f.relay_url,
        &["take", &lot, "--max-give", "40", "--min-receive", "32"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(client.stderr.take().unwrap()).lines();
        let sink = stderr.clone();
        tokio::spawn(async move {
            while let Ok(Some(l)) = lines.next_line().await {
                let mut s = sink.lock().unwrap();
                s.push_str(&l);
                s.push('\n');
            }
        });
    }
    let forwarded = |id: &str| {
        stderr
            .lock()
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["progress"] == "mint_attempt" && v["swap_id"] == id)
    };
    let start = std::time::Instant::now();
    let line = loop {
        f.step(true).await;
        if let Some(s) = f.jt.all::<Swap>("swap").await.unwrap().pop() {
            if let Some(line) = forwarded(&s.id) {
                break line;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "no forwarded progress\nclient:\n{}\nserve:\n{}",
            stderr.lock().unwrap(),
            std::fs::read_to_string(&log).unwrap()
        );
    };
    assert_eq!(line["mint"], f.b.url.as_str(), "{line}");
    assert_eq!(line["op"], "swap", "{line}");
    assert_eq!(line["outcome"], "no_answer", "{line}");
    assert_eq!(line["attempt"], 1, "{line}");
    assert!(utc(&line["at"]), "{line}");
    let served = std::fs::read_to_string(&log).unwrap();
    assert!(
        served.contains("\"progress\":\"mint_attempt\""),
        "serve prints it too"
    );
    for text in [served.as_str(), stderr.lock().unwrap().as_str()] {
        assert!(
            !text.contains("cashuA") && !text.contains("cashuB"),
            "SAFETY: token text: {text}"
        );
    }
    let _ = client.kill().await;
    kill(&serve, "TERM");
    assert_eq!(exit_code(&mut serve, 60).await, Some(5));
}
