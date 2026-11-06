//! Shared maxplayer home: trade and the buyer (maxplayer-core) on ONE wallet database.
//! In-process fake mints only. SAFETY assertions first.
mod support;
use cashu::State;
use cdk::{Amount, cdk_database::WalletDatabase, wallet::SendOptions};
use maxplayer_core::shared_wallet;
use maxplayer_trade::{journal::Journal, money, receive, send, wallet};
use std::{path::Path, process::Command, time::Duration};
use support::*;

const BIN: &str = env!("CARGO_BIN_EXE_maxplayer-trade");

fn trade(home: Option<&Path>, args: &[&str]) -> std::process::Output {
    let mut c = Command::new(BIN);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("MAXPLAYER_") {
            c.env_remove(k);
        }
    }
    if let Some(home) = home {
        c.arg("--home").arg(home);
    }
    c.args(["--relay", "ws://127.0.0.1:9"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}
fn err(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
async fn rows(home: &Path, mint: &str, states: Vec<State>) -> Vec<cdk::wallet::types::ProofInfo> {
    wallet::database(home, mint)
        .await
        .unwrap()
        .get_proofs(None, None, Some(states), None)
        .await
        .unwrap()
}
async fn core_wallet(home: &Path, mint: &str) -> cdk::wallet::Wallet {
    let mh = wallet::maxplayer_home(home).unwrap();
    shared_wallet::open_shared_wallet(&mh, mint, None)
        .await
        .unwrap()
}
const IN_FLIGHT: [State; 3] = [State::Reserved, State::Pending, State::PendingSpent];

/// Both sides select proofs from the same wallet at once: buyer-side core `prepare_send` (the
/// primitive every core send/payment uses, under the selection lock) and trade `send` (plan,
/// verify + journal + reserve under the same lock). No proof is reserved twice, none is lost:
/// every sat is either still spendable or in exactly one confirmed send, and nothing is left
/// reserved once both sides finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_core_and_trade_selection_never_double_reserves_or_loses_proofs() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 128).await;
    let j = Journal::open(home).await.unwrap();
    let w = core_wallet(home, &m.url).await;
    let wallet_dir = home.join("wallet");
    let (mut core_sent, mut trade_sent, mut refusals) = (0u64, 0u64, 0u32);
    for round in 0..8 {
        let out = h.path().join(format!("token-{round}"));
        let core = async {
            match shared_wallet::prepare_send_locked(
                &w,
                Some(&wallet_dir),
                Amount::from(3),
                SendOptions::default(),
            )
            .await
            {
                Ok(prepared) => {
                    prepared.confirm(None).await.unwrap();
                    Ok(3u64)
                }
                Err(e) => Err(e.to_string()),
            }
        };
        let trade = send::send(home, &j, &m.url, 5, &out, None);
        let (c, t) = tokio::join!(core, trade);
        match c {
            Ok(n) => core_sent += n,
            Err(e) => {
                assert!(e.contains("busy"), "core refusal must be retryable: {e}");
                refusals += 1;
            }
        }
        match t {
            Ok(r) => {
                assert_eq!(r.state, send::SendState::Sent, "{}", r.summary());
                trade_sent += r.amount;
            }
            Err(e) => {
                let e = format!("{e:#}");
                assert!(
                    e.contains("busy") || e.contains("changed while this operation chose"),
                    "trade refusal must be retryable and pre-journal: {e}"
                );
                refusals += 1;
            }
        }
    }
    assert!(core_sent > 0 && trade_sent > 0, "both sides spent: {refusals} refusals");
    assert!(
        rows(home, &m.url, vec![State::Reserved, State::Pending]).await.is_empty(),
        "SAFETY: nothing left reserved after both sides finished"
    );
    // cdk keeps a confirmed core send's token proofs PendingSpent until the payee claims them.
    let pending: u64 = rows(home, &m.url, vec![State::PendingSpent])
        .await
        .iter()
        .map(|p| u64::from(p.proof.amount))
        .sum();
    assert_eq!(pending, core_sent, "exactly the core tokens are awaiting claim");
    assert_eq!(
        balance(home, &m.url).await + core_sent + trade_sent,
        128,
        "SAFETY: no proof lost or spent twice (fee-free mint)"
    );
    // Every journaled trade send is terminal Sent: none was journaled and then lost its inputs.
    for r in j.all::<send::SendAttempt>("send").await.unwrap() {
        assert_eq!(r.state, send::SendState::Sent, "{}", r.summary());
    }
}

/// The selection lock is load-bearing on BOTH sides: while another holder has it, a core send and
/// a trade send each refuse after a bounded wait, retryably, with nothing selected, journaled or
/// reserved. A holder killed with SIGKILL (no cleanup at all) frees it; the trade send proceeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_select_lock_blocks_both_sides_and_sigkill_of_the_holder_frees_it() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 64).await;
    let j = Journal::open(home).await.unwrap();
    let lock = home.join("wallet").join(shared_wallet::SELECT_LOCK_FILE);
    // flock(1) -F: the process that holds the lock IS the pid we kill (no forked child keeps it).
    let mut holder = std::process::Command::new("flock")
        .args(["-x", "-F"])
        .arg(&lock)
        .args(["sleep", "600"])
        .spawn()
        .expect("flock(1)");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while shared_wallet::try_acquire(&home.join("wallet")).unwrap().is_some() {
        assert!(std::time::Instant::now() < deadline, "holder never took the lock");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let w = core_wallet(home, &m.url).await;
    let core = shared_wallet::prepare_send_locked(
        &w,
        Some(&home.join("wallet")),
        Amount::from(3),
        SendOptions::default(),
    )
    .await;
    assert!(
        matches!(core, Err(shared_wallet::LockedSelectError::Lock(_))),
        "SAFETY: core must not select while the lock is held"
    );
    let out = home.join("token");
    let blocked = send::send(home, &j, &m.url, 5, &out, None).await;
    let e = format!("{:#}", blocked.err().expect("trade must refuse"));
    assert!(e.contains("busy") && e.contains("retry"), "{e}");
    assert!(j.all::<send::SendAttempt>("send").await.unwrap().is_empty(), "nothing journaled");
    assert!(rows(home, &m.url, IN_FLIGHT.to_vec()).await.is_empty(), "nothing reserved");
    assert_eq!(balance(home, &m.url).await, 64);
    // Safety: plain kill(2) of our own child.
    assert_eq!(unsafe { libc_kill(holder.id() as i32, 9) }, 0);
    assert!(!holder.wait().unwrap().success());
    let r = send::send(home, &j, &m.url, 5, &out, None).await.unwrap();
    assert_eq!(r.state, send::SendState::Sent, "{}", r.summary());
    assert_eq!(balance(home, &m.url).await, 59);
}
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// cdk reserves row by row: a reservation that fails part-way (a proof another owner holds)
/// releases exactly the rows THIS call reserved, keeps what the operation held before, and never
/// touches the other owner's row.
#[tokio::test]
async fn partial_reserve_failure_releases_exactly_its_own_rows() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 7).await;
    let mut ys: Vec<_> = rows(home, &m.url, vec![State::Unspent])
        .await
        .into_iter()
        .map(|p| p.y)
        .collect();
    ys.sort();
    assert!(ys.len() >= 3, "{}", ys.len());
    let (y0, y1, y2) = (ys[0], ys[1], ys[2]);
    let op = uuid::Uuid::new_v4();
    let other = uuid::Uuid::new_v4();
    let sel = wallet::select(home, &m.url).await.unwrap();
    sel.reserve(&[y0], &op).await.unwrap();
    wallet::database(home, &m.url)
        .await
        .unwrap()
        .reserve_proofs(vec![y2], &other)
        .await
        .unwrap();
    let e = sel.reserve(&[y0, y1, y2], &op).await.unwrap_err();
    assert!(
        matches!(
            e.downcast_ref::<cdk_common::database::Error>(),
            Some(cdk_common::database::Error::ProofNotUnspent)
        ),
        "{e:#}"
    );
    let db = wallet::database(home, &m.url).await.unwrap();
    let state = |y| {
        let db = db.clone();
        async move {
            let r = db.get_proofs_by_ys(vec![y]).await.unwrap().remove(0);
            (r.state, r.used_by_operation)
        }
    };
    assert_eq!(state(y0).await, (State::Reserved, Some(op)), "held before: kept");
    assert_eq!(
        state(y1).await,
        (State::Unspent, None),
        "SAFETY: reserved by the failed call: released"
    );
    assert_eq!(state(y2).await, (State::Reserved, Some(other)), "other owner untouched");
    // A retry from the same state now succeeds once the other owner lets go.
    db.release_proofs(&other).await.unwrap();
    sel.reserve(&[y0, y1, y2], &op).await.unwrap();
    assert_eq!(db.get_reserved_proofs(&op).await.unwrap().len(), 3);
}

/// 4(a): the buyer's saga recovery never releases trade's reservations. cdk's own saga recovery
/// is owner-scoped (it releases only `get_reserved_proofs(saga.id)`), but a NUT-13
/// `Wallet::restore` (core runs it to complete a spent swap saga) re-derives every seed proof and
/// upserts it with NO owner. Shown here on the raw call, then proven fixed on core's wrapper.
#[tokio::test]
async fn buyer_restore_and_saga_retire_leave_trade_reservations_alone() {
    let m = MintFixture::start(0).await;
    // The hazard, on cdk itself: a plain restore releases a trade-reserved seed proof.
    let raw = maxplayer_home();
    fund(raw.path(), &m.url, 7).await;
    let op = uuid::Uuid::new_v4();
    let ys: Vec<_> = rows(raw.path(), &m.url, vec![State::Unspent])
        .await
        .into_iter()
        .map(|p| p.y)
        .collect();
    wallet::reserve(raw.path(), &m.url, &ys, &op).await.unwrap();
    core_wallet(raw.path(), &m.url).await.restore().await.unwrap();
    assert!(
        rows(raw.path(), &m.url, vec![State::Reserved]).await.is_empty(),
        "cdk 0.17.2 Wallet::restore clears foreign reservations (why core wraps it)"
    );

    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 7).await;
    let ys: Vec<_> = rows(home, &m.url, vec![State::Unspent])
        .await
        .into_iter()
        .map(|p| p.y)
        .collect();
    let op = uuid::Uuid::new_v4();
    wallet::reserve(home, &m.url, &ys, &op).await.unwrap();
    let w = core_wallet(home, &m.url).await;
    shared_wallet::restore_keeping_other_reservations(&w, Some(&home.join("wallet")))
        .await
        .unwrap();
    let report = maxplayer_core::payment_wallet::retire_eligible_incomplete_sagas_in(
        &w,
        Some(&home.join("wallet")),
    )
    .await
    .unwrap();
    assert!(report.unresolved.is_empty(), "{:?}", report.unresolved);
    let held = wallet::database(home, &m.url)
        .await
        .unwrap()
        .get_reserved_proofs(&op)
        .await
        .unwrap();
    assert_eq!(
        held.len(),
        ys.len(),
        "SAFETY: trade's reservation survives buyer recovery"
    );
    assert!(held.iter().all(|p| p.state == State::Reserved));
    assert_eq!(balance(home, &m.url).await, 0, "reserved is never spendable");
}

/// The reverse: trade's recovery passes never release a buyer saga's reservation.
#[tokio::test]
async fn trade_recovery_leaves_a_buyer_saga_reservation_alone() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 7).await;
    let j = Journal::open(home).await.unwrap();
    let w = core_wallet(home, &m.url).await;
    let prepared = shared_wallet::prepare_send_locked(
        &w,
        Some(&home.join("wallet")),
        Amount::from(2),
        SendOptions::default(),
    )
    .await
    .unwrap();
    let op = prepared.operation_id();
    let db = wallet::database(home, &m.url).await.unwrap();
    let before = db.get_reserved_proofs(&op).await.unwrap();
    assert!(!before.is_empty());
    money::recover(home, &j).await.unwrap();
    send::recover(home, &j).await.unwrap();
    receive::recover(home, &j).await.unwrap();
    let after = db.get_reserved_proofs(&op).await.unwrap();
    assert_eq!(
        after.iter().map(|p| p.y).collect::<Vec<_>>(),
        before.iter().map(|p| p.y).collect::<Vec<_>>(),
        "SAFETY: trade recovery left the buyer saga's proofs reserved"
    );
    prepared.cancel().await.unwrap();
    assert_eq!(balance(home, &m.url).await, 7);
}

/// 4(c): `maxplayer wallet balance` (core `balances_async`) shows trade-reserved sats as reserved,
/// never as spendable.
#[tokio::test]
async fn buyer_balance_reports_trade_reserved_sats_as_reserved_not_spendable() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 7).await;
    let all = rows(home, &m.url, vec![State::Unspent]).await;
    let held = &all[..all.len() / 2];
    let held_sats: u64 = held.iter().map(|p| u64::from(p.proof.amount)).sum();
    assert!(held_sats > 0 && held_sats < 7);
    let ys: Vec<_> = held.iter().map(|p| p.y).collect();
    wallet::reserve(home, &m.url, &ys, &uuid::Uuid::new_v4())
        .await
        .unwrap();
    let mh = wallet::maxplayer_home(home).unwrap();
    let rows = maxplayer_core::wallet_ops::balances_async(&mh).await.unwrap();
    let row = rows
        .iter()
        .find(|r| r.mint_url == m.url.trim_end_matches('/'))
        .unwrap();
    assert_eq!(
        (row.balance_sats, row.reserved_sats),
        (7 - held_sats, held_sats),
        "SAFETY: reserved sats are shown as reserved, never as spendable"
    );
    assert!(row.configured);
    assert_eq!(wallet::read_balance(home, &m.url).unwrap(), 7 - held_sats, "trade agrees");
}

/// Only mints in the home's configured list AND admitted by the real-mint fence are traded.
#[tokio::test]
async fn shared_home_trades_only_configured_fenced_mints() {
    let m = MintFixture::start(0).await;
    let h = tempfile::tempdir().unwrap();
    let home = h.path();
    // Outside the fixture registry: only the default mint is configured.
    std::fs::write(home.join("key"), nostr_sdk::Keys::generate().secret_key().to_secret_hex())
        .unwrap();
    std::fs::write(home.join("config.toml"), "").unwrap();
    let e = wallet::check_mint(home, &m.url).unwrap_err().to_string();
    assert!(e.contains("not one of this home's configured mints"), "{e}");
    assert!(e.contains("maxplayer wallet mints add"), "{e}");
    let out = trade(Some(home), &["send", &m.url, "--amount", "5", "--out", "/nonexistent/t"]);
    assert_eq!(out.status.code(), Some(1), "{}", err(&out));
    assert!(err(&out).contains("not one of this home's configured mints"), "{}", err(&out));
    assert!(!home.join("trade").exists(), "SAFETY: refused before any journal");
    let out = trade(Some(home), &["fund", &m.url, "--amount", "5"]);
    assert!(err(&out).contains("not one of this home's configured mints"), "{}", err(&out));
    // Configured, but the fence (allow_real_mints = false) refuses a real https mint.
    std::fs::write(
        home.join("config.toml"),
        "allow_real_mints = false\nextra_mints = [\"https://mint.example\"]\n",
    )
    .unwrap();
    let e = wallet::check_mint(home, "https://mint.example").unwrap_err().to_string();
    assert!(e.contains("real-mint fence"), "{e}");
    // Configured loopback fixture mint: admitted.
    std::fs::write(home.join("config.toml"), format!("extra_mints = [{:?}]\n", m.url)).unwrap();
    wallet::check_mint(home, &m.url).unwrap();
}

/// A maxplayer home is the default home (`$MAXPLAYER_HOME`), signs with its `key`, and keeps
/// trade's own files in `<home>/trade` (0700). No `trade.key` or `wallet.seed` is ever created.
#[tokio::test]
async fn shared_home_layout_and_default_home() {
    use std::os::unix::fs::PermissionsExt;
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 9).await;
    let key = std::fs::read_to_string(home.join("key")).unwrap();
    assert_eq!(
        wallet::identity(home).unwrap().secret_key().to_secret_hex(),
        key.trim(),
        "trading signs with the home key"
    );
    let mut c = Command::new(BIN);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("MAXPLAYER_") {
            c.env_remove(k);
        }
    }
    let out = c
        .env("MAXPLAYER_HOME", home)
        .args(["balance", &m.url])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", err(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["balance"], 9, "no --home: the maxplayer home");
    let out = trade(Some(home), &["recover"]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(!err(&out).contains("legacy"), "{}", err(&out));
    let state = home.join("trade");
    assert_eq!(
        std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(state.join("trade.sqlite").exists() && state.join("owner.lock").exists());
    for f in ["trade.key", "wallet.seed", "trade.sqlite", "owner.lock"] {
        assert!(!home.join(f).exists(), "SAFETY: {f} created at the home root");
    }
    assert!(home.join("wallet/cdk-wallet.sqlite").exists(), "the buyer's wallet DB");
}

/// Legacy standalone homes: every command warns; NEW list/take/fund/receive are refused with the
/// drain instructions and change nothing; everything needed to drain or finish still runs.
#[tokio::test]
async fn legacy_home_allow_refuse_matrix() {
    let h = tempfile::tempdir().unwrap();
    let home = h.path();
    std::fs::write(home.join("wallet.seed"), [7u8; 64]).unwrap();
    let mint = "https://testnut.cashu.space";
    let token = home.join("token-in");
    std::fs::write(&token, "cashuB").unwrap();
    let token = token.to_str().unwrap();
    let refused: Vec<Vec<&str>> = vec![
        vec!["list", "--give-mint", mint, "--give", "5", "--want-mint", "https://b.example", "--want", "5"],
        vec!["take", "abc", "--max-give", "5", "--min-receive", "1"],
        vec!["fund", mint, "--amount", "5"],
        vec!["receive", mint, "--token-file", token],
    ];
    for args in refused {
        let before: Vec<_> = std::fs::read_dir(home).unwrap().map(|e| e.unwrap().file_name()).collect();
        let out = trade(Some(home), &args);
        let e = err(&out);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {e}");
        assert!(e.contains("legacy standalone trading home") && e.contains("deprecated"), "{e}");
        assert!(e.contains("refused:") && e.contains("maxplayer wallet receive"), "{e}");
        let after: Vec<_> = std::fs::read_dir(home).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(before, after, "SAFETY: {args:?} changed the legacy home");
    }
    let allowed: Vec<Vec<&str>> = vec![
        vec!["balance", mint],
        vec!["status"],
        vec!["status", "--json"],
        vec!["recover"],
    ];
    for args in allowed {
        let out = trade(Some(home), &args);
        let e = err(&out);
        assert!(out.status.success(), "{args:?}: {e}");
        assert!(e.contains("deprecated"), "{args:?} must warn: {e}");
    }
    // Drain paths run (and fail only on money/mint grounds, never the legacy refusal).
    let out_file = home.join("drain-token");
    for args in [
        vec!["send", "http://127.0.0.1:9", "--amount", "5", "--out", out_file.to_str().unwrap()],
        vec!["cancel", "abc"],
    ] {
        let out = trade(Some(home), &args);
        let e = err(&out);
        assert!(e.contains("deprecated"), "{args:?}: {e}");
        assert!(!e.contains("refused:"), "SAFETY: drain path refused: {args:?}: {e}");
    }
    // A directory that is neither home: read-only commands only, nothing created.
    let empty = tempfile::tempdir().unwrap();
    let out = trade(Some(empty.path()), &["send", mint, "--amount", "5", "--out", "/nonexistent/t"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(err(&out).contains("no maxplayer home at"), "{}", err(&out));
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0, "nothing created");
    assert!(trade(Some(empty.path()), &["status"]).status.success());
}
