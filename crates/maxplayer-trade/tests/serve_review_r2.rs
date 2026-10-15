//! Review of #1157, round 2 (cashu-rust-dev): regression tests for M-A and M-B.
//! existing `support` module). Loopback fake mints, a local relay and disposable homes only.
//!
//! - `r4_*`: a funding whose quote failed its own checks (NUT-20 binding, invoice amount) is
//!   journaled before those checks run, so a re-run with the same `--id` prints that invoice.
//!   These assert the correct behaviour, so they FAIL on 6862428 / bd7227d (finding M-A).
//! - `r5_*`: a token file that a consumer moves away as soon as it appears is reported as "not
//!   the file serve wrote", and the client is told to `--reclaim` a delivered token (finding M-B).
//!   Timing-dependent; it failed on every run here.
mod support;
use std::{path::Path, process::Stdio, time::Duration};
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
async fn start_serve(home: &Path, relay: &str, dir: &Path) -> Child {
    let log = dir.join("serve-r2.stderr");
    let child = Command::new(BIN)
        .arg("--home")
        .arg(home)
        .args(["--relay", relay, "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("{\"serve_socket\":\"")
    {
        assert!(start.elapsed() < Duration::from_secs(30), "serve socket");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    child
}
fn balance(home: &Path, mint: &str) -> u64 {
    maxplayer_trade::wallet::read_balance(home, mint).unwrap()
}

/// Journal exactly the row `money::quote` writes after the quote POST and BEFORE its checks
/// (`quote`/`invoice` set, nothing else), for a quote the check would refuse; then run
/// `fund --id` through serve, as the client's own pre-printed advice says to.
async fn rerun_after_failed_check(quote_amount: u64, bind_key: bool) {
    let f = Fixture::new(0).await;
    let mint = f.b.url.clone();
    let key = "11".repeat(32);
    let pubkey = key.parse::<cdk::nuts::SecretKey>().unwrap().public_key();
    let body = if bind_key {
        serde_json::json!({"amount":quote_amount,"unit":"sat","pubkey":pubkey})
    } else {
        // A mint that ignores NUT-20 answers without "pubkey": the check refuses it.
        serde_json::json!({"amount":quote_amount,"unit":"sat"})
    };
    let q: serde_json::Value = maxplayer_trade::mint::rpc(&mint, "mint/quote/bolt11", &body)
        .await
        .unwrap();
    let (quote, invoice) = (
        q["quote"].as_str().unwrap().to_owned(),
        q["request"].as_str().unwrap().to_owned(),
    );
    let id = uuid::Uuid::new_v4().to_string();
    let row = serde_json::json!({"id":id,"mint":mint,"amount":10,"quote":quote,"invoice":invoice,
        "done":false,"expired_unpaid":false,"quote_key":key,"outputs":[],"result":null,
        "op":uuid::Uuid::new_v4().to_string()});
    f.jt.put("funding", &id, &row).await.unwrap();
    let before = balance(&f.taker, &mint);
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path()).await;
    let o = run(
        &f.taker,
        &f.relay_url,
        &["fund", &mint, "--amount", "10", "--id", &id],
    )
    .await;
    let (stdout, stderr) = text(&o);
    assert!(
        !stdout.contains(&invoice),
        "SAFETY: re-run with --id printed an invoice that failed the funding checks \
         (exit {:?}):\n{stdout}\n{stderr}",
        o.status.code()
    );
    assert_eq!(balance(&f.taker, &mint), before);
    serve.kill().await.unwrap();
}

/// M-A: the mint answered a 10-sat funding with a 20-sat invoice. The first run refuses it
/// ("funding invoice amount mismatch"); the same `--id` again must not show it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_fund_id_rerun_never_shows_an_invoice_with_the_wrong_amount() {
    rerun_after_failed_check(20, true).await;
}

/// M-A: the mint did not bind the quote to the NUT-20 key. The first run refuses it ("mint did
/// not bind funding quote to NUT-20 key"); the same `--id` again must not show it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_fund_id_rerun_never_shows_an_unbound_quote() {
    rerun_after_failed_check(10, false).await;
}

/// M-B: an outbox consumer moves the token file away as soon as it appears (same filesystem,
/// same view). The send succeeded and the token was delivered; the client must not tell the
/// user to reclaim it as if serve had written it somewhere the client cannot see.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_consumed_token_file_is_not_reported_as_another_filesystem_view() {
    let f = Fixture::new(0).await;
    let mint = f.b.url.clone();
    let mut serve = start_serve(&f.taker, &f.relay_url, f.root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("t.token");
    let delivered = dir.path().join("delivered.token");
    let watch = {
        let (out, delivered) = (out.clone(), delivered.clone());
        tokio::spawn(async move {
            loop {
                if std::fs::rename(&out, &delivered).is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
    };
    let o = run(
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
    tokio::time::timeout(Duration::from_secs(5), watch)
        .await
        .expect("token file never appeared")
        .unwrap();
    assert!(
        std::fs::read_to_string(&delivered)
            .unwrap()
            .starts_with("cashu")
    );
    let (_, stderr) = text(&o);
    assert!(
        !stderr.contains("--reclaim"),
        "a delivered token must not be sent to --reclaim (exit {:?}):\n{stderr}",
        o.status.code()
    );
    serve.kill().await.unwrap();
}
