//! `receive` handed to a running `serve` (T3a). serve stays the single writer: the client only
//! reads the bounded token and passes it over `<home>/serve.sock`; serve runs the unchanged
//! receive/journal path. Loopback fake mints, a local relay and disposable homes only.
//! SAFETY assertions come first in each test.
mod support;
use cashu::nuts::{CurrencyUnit, Proofs, Token};
use cdk::mint_url::MintUrl;
use maxplayer_trade::{
    coordinator::{self, Swap},
    receive::{self, Receipt, ReceiveState},
    wallet,
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::atomic::Ordering::SeqCst,
    time::Duration,
};
use support::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
};

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
fn text(o: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

struct Serve {
    child: Child,
    log: PathBuf,
    out: PathBuf,
}
impl Serve {
    /// serve's stdout and stderr so far.
    fn output(&self) -> String {
        std::fs::read_to_string(&self.out).unwrap_or_default()
            + &std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}
async fn start_serve(home: &Path, relay: &str, dir: &Path, name: &str) -> Serve {
    start_serve_env(home, relay, dir, name, &[]).await
}
async fn start_serve_env(
    home: &Path,
    relay: &str,
    dir: &Path,
    name: &str,
    envs: &[(&str, &str)],
) -> Serve {
    let log = dir.join(format!("{name}.stderr"));
    let out = dir.join(format!("{name}.stdout"));
    let mut c = cmd(home, relay, &["serve"]);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.stdout(std::fs::File::create(&out).unwrap())
        .stderr(std::fs::File::create(&log).unwrap());
    let child = c.spawn().unwrap();
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("{\"serve_socket\":\"")
    {
        assert!(start.elapsed() < Duration::from_secs(30), "serve socket");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Serve { child, log, out }
}

enum Input<'a> {
    File(&'a Path),
    Stdin(&'a str),
}
/// A `receive` CLI child with piped stdout/stderr.
async fn spawn_receive(home: &Path, relay: &str, mint: &str, input: Input<'_>) -> Child {
    let mut args = vec!["receive", mint];
    if let Input::File(f) = &input {
        args.extend(["--token-file", f.to_str().unwrap()]);
    }
    let mut c = cmd(home, relay, &args);
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    if matches!(input, Input::Stdin(_)) {
        c.stdin(Stdio::piped());
    }
    let mut child = c.spawn().unwrap();
    if let Input::Stdin(s) = input {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(s.as_bytes()).await.unwrap();
        drop(stdin);
    }
    child
}
async fn receive_cli(
    home: &Path,
    relay: &str,
    mint: &str,
    input: Input<'_>,
) -> std::process::Output {
    let child = spawn_receive(home, relay, mint, input).await;
    tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("receive timed out")
        .unwrap()
}
/// The `outcome` serve reported for this request (`credited`, `already_received`, ...).
fn outcome(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v["outcome"].as_str().map(str::to_owned))
}
/// The `note` serve reported for this request.
fn note(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v["note"].as_str().map(str::to_owned))
}
fn summary(stdout: &str) -> serde_json::Value {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("receive").is_some())
        .unwrap_or_else(|| panic!("no receive summary in {stdout}"))
}

/// A token for `n` sats issued to an unrelated sender home.
async fn issue(m: &MintFixture, n: u64) -> (tempfile::TempDir, Proofs, String) {
    let sender = tempfile::tempdir().unwrap();
    fund(sender.path(), &m.url, n).await;
    let proofs = wallet::wallet(sender.path(), &m.url)
        .await
        .unwrap()
        .get_unspent_proofs()
        .await
        .unwrap();
    let token = encode(&m.url, proofs.clone());
    (sender, proofs, token)
}
fn encode(mint: &str, proofs: Proofs) -> String {
    Token::new(
        MintUrl::from_str(mint).unwrap(),
        proofs,
        None,
        CurrencyUnit::Sat,
    )
    .to_string()
}
/// Neither the token (any encoding) nor any proof secret or `C` appears in `text`.
fn assert_secret_free(what: &str, text: &str, token: &str, proofs: &Proofs) {
    assert!(!text.contains(token), "SAFETY: token text in {what}");
    let v3 = Token::from_str(token).unwrap().to_v3_string();
    assert!(!text.contains(&v3), "SAFETY: token (v3) in {what}");
    for p in proofs {
        assert!(
            !text.contains(&p.secret.to_string()),
            "SAFETY: proof secret in {what}"
        );
        assert!(!text.contains(&p.c.to_hex()), "SAFETY: proof C in {what}");
    }
}
/// Every byte of every file in `home` (journal, wallet DBs, WAL files, socket excluded).
fn home_bytes(home: &Path) -> String {
    let mut all = Vec::new();
    for e in std::fs::read_dir(home).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            all.extend(std::fs::read(&p).unwrap());
        }
    }
    String::from_utf8_lossy(&all).into_owned()
}
async fn receipts(f: &Fixture) -> Vec<Receipt> {
    f.jt.all::<Receipt>("receive").await.unwrap()
}
fn credited(home: &Path, mint: &str) -> u64 {
    // Read-only snapshot: the test never opens the wallet for writing while serve owns it.
    wallet::read_balance(home, mint).unwrap()
}
fn lock_is_held(home: &Path) -> bool {
    use fs2::FileExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.join("owner.lock"))
        .unwrap();
    f.try_lock_exclusive().is_err()
}
async fn wait_settled(f: &Fixture, limit: Duration) -> Receipt {
    let start = std::time::Instant::now();
    loop {
        if let Some(r) = receipts(f).await.into_iter().find(Receipt::settled) {
            return r;
        }
        assert!(start.elapsed() < limit, "serve did not settle the receive");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// SAFETY: while serve owns the home with a published listing and a swap in flight, `receive`
/// is handed to serve and credits exactly once. serve keeps its lock, the listing stays
/// available, the swap settles, and other writers still get `home is already in use`. Without
/// serve, `receive` runs locally as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_via_serve_credits_once_and_serve_keeps_lock_listing_and_swap() {
    let mut f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let lot = f.list(false).await;
    // The taker's own listing, which serve must keep serving.
    let own = coordinator::list(
        &f.taker,
        &f.jt,
        &f.mt,
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.b.url).unwrap(),
            net: 16,
        },
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.a.url).unwrap(),
            net: 8,
        },
        16,
    )
    .await
    .unwrap();
    let (_s, proofs, token) = issue(&c, 48).await;
    let file = f.root.path().join("in.token");
    std::fs::write(&file, format!("{token}\n")).unwrap();
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve").await;
    let mut take = cmd(
        &f.taker,
        &f.relay_url,
        &["take", &lot, "--max-give", "40", "--min-receive", "32"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let mut id = None;
    for _ in 0..200 {
        f.step(true).await;
        if let Some(s) = f.jt.all::<Swap>("swap").await.unwrap().pop() {
            id = Some(s.id);
            break;
        }
    }
    let id = id.expect("serve journaled the take");
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert!(!coordinator::terminal(&s), "fixture: swap still active");

    let mut client = spawn_receive(&f.taker, &f.relay_url, &c.url, Input::File(&file)).await;
    let start = std::time::Instant::now();
    while client.try_wait().unwrap().is_none() {
        f.step(true).await;
        assert!(start.elapsed() < Duration::from_secs(120), "receive hung");
    }
    let out = client.wait_with_output().await.unwrap();
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("handed_to_serve"),
        "SAFETY: receive must go to serve: {stderr}"
    );
    assert_eq!(c.faults.swaps.load(SeqCst), 1, "SAFETY: exactly one swap");
    assert_eq!(credited(&f.taker, &c.url), 48, "SAFETY: credited once");
    assert_eq!(receipts(&f).await.len(), 1);
    assert_eq!(outcome(&stderr).as_deref(), Some("credited"), "{stderr}");
    let v = summary(&stdout);
    assert_eq!(
        (v["state"].as_str(), v["credited"].as_u64()),
        (Some("done"), Some(48))
    );
    assert_secret_free(
        "receive client output",
        &(stdout + &stderr),
        &token,
        &proofs,
    );

    // serve kept the home: its lock, its listing, its swap.
    assert!(serve.child.try_wait().unwrap().is_none(), "serve survived");
    assert!(lock_is_held(&f.taker), "SAFETY: serve kept owner.lock");
    let l: coordinator::Listing = f.jt.get("listing", &own).await.unwrap().unwrap();
    assert_eq!(
        maxplayer_trade::lifecycle(&l.event, &l.statuses).unwrap(),
        maxplayer_trade::Status::Available,
        "SAFETY: serve's listing stays available"
    );
    // Writers that are not handed off are still refused while serve runs.
    let fund = cmd(&f.taker, &f.relay_url, &["fund", &c.url, "--amount", "10"])
        .output()
        .await
        .unwrap();
    assert_eq!(fund.status.code(), Some(1));
    assert!(
        text(&fund).1.contains("home is already in use"),
        "{}",
        text(&fund).1
    );
    // The swap that was active during the receive settles.
    let begin = std::time::Instant::now();
    let mut next = std::time::Instant::now();
    while take.try_wait().unwrap().is_none() {
        f.step(true).await;
        if std::time::Instant::now() >= next {
            let _ = coordinator::recover(&f.maker, &f.jm, &f.mm).await;
            next = std::time::Instant::now() + Duration::from_secs(3);
        }
        assert!(
            begin.elapsed() < Duration::from_secs(150),
            "take did not finish"
        );
    }
    let t = take.wait_with_output().await.unwrap();
    assert!(t.status.success(), "{}", text(&t).1);
    assert_eq!(f.state(false, &id).await.as_deref(), Some("complete"));
    assert_eq!(credited(&f.taker, &c.url), 48);
    let log = serve.output();
    assert!(log.contains("\"serve_request\":\"receive\""), "{log}");
    assert!(log.contains(&receipts(&f).await[0].id), "{log}");
    assert_secret_free("serve output", &log, &token, &proofs);
    serve.child.kill().await.unwrap();
    serve.child.wait().await.unwrap();

    // Without serve, receive runs locally exactly as before (and stays idempotent).
    let local = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    let (stdout, stderr) = text(&local);
    assert!(local.status.success(), "{stderr}");
    assert!(!stderr.contains("handed_to_serve"), "{stderr}");
    assert_eq!(summary(&stdout)["state"], "done");
    assert_eq!(c.faults.swaps.load(SeqCst), 1);
    assert_eq!(credited(&f.taker, &c.url), 48);
}

/// SAFETY: the same token submitted twice at once (and again re-encoded) swaps once, credits
/// once, and each client is told truthfully whether ITS request credited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_submits_through_serve_credit_once() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&c, 40).await;
    let file = f.root.path().join("in.token");
    std::fs::write(&file, &token).unwrap();
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve").await;
    let (a, b) = tokio::join!(
        receive_cli(&f.taker, &f.relay_url, &c.url, Input::File(&file)),
        receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)),
    );
    let mut outcomes = vec![];
    for o in [&a, &b] {
        let (stdout, stderr) = text(o);
        assert!(
            o.status.success(),
            "SAFETY: a duplicate is not an error: {stderr}"
        );
        assert!(stderr.contains("handed_to_serve"), "{stderr}");
        assert_eq!(summary(&stdout)["state"], "done");
        outcomes.push(outcome(&stderr).unwrap());
        assert_secret_free("client", &(stdout + &stderr), &token, &proofs);
    }
    outcomes.sort();
    assert_eq!(c.faults.swaps.load(SeqCst), 1, "SAFETY: one swap");
    assert_eq!(credited(&f.taker, &c.url), 40, "SAFETY: one credit");
    assert_eq!(receipts(&f).await.len(), 1);
    assert_eq!(
        outcomes,
        ["already_received", "credited"],
        "truthful outcomes"
    );
    let v3 = Token::from_str(&token).unwrap().to_v3_string();
    let again = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&v3)).await;
    assert!(again.status.success());
    assert_eq!(
        outcome(&text(&again).1).as_deref(),
        Some("already_received")
    );
    assert_eq!(c.faults.swaps.load(SeqCst), 1);
    assert_eq!(credited(&f.taker, &c.url), 40);
    assert_secret_free("serve output", &serve.output(), &token, &proofs);
    serve.child.kill().await.unwrap();
}

/// SAFETY: a client that loses serve's reply (killed while the mint swap is in flight) never
/// half-applies the receive: serve finishes it, and a retry credits nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_reply_then_retry_credits_once() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&c, 32).await;
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve").await;
    c.faults.hold_swap.store(true, SeqCst);
    let mut client = spawn_receive(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    tokio::time::timeout(Duration::from_secs(60), c.faults.swap_entered.notified())
        .await
        .expect("swap reached the mint");
    client.kill().await.unwrap();
    c.faults.swap_release.notify_one();
    let r = wait_settled(&f, Duration::from_secs(60)).await;
    assert_eq!(r.net, 32);
    let retry = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    let (stdout, stderr) = text(&retry);
    assert!(retry.status.success(), "{stderr}");
    assert_eq!(c.faults.swaps.load(SeqCst), 1, "SAFETY: no second swap");
    assert_eq!(credited(&f.taker, &c.url), 32, "SAFETY: credited once");
    assert_eq!(
        outcome(&stderr).as_deref(),
        Some("already_received"),
        "{stderr}"
    );
    assert_eq!(summary(&stdout)["receive"], r.id.as_str());
    assert_secret_free("serve output", &serve.output(), &token, &proofs);
    serve.child.kill().await.unwrap();
}

/// SAFETY: an ambiguous mint reply is reported as in doubt (exit 3, the attempt id, how to
/// check), never as a failure that invites a fresh import; serve's recovery then settles it and
/// a retry reports `already_received` with no second swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_mint_reply_is_in_doubt_then_serve_recovers_it() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&c, 24).await;
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve").await;
    c.faults.lose_reply.store(true, SeqCst);
    let first = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    let (stdout, stderr) = text(&first);
    assert_eq!(
        first.status.code(),
        Some(3),
        "SAFETY: in doubt is exit 3: {stderr}"
    );
    assert_eq!(outcome(&stderr).as_deref(), Some("in_doubt"), "{stderr}");
    assert!(stderr.contains("never credits twice"), "{stderr}");
    let id = summary(&stdout)["receive"].as_str().unwrap().to_owned();
    assert_eq!(summary(&stdout)["state"], "submitted");
    assert_eq!(credited(&f.taker, &c.url), 0);
    c.faults.lose_reply.store(false, SeqCst);
    let r = wait_settled(&f, Duration::from_secs(90)).await;
    assert_eq!(r.id, id);
    assert_eq!(
        c.faults.swaps.load(SeqCst),
        1,
        "SAFETY: restore, never a second swap"
    );
    let retry = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    assert!(retry.status.success(), "{}", text(&retry).1);
    assert_eq!(
        outcome(&text(&retry).1).as_deref(),
        Some("already_received")
    );
    assert_eq!(c.faults.swaps.load(SeqCst), 1);
    assert_eq!(credited(&f.taker, &c.url), 24, "SAFETY: credited once");
    assert_secret_free("client", &(stdout + &stderr), &token, &proofs);
    assert_secret_free("serve output", &serve.output(), &token, &proofs);
    serve.child.kill().await.unwrap();
}

/// SAFETY: serve killed while the receive swap is at the mint. The client is told the receive
/// may have been applied (exit 3, attempt id); a restarted serve and a retry credit exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_killed_mid_receive_then_restart_and_retry_credits_once() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&c, 56).await;
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve1").await;
    c.faults.hold_swap.store(true, SeqCst);
    let client = spawn_receive(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    tokio::time::timeout(Duration::from_secs(60), c.faults.swap_entered.notified())
        .await
        .expect("swap reached the mint");
    serve.child.kill().await.unwrap();
    serve.child.wait().await.unwrap();
    // The mint completes the swap it already received.
    c.faults.swap_release.notify_one();
    tokio::time::timeout(Duration::from_secs(30), c.faults.swap_finished.notified())
        .await
        .unwrap();
    let out = tokio::time::timeout(Duration::from_secs(120), client.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(3),
        "SAFETY: in doubt, not failed: {stderr}"
    );
    let r = receipts(&f).await.pop().expect("journaled before POST");
    assert_eq!(r.state, ReceiveState::Submitted);
    assert!(
        stderr.contains("may have been applied") && stderr.contains(&r.id),
        "{stderr}"
    );
    assert_eq!(credited(&f.taker, &c.url), 0);
    assert_secret_free("client", &(stdout + &stderr), &token, &proofs);
    let first_log = serve.output();
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve2").await;
    let retry = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    let (stdout, stderr) = text(&retry);
    assert!(retry.status.success(), "{stderr}");
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert_eq!(c.faults.swaps.load(SeqCst), 1, "SAFETY: no second swap");
    assert_eq!(credited(&f.taker, &c.url), 56, "SAFETY: credited once");
    assert!(
        ["credited", "already_received"].contains(&outcome(&stderr).unwrap().as_str()),
        "{stderr}"
    );
    assert_eq!(summary(&stdout)["receive"], r.id.as_str());
    assert_eq!(receipts(&f).await.len(), 1);
    assert_secret_free(
        "serve output",
        &(first_log + &serve.output()),
        &token,
        &proofs,
    );
    serve.child.kill().await.unwrap();
}

/// A real 128-sat token padded with a memo (part of the token text, not of the swap; mints cap
/// each proof secret at 1024 bytes) to as close to `MAX_TOKEN_BYTES` as encoding allows.
async fn max_size_token(m: &MintFixture) -> (tempfile::TempDir, String, Proofs) {
    let (sender, proofs, _) = issue(m, 128).await;
    let target = receive::MAX_TOKEN_BYTES;
    let with = |n: usize| {
        Token::new(
            MintUrl::from_str(&m.url).unwrap(),
            proofs.clone(),
            Some("m".repeat(n)),
            CurrencyUnit::Sat,
        )
        .to_string()
    };
    let (mut lo, mut hi) = (0usize, target);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if with(mid).len() <= target {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let token = with(lo);
    assert!(
        token.len() <= target && token.len() + 8 > target,
        "{}",
        token.len()
    );
    (sender, token, proofs)
}

/// A token at the largest accepted size goes through serve and is credited; one byte over is
/// refused by the client before serve is contacted; an over-limit socket request is refused by
/// serve without executing anything or echoing its bytes, and serve keeps serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_size_token_accepted_and_over_limit_refused() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_sender, token, proofs) = max_size_token(&c).await;
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve").await;
    // One byte over the limit: refused clearly, never handed to serve.
    let over = f.root.path().join("over.token");
    std::fs::write(
        &over,
        format!("cashuB{}", "A".repeat(receive::MAX_TOKEN_BYTES)),
    )
    .unwrap();
    let refused = receive_cli(&f.taker, &f.relay_url, &c.url, Input::File(&over)).await;
    let (_, stderr) = text(&refused);
    assert_eq!(refused.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("token missing or oversized"), "{stderr}");
    assert!(
        !stderr.contains("handed_to_serve"),
        "SAFETY: never sent: {stderr}"
    );
    assert!(!serve.output().contains("\"serve_request\":\"receive\""));
    // A raw request over serve's frame limit: refused, nothing executed, bytes not echoed.
    {
        let mut s = tokio::net::UnixStream::connect(f.taker.join("serve.sock"))
            .await
            .unwrap();
        let mut junk = vec![b'Q'; maxplayer_trade::serve::MAX_REQUEST_BYTES + 1024];
        junk.push(b'\n');
        let _ = s.write_all(&junk).await;
        // Keep every byte read before serve's close resets the connection (unread input).
        let mut got = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = tokio::time::Instant::now() + Duration::from_secs(15);
        while let Ok(Ok(n @ 1..)) = tokio::time::timeout_at(end, s.read(&mut chunk)).await {
            got.extend_from_slice(&chunk[..n]);
        }
        let reply = String::from_utf8_lossy(&got);
        assert!(reply.contains("nothing executed"), "{reply}");
        assert!(!reply.contains("QQQQ"), "SAFETY: request echoed");
    }
    assert!(serve.child.try_wait().unwrap().is_none(), "serve survives");
    // The largest accepted token is credited through serve.
    let file = f.root.path().join("max.token");
    std::fs::write(&file, &token).unwrap();
    let out = receive_cli(&f.taker, &f.relay_url, &c.url, Input::File(&file)).await;
    let (stdout, stderr) = text(&out);
    assert!(
        out.status.success(),
        "{}\nserve: {}",
        &stderr[..stderr.len().min(4000)],
        serve
            .output()
            .lines()
            .map(maxplayer_trade::redact_reason)
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert_eq!(summary(&stdout)["credited"], 128);
    assert_eq!(credited(&f.taker, &c.url), 128);
    assert_eq!(c.faults.swaps.load(SeqCst), 1);
    assert_secret_free("client", &(stdout + &stderr), &token, &proofs);
    assert_secret_free("serve output", &serve.output(), &token, &proofs);
    serve.child.kill().await.unwrap();
}

/// The socket stays 0600 and same-user; a client that cannot open it is refused (exit 1) and
/// never falls back to writing the home itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_permissions_refuse_and_never_fall_back_to_a_second_writer() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&c, 16).await;
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve").await;
    let sock = f.taker.join("serve.sock");
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "SAFETY: socket mode");
    let home_mode = std::fs::metadata(&f.taker).unwrap().permissions().mode();
    assert_eq!(home_mode & 0o777, 0o700, "SAFETY: home mode");
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("connect to serve socket"), "{stderr}");
    assert!(receipts(&f).await.is_empty(), "SAFETY: nothing journaled");
    assert_eq!(c.faults.swaps.load(SeqCst), 0, "SAFETY: no swap");
    assert_eq!(credited(&f.taker, &c.url), 0);
    assert!(lock_is_held(&f.taker));
    assert_secret_free("client", &(stdout + &stderr), &token, &proofs);
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
    let ok = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    assert!(ok.status.success(), "{}", text(&ok).1);
    assert_eq!(credited(&f.taker, &c.url), 16);
    serve.child.kill().await.unwrap();
}

/// A spent token, an unparsable token and a malformed request carrying token text are refused
/// with a reason that never contains the token, in the client output and in serve's log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spent_and_invalid_tokens_refused_without_leaking() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&c, 32).await;
    // Spent elsewhere first.
    let other = tempfile::tempdir().unwrap();
    let jo = maxplayer_trade::journal::Journal::open(other.path())
        .await
        .unwrap();
    receive::receive(other.path(), &jo, &c.url, &token)
        .await
        .unwrap();
    let swaps = c.faults.swaps.load(SeqCst);
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve").await;
    let spent = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    let (stdout, stderr) = text(&spent);
    assert_eq!(spent.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert!(stderr.contains("not UNSPENT"), "{stderr}");
    assert_secret_free("spent client", &(stdout + &stderr), &token, &proofs);
    assert!(receipts(&f).await.is_empty(), "SAFETY: nothing journaled");
    assert_eq!(c.faults.swaps.load(SeqCst), swaps, "SAFETY: no swap POST");
    assert_eq!(credited(&f.taker, &c.url), 0);
    // Unparsable token-shaped text.
    let junk = format!("cashuB{}", "Zm9vYmFy".repeat(40));
    let bad = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&junk)).await;
    let (stdout, stderr) = text(&bad);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr.contains("contents withheld"), "{stderr}");
    assert!(!(stdout + &stderr).contains(&junk), "SAFETY: token echoed");
    // A malformed request whose mistyped field holds the token: serde would quote it.
    let mut s = tokio::net::UnixStream::connect(f.taker.join("serve.sock"))
        .await
        .unwrap();
    let line = serde_json::json!({"v":token,"relays":[f.relay_url],"command":{"receive":{"mint":c.url,"token":token}}});
    s.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    let mut reply = String::new();
    tokio::time::timeout(Duration::from_secs(15), s.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.contains("invalid serve request"), "{reply}");
    assert_secret_free("serve reply", &reply, &token, &proofs);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = serve.output();
    assert_secret_free("serve output", &log, &token, &proofs);
    assert!(!log.contains(&junk), "SAFETY: junk token in serve log");
    assert!(serve.child.try_wait().unwrap().is_none());
    serve.child.kill().await.unwrap();
}

/// Grep-style: after a credited receive, a duplicate, an in-doubt receive and a refusal through
/// serve, neither token string appears in serve's stdout/stderr or in any file of the home
/// (journal, wallet databases, WAL files).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_text_never_in_serve_output_or_home_files() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s1, p1, t1) = issue(&c, 40).await;
    let (_s2, p2, t2) = issue(&c, 24).await;
    let (_s3, p3, t3) = issue(&c, 8).await;
    let (_s4, p4, t4) = issue(&c, 16).await;
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path(), "serve").await;
    let ok = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&t1)).await;
    assert!(ok.status.success(), "{}", text(&ok).1);
    let dup = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&t1)).await;
    assert!(dup.status.success());
    let wrong = MintFixture::start(0).await;
    // A token of its own for the pre-journal refusal, so the home can be checked for its proofs.
    let refused = receive_cli(&f.taker, &f.relay_url, &wrong.url, Input::Stdin(&t3)).await;
    assert_eq!(refused.status.code(), Some(1));
    // Refused by the mint itself (journaled `refused`): the note names the state plainly.
    c.faults.swap_nut_error.store(true, SeqCst);
    let nut = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&t4)).await;
    c.faults.swap_nut_error.store(false, SeqCst);
    assert_eq!(nut.status.code(), Some(1), "{}", text(&nut).1);
    assert_eq!(outcome(&text(&nut).1).as_deref(), Some("not_credited"));
    assert_eq!(
        note(&text(&nut).1).as_deref(),
        Some("nothing was credited (state refused)")
    );
    c.faults.lose_reply.store(true, SeqCst);
    let doubt = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&t2)).await;
    assert_eq!(doubt.status.code(), Some(3));
    c.faults.lose_reply.store(false, SeqCst);
    let start = std::time::Instant::now();
    while receipts(&f).await.iter().filter(|r| r.settled()).count() < 2 {
        assert!(start.elapsed() < Duration::from_secs(90), "serve recovery");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    serve.child.kill().await.unwrap();
    serve.child.wait().await.unwrap();
    let log = serve.output();
    assert!(
        log.contains("\"serve_request\":\"receive\""),
        "fixture: serve ran them"
    );
    let files = home_bytes(&f.taker);
    // Refused before any journal write: not even its proofs reach the home.
    assert_secret_free("home files", &files, &t3, &p3);
    for (token, proofs) in [(&t1, &p1), (&t2, &p2), (&t3, &p3), (&t4, &p4)] {
        assert_secret_free("serve stdout/stderr", &log, token, proofs);
        for t in [
            token.clone(),
            Token::from_str(token).unwrap().to_v3_string(),
        ] {
            assert!(!files.contains(&t), "SAFETY: token text stored in the home");
        }
        for o in [&ok, &dup, &refused, &nut, &doubt] {
            let (a, b) = text(o);
            assert_secret_free("client", &(a + &b), token, proofs);
        }
    }
    assert_eq!(credited(&f.taker, &c.url), 64);
}

/// SAFETY: with the home locked by another writer and no serve socket, `receive` never becomes a
/// second writer: it is refused with `home is already in use`, as before, and nothing is touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_with_home_locked_and_no_serve_is_refused() {
    use fs2::FileExt;
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&c, 16).await;
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(f.taker.join("owner.lock"))
        .unwrap();
    held.lock_exclusive().unwrap();
    let out = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("home is already in use"), "{stderr}");
    assert!(
        receipts(&f).await.is_empty(),
        "SAFETY: second writer journaled"
    );
    assert_eq!(
        c.faults.swaps.load(SeqCst),
        0,
        "SAFETY: second writer swapped"
    );
    assert_eq!(credited(&f.taker, &c.url), 0);
    assert_secret_free("client", &(stdout + &stderr), &token, &proofs);
    drop(held);
    let ok = receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&token)).await;
    assert!(ok.status.success(), "{}", text(&ok).1);
    assert!(!text(&ok).1.contains("handed_to_serve"));
    assert_eq!(credited(&f.taker, &c.url), 16);
}

/// #1140 F4: a handed-off receive is checked against the relays it actually uses. An HTTP
/// receive never touches the market, so a client with a different (or default) `--relay` set is
/// served instead of refused. A `nostr://` receive uses mint relays: a client whose
/// `--mint-relay` set differs from serve's is refused before anything is queued, journaled or
/// sent (it is never silently run on serve's own mint relays), and the same set is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handed_off_receive_checks_only_the_relays_it_uses() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&c, 16).await;
    let serve_mint_relay = "ws://127.0.0.1:9";
    let mut sc = Command::new(BIN);
    let log = f.root.path().join("serve.stderr");
    sc.arg("--home")
        .arg(&f.taker)
        .args([
            "--relay",
            &f.relay_url,
            "--mint-relay",
            serve_mint_relay,
            "serve",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .kill_on_drop(true);
    let mut serve = sc.spawn().unwrap();
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("{\"serve_socket\":\"")
    {
        assert!(start.elapsed() < Duration::from_secs(30), "serve socket");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let run = |relay: Option<String>, mint_relay: Option<String>, mint: String, input: String| {
        let home = f.taker.clone();
        async move {
            let mut c = Command::new(BIN);
            c.arg("--home").arg(&home);
            if let Some(r) = relay {
                c.args(["--relay", &r]);
            }
            if let Some(r) = mint_relay {
                c.args(["--mint-relay", &r]);
            }
            c.args(["receive", &mint])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let mut child = c.spawn().unwrap();
            let mut i = child.stdin.take().unwrap();
            i.write_all(input.as_bytes()).await.unwrap();
            drop(i);
            tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
                .await
                .expect("receive timed out")
                .unwrap()
        }
    };
    // nostr:// receive with a different mint relay set: refused before queueing.
    let npub = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
    let other = run(
        Some(f.relay_url.clone()),
        Some("ws://127.0.0.1:10".into()),
        npub.into(),
        token.clone(),
    )
    .await;
    let (stdout, stderr) = text(&other);
    assert_eq!(other.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert!(
        stderr.contains("Use the same --mint-relay set as serve"),
        "SAFETY: a nostr:// receive must not run on serve's mint relays: {stderr}"
    );
    assert!(receipts(&f).await.is_empty(), "SAFETY: nothing journaled");
    assert_secret_free("client", &(stdout + &stderr), &token, &proofs);
    // The default mint relays differ from serve's too: refused the same way.
    let default = run(Some(f.relay_url.clone()), None, npub.into(), token.clone()).await;
    assert_eq!(default.status.code(), Some(1));
    assert!(text(&default).1.contains("--mint-relay set"));
    assert!(receipts(&f).await.is_empty());
    // HTTP receive with the default market relays (not serve's): accepted and credited.
    let ok = run(None, None, c.url.clone(), token.clone()).await;
    let (stdout, stderr) = text(&ok);
    assert!(ok.status.success(), "{stderr}");
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert_eq!(outcome(&stderr).as_deref(), Some("credited"), "{stderr}");
    assert_eq!(credited(&f.taker, &c.url), 16);
    assert_eq!(c.faults.swaps.load(SeqCst), 1);
    assert_secret_free("client", &(stdout + &stderr), &token, &proofs);
    // The same mint relay set (another spelling) passes the relay check and reaches the
    // receive itself, which refuses the token for that mint (it was issued by `c`).
    let same = run(
        Some("wss://unused.example".into()),
        Some("ws://127.0.0.1:9/".into()),
        npub.into(),
        token.clone(),
    )
    .await;
    let stderr = text(&same).1;
    assert_eq!(same.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("handed_to_serve"),
        "reached serve: {stderr}"
    );
    assert!(
        stderr.contains("token is for a different mint"),
        "passed the relay check and reached receive: {stderr}"
    );
    assert!(!stderr.contains("--mint-relay set"), "{stderr}");
    assert!(!stderr.contains("--relay list"), "{stderr}");
    assert_eq!(
        credited(&f.taker, &c.url),
        16,
        "SAFETY: nothing more credited"
    );
    let log = std::fs::read_to_string(&log).unwrap();
    assert_secret_free("serve log", &log, &token, &proofs);
    assert!(serve.try_wait().unwrap().is_none());
    serve.kill().await.unwrap();
}

/// A receive whose mint swap hangs holds serve's loop only for its budget (lab: 6 s on loopback):
/// the client is told `in_doubt` (exit 3), serve then serves another receive while that swap is
/// still stuck at the mint (uncredited, and every retry answered 429), and recovery later credits
/// the stuck one exactly once, from that one swap.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_mint_receive_releases_serve_loop_and_is_recovered() {
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let d = MintFixture::start(0).await;
    let (_s1, p1, slow) = issue(&c, 40).await;
    let (_s2, p2, quick) = issue(&d, 24).await;
    // 6 s: room for a cold-wallet receive in a debug build, far below the 20 s HTTP RPC timeout.
    let env = [("TRADE_LAB_SERVE_RECEIVE_SECONDS", "6")];
    let mut serve = start_serve_env(&f.taker, &f.relay_url, f.root.path(), "serve", &env).await;
    c.faults.hold_swap.store(true, SeqCst);
    let started = std::time::Instant::now();
    // Once the first swap POST is held at the mint, every later swap POST (serve's recovery
    // retries) is answered 429 unprocessed, so the held swap stays the only one in flight.
    let (first, ()) = tokio::join!(
        receive_cli(&f.taker, &f.relay_url, &c.url, Input::Stdin(&slow)),
        async {
            c.faults.swap_entered.notified().await;
            c.faults.rate_limit_swap.store(true, SeqCst);
        }
    );
    let (stdout, stderr) = text(&first);
    assert_eq!(
        first.status.code(),
        Some(3),
        "SAFETY: in doubt, not failed: {stderr}"
    );
    assert!(
        // Under the 20 s HTTP RPC timeout: serve's own budget (6 s here) released the loop.
        started.elapsed() < Duration::from_secs(15),
        "SAFETY: a stuck mint held serve's loop"
    );
    assert_eq!(outcome(&stderr).as_deref(), Some("in_doubt"), "{stderr}");
    assert_eq!(summary(&stdout)["state"], "submitted");
    // The swap is still held at the mint; serve answers the next request anyway.
    let second = receive_cli(&f.taker, &f.relay_url, &d.url, Input::Stdin(&quick)).await;
    assert!(second.status.success(), "{}", text(&second).1);
    assert_eq!(credited(&f.taker, &d.url), 24);
    // ...and it was served while the first was still stuck, not after it settled.
    assert_eq!(credited(&f.taker, &c.url), 0, "first still uncredited");
    let stuck = receipts(&f)
        .await
        .into_iter()
        .find(|r| r.mint == c.url)
        .unwrap();
    assert_eq!(stuck.state, ReceiveState::Submitted, "first still in doubt");
    assert_eq!(c.faults.swaps.load(SeqCst), 1, "only the held swap ran");
    // The mint finishes the held swap; recovery credits it from that swap (restore), once.
    c.faults.swap_release.notify_one();
    c.faults.swap_finished.notified().await;
    c.faults.rate_limit_swap.store(false, SeqCst);
    let start = std::time::Instant::now();
    while credited(&f.taker, &c.url) == 0 {
        assert!(start.elapsed() < Duration::from_secs(90), "serve recovery");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(credited(&f.taker, &c.url), 40, "SAFETY: credited once");
    assert_eq!(c.faults.swaps.load(SeqCst), 1, "SAFETY: no second swap");
    assert_eq!(receipts(&f).await.len(), 2);
    let log = serve.output();
    assert_secret_free("client", &(stdout + &stderr), &slow, &p1);
    assert_secret_free("serve output", &log, &slow, &p1);
    assert_secret_free("serve output", &log, &quick, &p2);
    serve.child.kill().await.unwrap();
}

/// #1140 review F1: a recovery pass runs active swaps' claims and refunds before wallet money
/// recovery. With a journaled receive whose mint has stalled and a maker swap whose claim is
/// possible, the claim lands first and well inside its deadline; the receive is retried in the
/// same pass afterwards, stays retained, and later credits exactly once.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_claims_active_swap_before_stalled_receive_mint() {
    let (f, id) = partial_claim_fixture().await;
    let c = MintFixture::start(0).await;
    let (_s, _p, token) = issue(&c, 40).await;
    // A receive in the same home whose swap reply was lost: journaled, owed recovery.
    c.faults.lose_reply.store(true, SeqCst);
    let r = receive::receive(&f.maker, &f.jm, &c.url, &token)
        .await
        .unwrap();
    c.faults.lose_reply.store(false, SeqCst);
    assert_eq!(
        r.state,
        ReceiveState::Submitted,
        "fixture: receive in doubt"
    );
    assert_eq!(f.state(true, &id).await.as_deref(), Some("claiming"));
    let long =
        f.jm.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .quote
            .unwrap()
            .long;
    // The receive mint now stalls: every request sits past the 20 s HTTP RPC timeout.
    c.faults.delay_ms.store(60_000, SeqCst);
    let before = c.faults.requests.load(SeqCst);
    let started = std::time::Instant::now();
    let watch = async {
        loop {
            let state = f.state(true, &id).await;
            if matches!(state.as_deref(), Some("settling" | "complete")) {
                return (
                    c.faults.requests.load(SeqCst),
                    started.elapsed(),
                    coordinator::now(),
                );
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "claim never landed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let (pass, (at_claim, claim_elapsed, claim_now)) =
        tokio::join!(coordinator::recover(&f.maker, &f.jm, &f.mm), watch);
    pass.unwrap();
    assert_eq!(
        at_claim, before,
        "SAFETY: the stalled receive mint was contacted before the swap's claim"
    );
    assert!(
        claim_elapsed < Duration::from_secs(15),
        "SAFETY: claim waited on the stalled receive mint ({claim_elapsed:?})"
    );
    assert!(claim_now < long, "SAFETY: claim landed before its deadline");
    assert!(
        c.faults.requests.load(SeqCst) > before,
        "the same pass still retried the receive after the swaps"
    );
    let r = f.jm.all::<Receipt>("receive").await.unwrap().pop().unwrap();
    assert_eq!(r.state, ReceiveState::Submitted, "SAFETY: receive retained");
    // The mint answers again: the retained receive credits exactly once, with no second swap.
    c.faults.delay_ms.store(0, SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert!(
        f.jm.all::<Receipt>("receive")
            .await
            .unwrap()
            .pop()
            .unwrap()
            .settled()
    );
    assert_eq!(credited(&f.maker, &c.url), 40, "SAFETY: credited once");
    assert_eq!(c.faults.swaps.load(SeqCst), 1, "SAFETY: one swap");
}

/// #1140 review F3: every error line on the receive and serve paths goes through
/// `redact_reason`. A lab fault makes the credit release fail with token- and secret-shaped text;
/// each line that prints it must show only the redaction markers: a new attempt
/// (`receive::receive`), a repeated attempt, `receive::recover` (CLI `recover` and serve's ticks),
/// a receive handed to serve, and serve's relay-mismatch reply and its connection log.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_and_serve_error_lines_redact_token_text() {
    const FAULT: &str = "TRADE_LAB_RECEIVE_RELEASE_ERROR";
    let token_like = format!("cashuB{}", "o2FteBtodHRwOi8v".repeat(4));
    let hex = "ab".repeat(32);
    let leak = format!("{token_like} {hex}");
    let leaked = |what: &str, text: &str| {
        assert!(
            !text.contains(&token_like),
            "SAFETY: token text in {what}: {text}"
        );
        assert!(!text.contains(&hex), "SAFETY: secret hex in {what}: {text}");
    };
    let redacted = "lab fault: release failed: <redacted token> <redacted hex>";
    async fn run(
        home: &Path,
        relay: &str,
        args: &[&str],
        stdin: Option<&str>,
        env: &[(&str, &str)],
    ) -> (String, String) {
        let mut c = cmd(home, relay, args);
        c.envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin.is_some() {
            c.stdin(Stdio::piped());
        }
        let mut child = c.spawn().unwrap();
        if let Some(s) = stdin {
            let mut i = child.stdin.take().unwrap();
            i.write_all(s.as_bytes()).await.unwrap();
            drop(i);
        }
        let out = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        text(&out)
    }
    let f = Fixture::new(0).await;
    let c = MintFixture::start(0).await;
    let (_s1, p1, t1) = issue(&c, 40).await;
    let (_s2, p2, t2) = issue(&c, 24).await;
    let fault = [(FAULT, leak.as_str())];
    let receive = ["receive", c.url.as_str()];
    // receive::receive, new attempt.
    let (out, err) = run(&f.taker, &f.relay_url, &receive, Some(&t1), &fault).await;
    leaked("new attempt", &(out.clone() + &err));
    assert!(
        err.contains(&format!("{redacted}; attempt retained")),
        "{err}"
    );
    // receive::receive, the same attempt again.
    let (out, err) = run(&f.taker, &f.relay_url, &receive, Some(&t1), &fault).await;
    leaked("repeated attempt", &(out.clone() + &err));
    assert!(
        err.contains(&format!("{redacted}; attempt retained")),
        "{err}"
    );
    // receive::recover.
    let (out, err) = run(&f.taker, &f.relay_url, &["recover"], None, &fault).await;
    leaked("recover", &(out.clone() + &err));
    assert!(
        err.contains(&format!("release deferred: {redacted}")),
        "{err}"
    );
    // Through serve: its recovery ticks and a handed-off receive.
    let mut serve = start_serve_env(&f.taker, &f.relay_url, f.root.path(), "serve", &fault).await;
    let (out, err) = run(&f.taker, &f.relay_url, &receive, Some(&t2), &[]).await;
    assert!(err.contains("handed_to_serve"), "{err}");
    leaked("handed-off client", &(out + &err));
    // serve's relay-mismatch reply quotes the requested relays: a mistyped one may be a token.
    let mut s = tokio::net::UnixStream::connect(f.taker.join("serve.sock"))
        .await
        .unwrap();
    let line = serde_json::json!({"v":maxplayer_trade::serve::PROTOCOL_V,"relays":[leak],"mint_relays":[],"command":{"cancel":{"lot":"x"}}});
    s.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    let mut reply = String::new();
    tokio::time::timeout(Duration::from_secs(15), s.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    leaked("serve reply", &reply);
    assert!(reply.contains("this command asked for"), "{reply}");
    assert!(reply.contains("<redacted token>"), "{reply}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = serve.output();
    leaked("serve output", &log);
    assert!(
        log.contains(&format!("{redacted}; attempt retained")),
        "{log}"
    );
    assert!(
        log.contains(&format!("release deferred: {redacted}")),
        "{log}"
    );
    assert!(
        log.contains("\"serve_client\":\"closed\"") && log.contains("this command asked for"),
        "{log}"
    );
    serve.child.kill().await.unwrap();
    serve.child.wait().await.unwrap();
    for (t, p) in [(&t1, &p1), (&t2, &p2)] {
        assert_secret_free("serve output", &log, t, p);
    }
    // Without the fault the retained attempts settle, each credited once.
    let (_, err) = run(&f.taker, &f.relay_url, &["recover"], None, &[]).await;
    assert!(receipts(&f).await.iter().all(Receipt::settled), "{err}");
    assert_eq!(credited(&f.taker, &c.url), 64, "SAFETY: credited once");
    assert_eq!(c.faults.swaps.load(SeqCst), 2, "SAFETY: one swap per token");
}
