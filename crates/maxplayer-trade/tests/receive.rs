//! `receive`: import a Cashu token. In-process fake mints only. SAFETY assertions first.
mod support;
use cashu::nuts::{CurrencyUnit, Proof, Proofs, SecretKey, Token, TokenV3};
use cdk::mint_url::MintUrl;
use maxplayer_trade::{
    journal::Journal,
    money,
    receive::{self, Receipt, ReceiveState},
    wallet,
};
use std::{path::Path, str::FromStr, sync::atomic::Ordering::SeqCst};
use support::*;

/// A token for `n` sats issued to an unrelated sender home (stands in for maxplayer-mint issue).
async fn issue(m: &MintFixture, n: u64) -> (tempfile::TempDir, Proofs, String) {
    let sender = tempfile::tempdir().unwrap();
    fund(sender.path(), &m.url, n).await;
    let proofs = wallet::wallet(sender.path(), &m.url)
        .await
        .unwrap()
        .get_unspent_proofs()
        .await
        .unwrap();
    assert!(
        proofs.iter().all(|p| p.dleq.is_some()),
        "fixture issues DLEQ"
    );
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
async fn home() -> (tempfile::TempDir, Journal) {
    let h = tempfile::tempdir().unwrap();
    let j = Journal::open(h.path()).await.unwrap();
    (h, j)
}
async fn receipts(j: &Journal) -> Vec<Receipt> {
    j.all::<Receipt>("receive").await.unwrap()
}
fn fake(secret: cashu::secret::Secret) -> Proof {
    Proof::new(
        1.into(),
        cashu::nuts::Id::from_str("009a1f293253e41e").unwrap(),
        secret,
        SecretKey::generate().public_key(),
    )
}

#[tokio::test]
async fn happy_path_deducts_input_fee_and_credits_net() {
    let m = MintFixture::start(100).await;
    let (_s, proofs, token) = issue(&m, 100).await;
    let (h, j) = home().await;
    let r = receive::receive(h.path(), &j, &m.url, &token)
        .await
        .unwrap();
    let fee = (proofs.len() as u64 * 100).div_ceil(1000);
    assert_eq!(r.state, ReceiveState::Done, "SAFETY: credited exactly once");
    assert_eq!(m.faults.swaps.load(SeqCst), 1, "SAFETY: exactly one swap");
    assert_eq!((r.amount, r.fee, r.net), (100, fee, 100 - fee));
    assert_eq!(balance(h.path(), &m.url).await, 100 - fee);
    assert_eq!(r.summary()["credited"], 100 - fee);
    // Recovery is idempotent for a terminal receive.
    assert!(!money::recover(h.path(), &j).await.unwrap());
    assert_eq!(balance(h.path(), &m.url).await, 100 - fee);
}

#[tokio::test]
async fn lost_swap_reply_restores_same_outputs_without_second_swap() {
    let m = MintFixture::start(0).await;
    let (_s, _, token) = issue(&m, 64).await;
    let (h, j) = home().await;
    m.faults.lose_reply.store(true, SeqCst);
    let r = receive::receive(h.path(), &j, &m.url, &token)
        .await
        .unwrap();
    assert_eq!(
        r.state,
        ReceiveState::Submitted,
        "SAFETY: ambiguous, not refused"
    );
    assert_eq!(balance(h.path(), &m.url).await, 0);
    assert_eq!(m.faults.swaps.load(SeqCst), 1);
    m.faults.lose_reply.store(false, SeqCst);
    assert!(!money::recover(h.path(), &j).await.unwrap());
    assert_eq!(
        m.faults.swaps.load(SeqCst),
        1,
        "SAFETY: restore, never a second swap"
    );
    assert_eq!(balance(h.path(), &m.url).await, 64, "SAFETY: credited once");
    assert_eq!(receipts(&j).await[0].state, ReceiveState::Done);
    money::recover(h.path(), &j).await.unwrap();
    let again = receive::receive(h.path(), &j, &m.url, &token)
        .await
        .unwrap();
    assert_eq!(again.state, ReceiveState::Done);
    assert_eq!(m.faults.swaps.load(SeqCst), 1);
    assert_eq!(balance(h.path(), &m.url).await, 64);
}

#[tokio::test]
async fn double_receive_swaps_once_credits_once_even_reencoded() {
    let m = MintFixture::start(0).await;
    let (_s, _, token) = issue(&m, 48).await;
    let (h, j) = home().await;
    let first = receive::receive(h.path(), &j, &m.url, &token)
        .await
        .unwrap();
    let v3 = Token::from_str(&token).unwrap().to_v3_string();
    let second = receive::receive(h.path(), &j, &m.url, &v3).await.unwrap();
    assert_eq!(first.id, second.id, "SAFETY: idempotent per Y-set");
    assert_eq!(m.faults.swaps.load(SeqCst), 1, "SAFETY: one swap");
    assert_eq!(balance(h.path(), &m.url).await, 48, "SAFETY: one credit");
    assert_eq!(receipts(&j).await.len(), 1);
}

#[tokio::test]
async fn already_spent_token_refused_and_nothing_credited() {
    let m = MintFixture::start(0).await;
    let (_s, _, token) = issue(&m, 32).await;
    let (first, j1) = home().await;
    receive::receive(first.path(), &j1, &m.url, &token)
        .await
        .unwrap();
    let (h, j) = home().await;
    let swaps = m.faults.swaps.load(SeqCst);
    assert!(
        receive::receive(h.path(), &j, &m.url, &token)
            .await
            .is_err()
    );
    assert_eq!(
        balance(h.path(), &m.url).await,
        0,
        "SAFETY: nothing credited"
    );
    assert_eq!(m.faults.swaps.load(SeqCst), swaps, "SAFETY: no swap POST");
    assert!(receipts(&j).await.is_empty());
}

#[tokio::test]
async fn invalid_tokens_refused_before_journal() {
    let a = MintFixture::start(0).await;
    let b = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&a, 40).await;
    let (h, j) = home().await;
    let mut bad_dleq = proofs.clone();
    for p in &mut bad_dleq {
        p.dleq.as_mut().unwrap().e = SecretKey::generate();
    }
    let locked: cashu::secret::Secret = cashu::nuts::nut10::Secret::from(
        cashu::nuts::SpendingConditions::new_p2pk(SecretKey::generate().public_key(), None),
    )
    .try_into()
    .unwrap();
    let multi = Token::TokenV3(TokenV3 {
        token: vec![
            cashu::nuts::nut00::token::TokenV3Token::new(
                MintUrl::from_str(&a.url).unwrap(),
                proofs[..1].to_vec(),
            ),
            cashu::nuts::nut00::token::TokenV3Token::new(
                MintUrl::from_str(&b.url).unwrap(),
                proofs[1..].to_vec(),
            ),
        ],
        memo: None,
        unit: Some(CurrencyUnit::Sat),
    })
    .to_string();
    let msat = Token::new(
        MintUrl::from_str(&a.url).unwrap(),
        proofs.clone(),
        None,
        CurrencyUnit::Msat,
    )
    .to_string();
    let cases = [
        ("wrong mint", b.url.clone(), token.clone()),
        ("p2pk", a.url.clone(), encode(&a.url, vec![fake(locked)])),
        ("multi-mint", a.url.clone(), multi),
        (
            "129 proofs",
            a.url.clone(),
            encode(
                &a.url,
                (0..129)
                    .map(|_| fake(cashu::secret::Secret::generate()))
                    .collect(),
            ),
        ),
        ("bad dleq", a.url.clone(), encode(&a.url, bad_dleq)),
        ("msat unit", a.url.clone(), msat),
        ("garbage", a.url.clone(), "cashuBnotatoken".into()),
    ];
    for (name, mint, t) in cases {
        let e = receive::receive(h.path(), &j, &mint, &t)
            .await
            .err()
            .unwrap_or_else(|| panic!("SAFETY: {name} must be refused"));
        assert!(receipts(&j).await.is_empty(), "SAFETY: {name} journaled");
        assert!(
            !format!("{e:#}").contains(&t),
            "SAFETY: {name} echoed token"
        );
    }
    assert_eq!(a.faults.swaps.load(SeqCst) + b.faults.swaps.load(SeqCst), 0);
    // The untouched original remains receivable afterwards.
    let r = receive::receive(h.path(), &j, &a.url, &token)
        .await
        .unwrap();
    assert_eq!(r.state, ReceiveState::Done);
}

#[tokio::test]
async fn cap_exceeded_refused_before_post_and_shared_with_funding() {
    let m = MintFixture::start(0).await;
    let (_s, _, big) = issue(&m, 100_001).await;
    let (h, j) = home().await;
    assert!(receive::receive(h.path(), &j, &m.url, &big).await.is_err());
    assert_eq!(m.faults.swaps.load(SeqCst), 0, "SAFETY: no POST over cap");
    assert!(receipts(&j).await.is_empty());
    // Cumulative: a retained 99,990-sat funding intent leaves room for at most 10.
    money::fund(h.path(), &j, &m.url, 99_990).await.unwrap();
    let (_s2, _, small) = issue(&m, 64).await;
    let e = receive::receive(h.path(), &j, &m.url, &small)
        .await
        .err()
        .expect("SAFETY: cumulative cap");
    assert!(format!("{e:#}").contains("cumulative"));
    assert_eq!(m.faults.swaps.load(SeqCst), 0);
    // Receives also count toward later funding.
    let (h2, j2) = home().await;
    receive::receive(h2.path(), &j2, &m.url, &small)
        .await
        .unwrap();
    assert!(money::fund(h2.path(), &j2, &m.url, 99_950).await.is_err());
    assert!(money::fund(h2.path(), &j2, &m.url, 99_936).await.is_ok());
}

fn run(
    home: &Path,
    mint: &str,
    file: Option<&Path>,
    stdin: Option<&str>,
    env: Option<&str>,
) -> std::process::Output {
    use std::io::Write;
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"));
    c.args(["--home", home.to_str().unwrap(), "receive", mint]);
    if let Some(f) = file {
        c.args(["--token-file", f.to_str().unwrap()]);
    }
    if let Some(var) = env {
        c.env(var, "1");
    }
    c.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(s) = stdin {
        input.write_all(s.as_bytes()).unwrap();
    }
    drop(input);
    child.wait_with_output().unwrap()
}
async fn cli(
    home: &Path,
    mint: &str,
    file: Option<&Path>,
    stdin: Option<&str>,
    env: Option<&'static str>,
) -> std::process::Output {
    let (home, mint) = (home.to_owned(), mint.to_owned());
    let (file, stdin) = (file.map(Path::to_owned), stdin.map(str::to_owned));
    tokio::task::spawn_blocking(move || run(&home, &mint, file.as_deref(), stdin.as_deref(), env))
        .await
        .unwrap()
}
fn assert_secret_free(out: &std::process::Output, token: &str, proofs: &Proofs) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!text.contains(token), "SAFETY: token printed");
    for p in proofs {
        assert!(
            !text.contains(&p.secret.to_string()),
            "SAFETY: secret printed"
        );
        assert!(!text.contains(&p.c.to_hex()), "SAFETY: proof C printed");
    }
}

#[tokio::test]
async fn cli_never_prints_token_and_never_takes_it_as_argv() {
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args(["receive", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(
        help.contains("receive [OPTIONS] <MINT>\n"),
        "SAFETY: the only positional is the mint: {help}"
    );
    let arguments = help
        .split("Arguments:")
        .nth(1)
        .unwrap()
        .split("Options:")
        .next()
        .unwrap();
    assert_eq!(
        arguments.split_whitespace().collect::<Vec<_>>(),
        ["<MINT>"],
        "SAFETY: no token positional"
    );
    assert!(help.contains("--token-file"));
    let m = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&m, 24).await;
    let (h, _j) = home().await;
    let file = h.path().join("in.token");
    std::fs::write(&file, format!("{token}\n")).unwrap();
    // Wrong mint first: a refusal must not echo the token either.
    let other = MintFixture::start(0).await;
    let refused = cli(h.path(), &other.url, Some(&file), None, None).await;
    assert_eq!(refused.status.code(), Some(1));
    assert_secret_free(&refused, &token, &proofs);
    let out = cli(h.path(), &m.url, Some(&file), None, None).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_secret_free(&out, &token, &proofs);
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        (summary["state"].as_str(), summary["net"].as_u64()),
        (Some("done"), Some(24))
    );
    // Stdin path: same token is idempotent, still secret-free.
    let again = cli(h.path(), &m.url, None, Some(&token), None).await;
    assert!(again.status.success());
    assert_secret_free(&again, &token, &proofs);
    assert_eq!(m.faults.swaps.load(SeqCst), 1);
    // Lost reply exits 3 (unresolved), not 1.
    let (_s2, proofs2, token2) = issue(&m, 16).await;
    m.faults.lose_reply.store(true, SeqCst);
    let pending = cli(h.path(), &m.url, None, Some(&token2), None).await;
    assert_eq!(
        pending.status.code(),
        Some(3),
        "SAFETY: ambiguous is exit 3"
    );
    assert_secret_free(&pending, &token2, &proofs2);
    m.faults.lose_reply.store(false, SeqCst);
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args(["--home", h.path().to_str().unwrap(), "status", "--json"])
        .output()
        .unwrap();
    assert_secret_free(&status, &token2, &proofs2);
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"receive\""));
    // The default readable summary is secret-free too and lists the unresolved receive.
    let human = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args(["--home", h.path().to_str().unwrap(), "status"])
        .output()
        .unwrap();
    assert_secret_free(&human, &token2, &proofs2);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.starts_with("UNRESOLVED ("), "{text}");
    assert!(text.contains("receive "), "{text}");
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn crash_after_journal_before_post_then_recover_completes() {
    let m = MintFixture::start(0).await;
    let (_s, proofs, token) = issue(&m, 56).await;
    let (h, j) = home().await;
    let out = cli(
        h.path(),
        &m.url,
        None,
        Some(&token),
        Some("TRADE_CRASH_BEFORE_RECEIVE_SWAP"),
    )
    .await;
    assert_eq!(out.status.code(), Some(87));
    assert_secret_free(&out, &token, &proofs);
    assert_eq!(
        m.faults.swaps.load(SeqCst),
        0,
        "SAFETY: crashed before POST"
    );
    assert_eq!(receipts(&j).await.len(), 1, "SAFETY: journaled before POST");
    assert!(!money::recover(h.path(), &j).await.unwrap());
    assert_eq!(m.faults.swaps.load(SeqCst), 1);
    assert_eq!(balance(h.path(), &m.url).await, 56);
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn crash_after_post_before_wallet_commit_then_recover_completes() {
    let m = MintFixture::start(0).await;
    let (_s, _, token) = issue(&m, 56).await;
    let (h, j) = home().await;
    let out = cli(
        h.path(),
        &m.url,
        None,
        Some(&token),
        Some("TRADE_CRASH_AFTER_RECEIVE_SWAP"),
    )
    .await;
    assert_eq!(out.status.code(), Some(86));
    assert_eq!(m.faults.swaps.load(SeqCst), 1);
    assert_eq!(balance(h.path(), &m.url).await, 0, "crashed before credit");
    assert!(!money::recover(h.path(), &j).await.unwrap());
    assert_eq!(m.faults.swaps.load(SeqCst), 1, "SAFETY: no second swap");
    assert_eq!(balance(h.path(), &m.url).await, 56, "SAFETY: credited once");
    money::recover(h.path(), &j).await.unwrap();
    assert_eq!(balance(h.path(), &m.url).await, 56);
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn journaled_token_spent_elsewhere_is_terminal_already_spent() {
    let m = MintFixture::start(0).await;
    let (_s, _, token) = issue(&m, 40).await;
    let (h, j) = home().await;
    let out = cli(
        h.path(),
        &m.url,
        None,
        Some(&token),
        Some("TRADE_CRASH_BEFORE_RECEIVE_SWAP"),
    )
    .await;
    assert_eq!(out.status.code(), Some(87));
    let (other, jo) = home().await;
    receive::receive(other.path(), &jo, &m.url, &token)
        .await
        .unwrap();
    let swaps = m.faults.swaps.load(SeqCst);
    assert!(!money::recover(h.path(), &j).await.unwrap());
    assert_eq!(receipts(&j).await[0].state, ReceiveState::AlreadySpent);
    assert_eq!(
        m.faults.swaps.load(SeqCst),
        swaps,
        "SAFETY: no POST of spent inputs"
    );
    assert_eq!(
        balance(h.path(), &m.url).await,
        0,
        "SAFETY: nothing credited"
    );
}

#[tokio::test]
async fn definitive_refusal_is_terminal_refused() {
    let m = MintFixture::start(0).await;
    let (_s, _, token) = issue(&m, 40).await;
    let (h, j) = home().await;
    // A NUT error code from the swap with inputs still UNSPENT.
    m.faults.swap_nut_error.store(true, SeqCst);
    let r = receive::receive(h.path(), &j, &m.url, &token)
        .await
        .unwrap();
    assert_eq!(r.state, ReceiveState::Refused, "SAFETY: definitive refusal");
    assert_eq!(balance(h.path(), &m.url).await, 0);
    assert!(!money::recover(h.path(), &j).await.unwrap(), "terminal");
    assert_eq!(
        m.faults.swaps.load(SeqCst),
        1,
        "SAFETY: no replay after refusal"
    );
}

#[tokio::test]
async fn invalid_returned_dleq_quarantines_without_credit() {
    let m = MintFixture::start(0).await;
    let (_s, _, token) = issue(&m, 40).await;
    let (h, j) = home().await;
    m.faults.invalid_dleq.store(true, SeqCst);
    let r = receive::receive(h.path(), &j, &m.url, &token)
        .await
        .unwrap();
    assert_eq!(
        r.state,
        ReceiveState::Quarantined,
        "SAFETY: never credit bad DLEQ"
    );
    assert_eq!(balance(h.path(), &m.url).await, 0);
    m.faults.invalid_dleq.store(false, SeqCst);
    money::recover(h.path(), &j).await.unwrap();
    assert_eq!(
        balance(h.path(), &m.url).await,
        0,
        "SAFETY: quarantine terminal"
    );
}

#[tokio::test]
async fn omitted_returned_dleq_quarantines_without_credit() {
    let m = MintFixture::start(0).await;
    let (_s, _, token) = issue(&m, 40).await;
    let (h, j) = home().await;
    m.faults.omit_dleq.store(true, SeqCst);
    let r = receive::receive(h.path(), &j, &m.url, &token)
        .await
        .unwrap();
    assert_eq!(
        r.state,
        ReceiveState::Quarantined,
        "SAFETY: never credit proofs without DLEQ"
    );
    assert_eq!(balance(h.path(), &m.url).await, 0);
    assert_eq!(r.summary()["credited"], 0);
}
