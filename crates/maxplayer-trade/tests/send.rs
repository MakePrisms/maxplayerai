//! `send`: export a Cashu token file. In-process fake mints only. SAFETY assertions first.
mod support;
use cashu::nuts::{CurrencyUnit, Proof, SecretKey, State, Token};
use cdk::{cdk_database::WalletDatabase, mint_url::MintUrl};
use cdk_common::wallet::ProofInfo;
use maxplayer_trade::{
    journal::Journal,
    money, receive,
    send::{self, SendAttempt, SendState},
    wallet,
};
use std::{os::unix::fs::PermissionsExt, path::Path, str::FromStr, sync::atomic::Ordering::SeqCst};
use support::*;

async fn home() -> (tempfile::TempDir, Journal) {
    let h = tempfile::tempdir().unwrap();
    let j = Journal::open(h.path()).await.unwrap();
    (h, j)
}
async fn funded(m: &MintFixture, n: u64) -> (tempfile::TempDir, Journal) {
    let (h, j) = home().await;
    fund(h.path(), &m.url, n).await;
    (h, j)
}
async fn attempts(j: &Journal) -> Vec<SendAttempt> {
    j.all::<SendAttempt>("send").await.unwrap()
}
/// The privately journaled token (test-only read of the raw record).
async fn journaled_token(j: &Journal, id: &str) -> Option<String> {
    j.get::<serde_json::Value>("send", id)
        .await
        .unwrap()
        .unwrap()["token"]
        .as_str()
        .map(str::to_owned)
}
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().trim().to_owned()
}
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn send_then_receive_moves_balances_exactly_with_quoted_fees() {
    let m = MintFixture::start(100).await;
    let (a, ja) = funded(&m, 100).await;
    let out = a.path().join("out.token");
    let r = send::send(a.path(), &ja, &m.url, 40, &out, Some(5))
        .await
        .unwrap();
    assert_eq!(r.state, SendState::Sent, "SAFETY: sent once");
    assert_eq!(m.faults.swaps.load(SeqCst), 1, "SAFETY: exactly one swap");
    assert_eq!(mode(&out), 0o600, "SAFETY: token file is 0600");
    let token = read(&out);
    assert!(token.starts_with("cashuB"), "V4 token");
    assert_eq!(Some(token.clone()), journaled_token(&ja, &r.id).await);
    let t = Token::from_str(&token).unwrap();
    assert_eq!(u64::from(t.value().unwrap()), 40, "SAFETY: exact amount");
    assert!(r.fee > 0 && r.fee <= 5, "fee charged to the sender");
    assert_eq!(
        balance(a.path(), &m.url).await,
        100 - 40 - r.fee,
        "SAFETY: sender debited amount + fee"
    );
    let (b, jb) = home().await;
    let got = receive::receive(b.path(), &jb, &m.url, &token)
        .await
        .unwrap();
    assert_eq!(got.state, receive::ReceiveState::Done);
    assert_eq!(got.amount, 40);
    assert_eq!(balance(b.path(), &m.url).await, 40 - got.fee);
    // Rerunning the same command is idempotent: no second send.
    let again = send::send(a.path(), &ja, &m.url, 40, &out, Some(5))
        .await
        .unwrap();
    assert_eq!(again.id, r.id, "SAFETY: same attempt");
    assert_eq!(attempts(&ja).await.len(), 1);
    assert_eq!(balance(a.path(), &m.url).await, 100 - 40 - r.fee);
    assert!(!money::recover(a.path(), &ja).await.unwrap());
}

#[tokio::test]
async fn lost_swap_reply_restores_same_outputs_without_second_swap() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 64).await;
    let out = a.path().join("t");
    m.faults.lose_reply.store(true, SeqCst);
    let r = send::send(a.path(), &ja, &m.url, 24, &out, None)
        .await
        .unwrap();
    assert_eq!(r.state, SendState::Submitted, "SAFETY: ambiguous");
    assert!(!out.exists(), "SAFETY: no token before a definitive swap");
    let w = wallet::wallet(a.path(), &m.url).await.unwrap();
    let held = u64::from(w.total_reserved_balance().await.unwrap());
    assert!(held >= 24, "SAFETY: inputs stay reserved while ambiguous");
    assert_eq!(balance(a.path(), &m.url).await + held, 64, "no change yet");
    m.faults.lose_reply.store(false, SeqCst);
    // Rerun with the same --out resumes the SAME attempt.
    let again = send::send(a.path(), &ja, &m.url, 24, &out, None)
        .await
        .unwrap();
    assert_eq!(again.id, r.id, "SAFETY: resumed, not a new send");
    assert_eq!(again.state, SendState::Sent);
    assert_eq!(
        m.faults.swaps.load(SeqCst),
        1,
        "SAFETY: restore, no 2nd swap"
    );
    assert_eq!(balance(a.path(), &m.url).await, 40, "SAFETY: change once");
    assert_eq!(Some(read(&out)), journaled_token(&ja, &r.id).await);
}

#[tokio::test]
async fn lost_reply_resolved_by_restore_alone_without_nut07() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 64).await;
    let out = a.path().join("t");
    m.faults.lose_reply.store(true, SeqCst);
    let r = send::send(a.path(), &ja, &m.url, 64, &out, None)
        .await
        .unwrap();
    assert_eq!(r.state, SendState::Submitted);
    m.faults.lose_reply.store(false, SeqCst);
    // NUT-07 is down: restore of the SAME outputs must settle it on its own.
    m.faults.reject_checkstate.store(true, SeqCst);
    let done = send::send(a.path(), &ja, &m.url, 64, &out, None)
        .await
        .unwrap();
    assert_eq!(
        done.state,
        SendState::Sent,
        "SAFETY: restore is checked first"
    );
    assert_eq!(m.faults.swaps.load(SeqCst), 1, "SAFETY: no second swap");
    assert_eq!(Some(read(&out)), journaled_token(&ja, &r.id).await);
}

#[tokio::test]
async fn over_cap_refused_even_with_balance() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 100_010).await;
    let out = a.path().join("t");
    let e = send::send(a.path(), &ja, &m.url, 100_001, &out, None)
        .await
        .err()
        .expect("SAFETY: per-send cap");
    assert!(format!("{e:#}").contains("100,000"), "{e:#}");
    assert!(attempts(&ja).await.is_empty() && !out.exists());
    assert_eq!(m.faults.swaps.load(SeqCst), 0);
    let ok = send::send(a.path(), &ja, &m.url, 100_000, &out, None)
        .await
        .unwrap();
    assert_eq!(ok.state, SendState::Sent, "cap is inclusive");
}

#[tokio::test]
async fn reclaim_after_partial_redemption_returns_only_the_rest() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 3).await;
    let out = a.path().join("t");
    let r = send::send(a.path(), &ja, &m.url, 3, &out, None)
        .await
        .unwrap();
    let proofs = Token::from_str(&read(&out))
        .unwrap()
        .proofs(
            &wallet::wallet(a.path(), &m.url)
                .await
                .unwrap()
                .get_mint_keysets(cdk::wallet::KeysetFilter::All)
                .await
                .unwrap(),
        )
        .unwrap();
    assert_eq!(proofs.len(), 2, "fixture: 1 + 2");
    let one: Vec<_> = proofs
        .into_iter()
        .filter(|p| u64::from(p.amount) == 1)
        .collect();
    let part = Token::new(
        MintUrl::from_str(&m.url).unwrap(),
        one,
        None,
        CurrencyUnit::Sat,
    );
    let (b, jb) = home().await;
    receive::receive(b.path(), &jb, &m.url, &part.to_string())
        .await
        .unwrap();
    let back = send::reclaim(a.path(), &ja, &r.id, None).await.unwrap();
    assert_eq!(
        back.state,
        SendState::Reclaimed,
        "SAFETY: unspent rest reclaimed"
    );
    assert_eq!(back.reclaimed, 2, "SAFETY: only the unredeemed proofs");
    assert_eq!(balance(a.path(), &m.url).await, 2);
}

#[tokio::test]
async fn refusals_have_no_side_effects() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 64).await;
    let before = balance(a.path(), &m.url).await;
    let check = |name: &'static str| {
        let (ja, a, m) = (&ja, a.path().to_owned(), &m);
        async move {
            assert!(attempts(ja).await.is_empty(), "SAFETY: {name} journaled");
            assert_eq!(m.faults.swaps.load(SeqCst), 0, "SAFETY: {name} swapped");
            assert_eq!(balance(&a, &m.url).await, before, "{name} balance");
        }
    };
    let out = a.path().join("t");
    assert!(
        send::send(a.path(), &ja, &m.url, 65, &out, None)
            .await
            .is_err()
    );
    check("insufficient").await;
    assert!(
        send::send(a.path(), &ja, &m.url, 100_001, &out, None)
            .await
            .is_err()
    );
    check("over cap").await;
    std::fs::write(&out, "precious").unwrap();
    assert!(
        send::send(a.path(), &ja, &m.url, 8, &out, None)
            .await
            .is_err()
    );
    assert_eq!(read(&out), "precious", "SAFETY: never overwritten");
    check("existing --out").await;
    let dangling = a.path().join("link");
    std::os::unix::fs::symlink(a.path().join("nowhere"), &dangling).unwrap();
    assert!(
        send::send(a.path(), &ja, &m.url, 8, &dangling, None)
            .await
            .is_err()
    );
    check("symlink --out").await;
    // Reserved and locked proofs are not spendable for a send.
    let db = wallet::database(a.path(), &m.url).await.unwrap();
    let rows = db.get_proofs(None, None, None, None).await.unwrap();
    let biggest = rows.iter().max_by_key(|p| p.proof.amount).unwrap();
    let held = u64::from(biggest.proof.amount);
    db.reserve_proofs(vec![biggest.y], &uuid::Uuid::new_v4())
        .await
        .unwrap();
    let locked: cashu::secret::Secret = cashu::nuts::nut10::Secret::from(
        cashu::nuts::SpendingConditions::new_p2pk(SecretKey::generate().public_key(), None),
    )
    .try_into()
    .unwrap();
    let fake = Proof::new(
        1024.into(),
        rows[0].proof.keyset_id,
        locked,
        SecretKey::generate().public_key(),
    );
    // A row held by another operation but still marked UNSPENT is excluded too.
    let held_row = Proof::new(
        2048.into(),
        rows[0].proof.keyset_id,
        cashu::secret::Secret::generate(),
        SecretKey::generate().public_key(),
    );
    let op = uuid::Uuid::new_v4();
    db.update_proofs(
        vec![
            ProofInfo::new(
                fake,
                MintUrl::from_str(&m.url).unwrap(),
                State::Unspent,
                CurrencyUnit::Sat,
            )
            .unwrap(),
            ProofInfo::new_with_operations(
                held_row,
                MintUrl::from_str(&m.url).unwrap(),
                State::Unspent,
                CurrencyUnit::Sat,
                Some(op),
                Some(op),
            )
            .unwrap(),
        ],
        vec![],
    )
    .await
    .unwrap();
    let out2 = a.path().join("t2");
    let e = send::send(a.path(), &ja, &m.url, 64 - held + 1, &out2, None)
        .await
        .err()
        .expect("SAFETY: reserved/locked proofs excluded");
    assert!(format!("{e:#}").contains("insufficient"), "{e:#}");
    assert!(attempts(&ja).await.is_empty() && !out2.exists());
    assert_eq!(m.faults.swaps.load(SeqCst), 0);
    // An exact-fit send still works from the remaining ordinary balance and leaves
    // the reserved proof reserved.
    let ok = send::send(a.path(), &ja, &m.url, 64 - held, &out2, None)
        .await
        .unwrap();
    assert_eq!(ok.state, SendState::Sent);
    assert_eq!(
        db.get_proofs_by_ys(vec![biggest.y]).await.unwrap()[0].state,
        State::Reserved,
        "SAFETY: reserved proof untouched"
    );
}

#[tokio::test]
async fn fee_mint_requires_max_fees_and_honours_it() {
    let m = MintFixture::start(1000).await;
    let (a, ja) = funded(&m, 64).await;
    let out = a.path().join("t");
    let e = send::send(a.path(), &ja, &m.url, 16, &out, None)
        .await
        .err()
        .unwrap();
    assert!(format!("{e:#}").contains("--max-fees"));
    assert!(
        send::send(a.path(), &ja, &m.url, 16, &out, Some(0))
            .await
            .is_err()
    );
    assert!(attempts(&ja).await.is_empty(), "SAFETY: no journal intent");
    assert_eq!(m.faults.swaps.load(SeqCst), 0);
    let r = send::send(a.path(), &ja, &m.url, 16, &out, Some(16))
        .await
        .unwrap();
    assert_eq!(r.state, SendState::Sent);
    assert_eq!(balance(a.path(), &m.url).await, 64 - 16 - r.fee);
}

#[tokio::test]
async fn reclaim_unredeemed_restores_balance_and_redeemed_refunds_nothing() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 64).await;
    let out = a.path().join("t");
    let r = send::send(a.path(), &ja, &m.url, 32, &out, None)
        .await
        .unwrap();
    assert_eq!(balance(a.path(), &m.url).await, 32);
    let back = send::reclaim(a.path(), &ja, &r.id, None).await.unwrap();
    assert_eq!(back.state, SendState::Reclaimed, "SAFETY: reclaimed");
    assert_eq!(back.reclaimed, 32);
    assert_eq!(
        balance(a.path(), &m.url).await,
        64,
        "SAFETY: balance restored"
    );
    // The old token is now worthless to the recipient.
    let (b, jb) = home().await;
    assert!(
        receive::receive(b.path(), &jb, &m.url, &read(&out))
            .await
            .is_err()
    );
    // Reclaim twice: no second credit.
    send::reclaim(a.path(), &ja, &r.id, None).await.unwrap();
    assert_eq!(balance(a.path(), &m.url).await, 64);

    let out2 = a.path().join("t2");
    let r2 = send::send(a.path(), &ja, &m.url, 16, &out2, None)
        .await
        .unwrap();
    receive::receive(b.path(), &jb, &m.url, &read(&out2))
        .await
        .unwrap();
    let swaps = m.faults.swaps.load(SeqCst);
    let late = send::reclaim(a.path(), &ja, &r2.id, None).await.unwrap();
    assert_eq!(
        late.state,
        SendState::Redeemed,
        "SAFETY: redeemed by recipient"
    );
    assert_eq!(late.reclaimed, 0);
    assert_eq!(m.faults.swaps.load(SeqCst), swaps, "SAFETY: no swap");
    assert_eq!(
        balance(a.path(), &m.url).await,
        48,
        "SAFETY: nothing refunded"
    );
}

#[tokio::test]
async fn lost_reclaim_reply_restores_without_second_swap() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 32).await;
    let r = send::send(a.path(), &ja, &m.url, 32, &a.path().join("t"), None)
        .await
        .unwrap();
    m.faults.lose_reply.store(true, SeqCst);
    let pending = send::reclaim(a.path(), &ja, &r.id, None).await.unwrap();
    assert_eq!(pending.state, SendState::Reclaiming, "SAFETY: ambiguous");
    m.faults.lose_reply.store(false, SeqCst);
    let swaps = m.faults.swaps.load(SeqCst);
    assert!(!money::recover(a.path(), &ja).await.unwrap());
    assert_eq!(m.faults.swaps.load(SeqCst), swaps, "SAFETY: restored");
    assert_eq!(balance(a.path(), &m.url).await, 32, "SAFETY: credited once");
}

#[tokio::test]
async fn invalid_or_omitted_dleq_quarantines_and_writes_no_token() {
    for omit in [false, true] {
        let m = MintFixture::start(0).await;
        let (a, ja) = funded(&m, 32).await;
        let out = a.path().join("t");
        if omit {
            m.faults.omit_dleq.store(true, SeqCst);
        } else {
            m.faults.invalid_dleq.store(true, SeqCst);
        }
        let r = send::send(a.path(), &ja, &m.url, 8, &out, None)
            .await
            .unwrap();
        assert_eq!(r.state, SendState::Quarantined, "SAFETY: quarantined");
        assert!(!out.exists(), "SAFETY: no unverified token written");
        assert!(r.summary()["manual_recovery"].as_bool().unwrap());
    }
}

#[tokio::test]
async fn definitive_refusal_releases_inputs_and_is_terminal() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 32).await;
    m.faults.swap_nut_error.store(true, SeqCst);
    let out = a.path().join("t");
    let r = send::send(a.path(), &ja, &m.url, 8, &out, None)
        .await
        .unwrap();
    assert_eq!(r.state, SendState::Refused);
    assert!(!out.exists());
    assert_eq!(
        balance(a.path(), &m.url).await,
        32,
        "SAFETY: inputs released"
    );
    m.faults.swap_nut_error.store(false, SeqCst);
    assert!(!money::recover(a.path(), &ja).await.unwrap());
    assert_eq!(m.faults.swaps.load(SeqCst), 1, "SAFETY: never replayed");
}

fn run(home: &Path, args: &[&str], env: Option<&str>) -> std::process::Output {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"));
    c.args(["--home", home.to_str().unwrap()]).args(args);
    if let Some(var) = env {
        c.env(var, "1");
    }
    c.stdin(std::process::Stdio::null()).output().unwrap()
}
async fn cli(home: &Path, args: &[&str], env: Option<&'static str>) -> std::process::Output {
    let home = home.to_owned();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        run(
            &home,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            env,
        )
    })
    .await
    .unwrap()
}
fn assert_secret_free(out: &std::process::Output, token: &str) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!text.contains(token), "SAFETY: token printed");
    for p in Token::from_str(token).unwrap().token_secrets() {
        assert!(!text.contains(&p.to_string()), "SAFETY: secret printed");
    }
}

#[tokio::test]
async fn cli_never_prints_or_takes_the_token_and_reports_redemption() {
    let help = String::from_utf8(run(Path::new("/"), &["send", "--help"], None).stdout).unwrap();
    let arguments = help
        .split("Arguments:")
        .nth(1)
        .unwrap()
        .split("Options:")
        .next()
        .unwrap();
    assert_eq!(
        arguments.split_whitespace().collect::<Vec<_>>(),
        ["[MINT]"],
        "SAFETY: no token positional"
    );
    assert!(!help.contains("--token"), "SAFETY: no token flag");
    let m = MintFixture::start(0).await;
    let (a, _ja) = funded(&m, 64).await;
    let out = a.path().join("t");
    let o = out.to_str().unwrap();
    let ok = cli(
        a.path(),
        &["send", &m.url, "--amount", "24", "--out", o],
        None,
    )
    .await;
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let token = read(&out);
    assert_secret_free(&ok, &token);
    assert_eq!(mode(&out), 0o600, "SAFETY: 0600");
    let s: serde_json::Value = serde_json::from_slice(&ok.stdout).unwrap();
    assert_eq!(
        (s["state"].as_str(), s["amount"].as_u64()),
        (Some("sent"), Some(24))
    );
    // Same command again: idempotent, still secret-free, file unchanged.
    let again = cli(
        a.path(),
        &["send", &m.url, "--amount", "24", "--out", o],
        None,
    )
    .await;
    assert!(again.status.success());
    assert_secret_free(&again, &token);
    assert_eq!(read(&out), token);
    // Existing other file: exit 1, nothing printed.
    let other = a.path().join("other");
    std::fs::write(&other, "x").unwrap();
    let refused = cli(
        a.path(),
        &[
            "send",
            &m.url,
            "--amount",
            "8",
            "--out",
            other.to_str().unwrap(),
        ],
        None,
    )
    .await;
    assert_eq!(refused.status.code(), Some(1));
    // Status: offline summary; --check-sends asks NUT-07.
    let st = cli(a.path(), &["status", "--check-sends"], None).await;
    assert!(st.status.success());
    assert_secret_free(&st, &token);
    assert!(String::from_utf8_lossy(&st.stdout).contains("\"unredeemed\""));
    let (b, jb) = home().await;
    receive::receive(b.path(), &jb, &m.url, &token)
        .await
        .unwrap();
    let st = cli(a.path(), &["status", "--check-sends"], None).await;
    assert!(String::from_utf8_lossy(&st.stdout).contains("\"redeemed\""));
    let id = s["send"].as_str().unwrap();
    let late = cli(a.path(), &["send", "--reclaim", id], None).await;
    assert_eq!(late.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&late.stderr).contains("redeemed by recipient"));
    assert_secret_free(&late, &token);
    // Lost reply is exit 3 (unresolved), not 1.
    m.faults.lose_reply.store(true, SeqCst);
    let out3 = a.path().join("t3");
    let p = cli(
        a.path(),
        &[
            "send",
            &m.url,
            "--amount",
            "8",
            "--out",
            out3.to_str().unwrap(),
        ],
        None,
    )
    .await;
    assert_eq!(p.status.code(), Some(3), "SAFETY: ambiguous is exit 3");
    m.faults.lose_reply.store(false, SeqCst);
    let rec = cli(a.path(), &["recover"], None).await;
    assert_eq!(
        rec.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&rec.stderr)
    );
    assert_secret_free(&rec, &read(&out3));
}

#[cfg(feature = "lab")]
async fn crash_then_recover(var: &'static str, code: i32, swaps_at_crash: u64) {
    let m = MintFixture::start(100).await;
    let (a, ja) = funded(&m, 64).await;
    let out = a.path().join("t");
    let o = out.to_str().unwrap();
    let args = [
        "send",
        &m.url,
        "--amount",
        "20",
        "--out",
        o,
        "--max-fees",
        "2",
    ];
    let crashed = cli(a.path(), &args, Some(var)).await;
    assert_eq!(crashed.status.code(), Some(code), "{var}");
    assert_eq!(m.faults.swaps.load(SeqCst), swaps_at_crash, "{var}");
    assert!(!out.exists(), "SAFETY: {var}: no token yet");
    let r = &attempts(&ja).await[0];
    let before = journaled_token(&ja, &r.id).await;
    let rec = cli(a.path(), &["recover"], None).await;
    assert_eq!(
        rec.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&rec.stderr)
    );
    assert_eq!(m.faults.swaps.load(SeqCst), 1, "SAFETY: {var}: one swap");
    assert_eq!(
        attempts(&ja).await.len(),
        1,
        "SAFETY: {var}: no second send"
    );
    let token = read(&out);
    if let Some(t) = before {
        assert_eq!(t, token, "SAFETY: {var}: same token rewritten");
    }
    assert_eq!(Some(token.clone()), journaled_token(&ja, &r.id).await);
    assert_secret_free(&rec, &token);
    assert_eq!(mode(&out), 0o600);
    let fee = attempts(&ja).await[0].fee;
    assert_eq!(balance(a.path(), &m.url).await, 64 - 20 - fee);
    // Rerunning the original command reports the finished send; no new one.
    let again = cli(a.path(), &args, None).await;
    assert!(again.status.success());
    assert_eq!(attempts(&ja).await.len(), 1);
    let (b, jb) = home().await;
    let got = receive::receive(b.path(), &jb, &m.url, &token)
        .await
        .unwrap();
    assert_eq!(got.amount, 20, "SAFETY: recovered token is valid");
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn recovery_never_overwrites_a_path_taken_after_the_crash() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 32).await;
    let out = a.path().join("t");
    let o = out.to_str().unwrap();
    let args = ["send", &m.url, "--amount", "8", "--out", o];
    let crashed = cli(a.path(), &args, Some("TRADE_CRASH_BEFORE_SEND_FILE")).await;
    assert_eq!(crashed.status.code(), Some(85));
    std::fs::write(&out, "precious").unwrap();
    let rec = cli(a.path(), &["recover"], None).await;
    assert_eq!(
        rec.status.code(),
        Some(3),
        "SAFETY: unresolved, not overwritten"
    );
    assert_eq!(read(&out), "precious", "SAFETY: never overwritten");
    std::fs::remove_file(&out).unwrap();
    let rec = cli(a.path(), &["recover"], None).await;
    assert_eq!(rec.status.code(), Some(0));
    let id = attempts(&ja).await[0].id.clone();
    assert_eq!(Some(read(&out)), journaled_token(&ja, &id).await);
    assert_eq!(m.faults.swaps.load(SeqCst), 1);
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn crash_after_journal_before_post_then_recover_completes() {
    crash_then_recover("TRADE_CRASH_BEFORE_SEND_SWAP", 87, 0).await;
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn crash_right_after_journal_before_reservation_then_recover_completes() {
    // SAFETY: the intent is journaled before ANY wallet effect (reservation or POST).
    crash_then_recover("TRADE_CRASH_AFTER_SEND_JOURNAL", 84, 0).await;
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn crash_after_post_before_wallet_commit_then_recover_completes() {
    crash_then_recover("TRADE_CRASH_AFTER_SEND_SWAP", 86, 1).await;
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn crash_after_swap_before_file_write_then_recover_rewrites_same_token() {
    crash_then_recover("TRADE_CRASH_BEFORE_SEND_FILE", 85, 1).await;
}

#[tokio::test]
async fn non_nut_400_stays_submitted_and_replays_the_same_swap() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 32).await;
    let out = a.path().join("t");
    // 1: non-JSON 400 body. 2: JSON 400 without a numeric NUT `code` (e.g. a
    // transport `expired`). Neither proves the mint refused this request.
    for (bad, swaps) in [(1, 1), (2, 2)] {
        m.faults.swap_bad_400.store(bad, SeqCst);
        let r = send::send(a.path(), &ja, &m.url, 8, &out, None)
            .await
            .unwrap();
        assert_eq!(
            r.state,
            SendState::Submitted,
            "SAFETY: a non-NUT 400 ({bad}) is not a definitive refusal"
        );
        assert!(!r.terminal());
        assert_eq!(attempts(&ja).await[0].state, SendState::Submitted);
        assert_eq!(m.faults.swaps.load(SeqCst), swaps);
        assert!(!out.exists());
    }
    let o = out.to_str().unwrap();
    let cli_out = cli(
        a.path(),
        &["send", &m.url, "--amount", "8", "--out", o],
        None,
    )
    .await;
    assert_eq!(
        cli_out.status.code(),
        Some(3),
        "SAFETY: ambiguous is exit 3"
    );
    m.faults.swap_bad_400.store(0, SeqCst);
    assert!(!money::recover(a.path(), &ja).await.unwrap());
    let all = attempts(&ja).await;
    assert_eq!(all.len(), 1, "SAFETY: no second attempt");
    assert_eq!(
        all[0].state,
        SendState::Sent,
        "SAFETY: same outputs replayed"
    );
    assert_eq!(balance(a.path(), &m.url).await, 24);
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn journal_write_failure_does_not_report_a_false_terminal_state() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 32).await;
    let out = a.path().join("t");
    let o = out.to_str().unwrap();
    m.faults.swap_nut_error.store(true, SeqCst);
    let failed = cli(
        a.path(),
        &["send", &m.url, "--amount", "8", "--out", o],
        Some("TRADE_FAIL_SEND_TERMINAL_WRITE"),
    )
    .await;
    assert_eq!(
        failed.status.code(),
        Some(3),
        "SAFETY: an unpersisted refusal is not reported as refused: {}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let printed: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(
        printed["state"], "submitted",
        "SAFETY: printed state = journal"
    );
    assert_eq!(attempts(&ja).await[0].state, SendState::Submitted);
    assert!(
        balance(a.path(), &m.url).await < 32,
        "SAFETY: inputs stay held while the refusal is unjournaled"
    );
    // Without the fault, recovery journals the refusal and only then releases.
    assert!(!money::recover(a.path(), &ja).await.unwrap());
    assert_eq!(attempts(&ja).await[0].state, SendState::Refused);
    assert_eq!(balance(a.path(), &m.url).await, 32);
}

#[cfg(feature = "lab")]
#[tokio::test]
async fn reclaim_journal_write_failure_is_not_reported_reclaimed() {
    let m = MintFixture::start(0).await;
    let (a, ja) = funded(&m, 64).await;
    let out = a.path().join("t");
    let r = send::send(a.path(), &ja, &m.url, 32, &out, None)
        .await
        .unwrap();
    let failed = cli(
        a.path(),
        &["send", "--reclaim", &r.id],
        Some("TRADE_FAIL_SEND_TERMINAL_WRITE"),
    )
    .await;
    assert_eq!(
        failed.status.code(),
        Some(3),
        "SAFETY: an unpersisted reclaim is not reported done: {}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let printed: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(
        printed["state"], "reclaiming",
        "SAFETY: printed state = journal"
    );
    assert_eq!(printed["reclaimed"], 0);
    assert_eq!(attempts(&ja).await[0].state, SendState::Reclaiming);
    // Recovery completes it once, without double credit.
    assert!(!money::recover(a.path(), &ja).await.unwrap());
    let done = attempts(&ja).await;
    assert_eq!(done[0].state, SendState::Reclaimed);
    assert_eq!(balance(a.path(), &m.url).await, 64, "SAFETY: credited once");
}
