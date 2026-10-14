//! `send` and `fund` handed to a running `serve` (T3b). serve stays the single writer: the client
//! resolves its bounds (absolute `--out`, `--max-fees` exactly as given, a funding operation id)
//! and passes them over `<home>/serve.sock`; serve runs the unchanged send/fund journal paths.
//! Loopback fake mints, a local relay and disposable homes only. SAFETY assertions first.
mod support;
use maxplayer_trade::{
    money::Funding,
    send::{SendAttempt, SendState},
    wallet,
};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::Ordering::SeqCst,
    time::Duration,
};
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    c
}
async fn run(home: &Path, relay: &str, args: &[&str]) -> std::process::Output {
    tokio::time::timeout(Duration::from_secs(120), cmd(home, relay, args).output())
        .await
        .expect("command timed out")
        .unwrap()
}
fn text(o: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}
struct Serve {
    child: Child,
    log: PathBuf,
}
async fn start_serve(
    home: &Path,
    relay: &str,
    dir: &Path,
    name: &str,
    env: &[(&str, &str)],
) -> Serve {
    let log = dir.join(format!("{name}.stderr"));
    let mut c = Command::new(BIN);
    c.arg("--home")
        .arg(home)
        .args(["--relay", relay, "serve"])
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .kill_on_drop(true);
    let child = c.spawn().unwrap();
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("{\"serve_socket\":\"")
    {
        assert!(start.elapsed() < Duration::from_secs(30), "serve socket");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Serve { child, log }
}
fn balance(home: &Path, mint: &str) -> u64 {
    wallet::read_balance(home, mint).unwrap()
}
fn json_lines(s: &str) -> Vec<serde_json::Value> {
    s.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}
async fn sends(j: &maxplayer_trade::journal::Journal) -> Vec<SendAttempt> {
    j.all::<SendAttempt>("send").await.unwrap()
}

/// A send handed to serve writes exactly the named file (0600), once; the same send again is an
/// idempotent no-op (one mint swap, one debit); a different amount for that `--out` is refused;
/// a relative `--out` is resolved by the client, not by serve's working directory; serve keeps
/// its lock throughout, and the token is never printed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_via_serve_writes_the_named_file_once_and_is_idempotent() {
    let f = Fixture::new(0).await;
    let mint = f.b.url.clone();
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve", &[]).await;
    let before = balance(&f.taker, &mint);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("t.token");
    let args = [
        "send",
        &mint,
        "--amount",
        "20",
        "--out",
        out.to_str().unwrap(),
    ];
    let first = run(&f.taker, &f.relay_url, &args).await;
    let (stdout, stderr) = text(&first);
    assert!(first.status.success(), "{stderr}");
    assert!(
        stderr.contains("handed_to_serve"),
        "SAFETY: send went to serve: {stderr}"
    );
    let token = std::fs::read_to_string(&out).unwrap().trim().to_owned();
    assert!(token.starts_with("cashu"));
    assert_eq!(
        std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o600,
        "SAFETY: token file private"
    );
    assert!(
        !stdout.contains(&token) && !stderr.contains(&token),
        "SAFETY: token printed"
    );
    let v = &json_lines(&stdout)[0];
    assert_eq!(
        (v["state"].as_str(), v["amount"].as_u64()),
        (Some("sent"), Some(20))
    );
    assert_eq!(balance(&f.taker, &mint), before - 20, "SAFETY: one debit");
    let swaps = f.b.faults.swaps.load(SeqCst);
    // Same send again (lost reply / retry): no second swap, same file.
    let again = run(&f.taker, &f.relay_url, &args).await;
    assert!(again.status.success(), "{}", text(&again).1);
    assert_eq!(
        f.b.faults.swaps.load(SeqCst),
        swaps,
        "SAFETY: no second swap"
    );
    assert_eq!(
        balance(&f.taker, &mint),
        before - 20,
        "SAFETY: no second debit"
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), token);
    assert_eq!(sends(&f.jt).await.len(), 1);
    // Other terms for the same --out: refused, nothing sent.
    let other = run(
        &f.taker,
        &f.relay_url,
        &[
            "send",
            &mint,
            "--amount",
            "21",
            "--out",
            out.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(other.status.code(), Some(1));
    assert!(text(&other).1.contains("other terms"), "{}", text(&other).1);
    assert_eq!(f.b.faults.swaps.load(SeqCst), swaps);
    // Relative --out: the client's directory, not serve's.
    let rel = tempfile::tempdir().unwrap();
    let mut c = cmd(
        &f.taker,
        &f.relay_url,
        &["send", &mint, "--amount", "5", "--out", "r.token"],
    );
    c.current_dir(rel.path());
    let o = c.output().await.unwrap();
    assert!(o.status.success(), "{}", text(&o).1);
    assert!(
        rel.path().join("r.token").is_file(),
        "SAFETY: file in the client's directory"
    );
    assert_eq!(balance(&f.taker, &mint), before - 25);
    // serve kept the home and never logged a token.
    assert!(serve.child.try_wait().unwrap().is_none());
    let log = std::fs::read_to_string(&serve.log).unwrap();
    assert!(!log.contains(&token), "SAFETY: token in serve log");
    assert!(log.contains("\"serve_request\":\"send\""), "{log}");
    serve.child.kill().await.unwrap();
}

/// Fee caps reach serve exactly as given: at a mint that charges input fees, a send without
/// `--max-fees` (or with one below the fee) is refused by serve with nothing journaled, reserved
/// or sent; with an adequate cap it succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_via_serve_keeps_explicit_fee_caps() {
    let f = Fixture::new(100).await;
    let mint = f.b.url.clone();
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve", &[]).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("t.token");
    let o = out.to_str().unwrap();
    let before = balance(&f.taker, &mint);
    let swaps = f.b.faults.swaps.load(SeqCst);
    let none = run(
        &f.taker,
        &f.relay_url,
        &["send", &mint, "--amount", "20", "--out", o],
    )
    .await;
    assert_eq!(none.status.code(), Some(1), "{}", text(&none).1);
    assert!(
        text(&none).1.contains("--max-fees is required"),
        "{}",
        text(&none).1
    );
    let zero = run(
        &f.taker,
        &f.relay_url,
        &[
            "send",
            &mint,
            "--amount",
            "20",
            "--out",
            o,
            "--max-fees",
            "0",
        ],
    )
    .await;
    assert_eq!(zero.status.code(), Some(1));
    assert!(
        text(&zero).1.contains("exceeds --max-fees"),
        "{}",
        text(&zero).1
    );
    assert!(sends(&f.jt).await.is_empty(), "SAFETY: nothing journaled");
    assert_eq!(f.b.faults.swaps.load(SeqCst), swaps, "SAFETY: nothing sent");
    assert_eq!(balance(&f.taker, &mint), before);
    assert!(!out.exists());
    let ok = run(
        &f.taker,
        &f.relay_url,
        &[
            "send",
            &mint,
            "--amount",
            "20",
            "--out",
            o,
            "--max-fees",
            "8",
        ],
    )
    .await;
    assert!(ok.status.success(), "{}", text(&ok).1);
    let fee = json_lines(&text(&ok).0)[0]["fee"].as_u64().unwrap();
    assert!(fee > 0 && fee <= 8);
    assert_eq!(balance(&f.taker, &mint), before - 20 - fee);
    serve.child.kill().await.unwrap();
}

/// serve dies after the send swap reached the mint but before the token file was written: the
/// client loses its reply (exit 3, told to re-run the same send). A restarted serve's recovery
/// restores the same outputs and writes the file; the re-run resumes that attempt. Exactly one
/// mint swap, one debit, one file.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_killed_mid_send_then_restart_writes_the_same_token_once() {
    let f = Fixture::new(0).await;
    let mint = f.b.url.clone();
    let before = balance(&f.taker, &mint);
    let mut serve = start_serve(
        &f.taker,
        &f.relay_url,
        f.root.path(),
        "serve1",
        &[("TRADE_CRASH_AFTER_SEND_SWAP", "1")],
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("t.token");
    let args = [
        "send",
        &mint,
        "--amount",
        "20",
        "--out",
        out.to_str().unwrap(),
    ];
    let lost = run(&f.taker, &f.relay_url, &args).await;
    let (_, stderr) = text(&lost);
    assert_eq!(lost.status.code(), Some(3), "{stderr}");
    assert!(stderr.contains("Re-run this same send"), "{stderr}");
    assert_eq!(serve.child.wait().await.unwrap().code(), Some(86));
    assert!(!out.exists(), "fixture: crashed before the file");
    assert_eq!(f.b.faults.swaps.load(SeqCst), 1);
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve2", &[]).await;
    let retry = run(&f.taker, &f.relay_url, &args).await;
    assert!(retry.status.success(), "{}", text(&retry).1);
    let token = std::fs::read_to_string(&out).unwrap().trim().to_owned();
    assert!(token.starts_with("cashu"));
    assert_eq!(f.b.faults.swaps.load(SeqCst), 1, "SAFETY: one mint swap");
    assert_eq!(balance(&f.taker, &mint), before - 20, "SAFETY: one debit");
    let all = sends(&f.jt).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].state, SendState::Sent);
    serve.child.kill().await.unwrap();
}

/// A fund handed to serve prints its operation id before anything is created, then the invoice;
/// serve's recovery issues it (the fake mint pays at once) while the client only follows the
/// journal. The same `--id` again returns the same quote and invoice and never creates or
/// issues a second funding; a mismatched amount for that id is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fund_via_serve_is_idempotent_by_operation_id() {
    let f = Fixture::new(0).await;
    let mint = f.b.url.clone();
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve", &[]).await;
    let before = balance(&f.taker, &mint);
    let first = run(&f.taker, &f.relay_url, &["fund", &mint, "--amount", "30"]).await;
    let (stdout, stderr) = text(&first);
    assert!(first.status.success(), "{stdout}\n{stderr}");
    let pre = json_lines(&stderr);
    let id = pre
        .iter()
        .find_map(|v| v["funding_id"].as_str().map(str::to_owned))
        .expect("funding id printed first");
    let handed = stderr.find("handed_to_serve").unwrap();
    assert!(
        stderr.find(&id).unwrap() < handed,
        "SAFETY: id printed before the hand-off"
    );
    let line = json_lines(&stdout)
        .into_iter()
        .find(|v| v.get("invoice").is_some())
        .unwrap();
    assert_eq!(line["funding_id"], id.as_str());
    let invoice = line["invoice"].as_str().unwrap().to_owned();
    let start = std::time::Instant::now();
    while balance(&f.taker, &mint) != before + 30 {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "funding not issued"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let again = run(
        &f.taker,
        &f.relay_url,
        &["fund", &mint, "--amount", "30", "--id", &id],
    )
    .await;
    let (stdout, stderr) = text(&again);
    assert!(again.status.success(), "{stderr}");
    let line = json_lines(&stdout)
        .into_iter()
        .find(|v| v.get("invoice").is_some())
        .unwrap();
    assert_eq!(
        line["invoice"].as_str(),
        Some(invoice.as_str()),
        "SAFETY: same invoice"
    );
    assert_eq!(balance(&f.taker, &mint), before + 30, "SAFETY: issued once");
    let fundings = f.jt.all::<Funding>("funding").await.unwrap();
    assert_eq!(fundings.len(), 1, "SAFETY: one funding intent");
    let wrong = run(
        &f.taker,
        &f.relay_url,
        &["fund", &mint, "--amount", "31", "--id", &id],
    )
    .await;
    assert_eq!(wrong.status.code(), Some(1));
    assert!(
        text(&wrong).1.contains("another mint or amount"),
        "{}",
        text(&wrong).1
    );
    assert_eq!(f.jt.all::<Funding>("funding").await.unwrap().len(), 1);
    assert!(serve.child.try_wait().unwrap().is_none());
    serve.child.kill().await.unwrap();
}

/// A funding whose quote stays unpaid does not wait inline in serve's loop: a send to the same
/// home, handed to serve while that fund's client is still following, completes first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_fund_does_not_hold_serve() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let mint = f.b.url.clone();
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve", &[]).await;
    // Funding on another mint that answers every request after 10 s (still inside the 20 s
    // RPC timeout): its quote creation alone holds that mint's unit for tens of seconds.
    c.faults.delay_ms.store(10_000, SeqCst);
    let fund = cmd(&f.taker, &f.relay_url, &["fund", &c.url, "--amount", "10"])
        .spawn()
        .unwrap();
    // Wait until serve is inside the fund unit (its first request reached the slow mint).
    let start = std::time::Instant::now();
    while c.faults.requests.load(SeqCst) == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "fund never reached its mint"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("t.token");
    let started = std::time::Instant::now();
    let send = run(
        &f.taker,
        &f.relay_url,
        &[
            "send",
            &mint,
            "--amount",
            "5",
            "--out",
            out.to_str().unwrap(),
        ],
    )
    .await;
    assert!(send.status.success(), "{}", text(&send).1);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "SAFETY: send waited on the slow funding mint ({:?})",
        started.elapsed()
    );
    c.faults.delay_ms.store(0, SeqCst);
    let fo = tokio::time::timeout(Duration::from_secs(120), fund.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(fo.status.success(), "{}", text(&fo).1);
    serve.child.kill().await.unwrap();
}
