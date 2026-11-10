//! Shared maxplayer home: trade and the buyer (maxplayer-core) on ONE wallet database.
//! In-process fake mints only. SAFETY assertions first.
mod support;
use cashu::State;
use cdk::{
    Amount,
    cdk_database::WalletDatabase,
    nuts::{MeltQuoteState, PaymentMethod},
    wallet::SendOptions,
};
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
            // Like every production core send: the operation lock, held until confirm returns.
            let _op = shared_wallet::operation_guard(Some(&wallet_dir))
                .await
                .unwrap();
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
    assert!(
        core_sent > 0 && trade_sent > 0,
        "both sides spent: {refusals} refusals"
    );
    assert!(
        rows(home, &m.url, vec![State::Reserved, State::Pending])
            .await
            .is_empty(),
        "SAFETY: nothing left reserved after both sides finished"
    );
    // cdk keeps a confirmed core send's token proofs PendingSpent until the payee claims them.
    let pending: u64 = rows(home, &m.url, vec![State::PendingSpent])
        .await
        .iter()
        .map(|p| u64::from(p.proof.amount))
        .sum();
    assert_eq!(
        pending, core_sent,
        "exactly the core tokens are awaiting claim"
    );
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
    while shared_wallet::try_acquire(&home.join("wallet"))
        .unwrap()
        .is_some()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "holder never took the lock"
        );
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
    assert!(
        j.all::<send::SendAttempt>("send").await.unwrap().is_empty(),
        "nothing journaled"
    );
    assert!(
        rows(home, &m.url, IN_FLIGHT.to_vec()).await.is_empty(),
        "nothing reserved"
    );
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
    assert_eq!(
        state(y0).await,
        (State::Reserved, Some(op)),
        "held before: kept"
    );
    assert_eq!(
        state(y1).await,
        (State::Unspent, None),
        "SAFETY: reserved by the failed call: released"
    );
    assert_eq!(
        state(y2).await,
        (State::Reserved, Some(other)),
        "other owner untouched"
    );
    // A retry from the same state now succeeds once the other owner lets go.
    db.release_proofs(&other).await.unwrap();
    sel.reserve(&[y0, y1, y2], &op).await.unwrap();
    assert_eq!(db.get_reserved_proofs(&op).await.unwrap().len(), 3);
}

/// 4(a): the buyer's saga recovery never releases trade's reservations. cdk's own saga recovery
/// is owner-scoped (it releases only `get_reserved_proofs(saga.id)`), but a NUT-13
/// `Wallet::restore` re-derives every seed proof and upserts it with NO owner. Shown here on the
/// raw call (why core never calls it), then on core's retire pass.
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
    core_wallet(raw.path(), &m.url)
        .await
        .restore()
        .await
        .unwrap();
    assert!(
        rows(raw.path(), &m.url, vec![State::Reserved])
            .await
            .is_empty(),
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
    assert_eq!(
        balance(home, &m.url).await,
        0,
        "reserved is never spendable"
    );
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
    let rows = maxplayer_core::wallet_ops::balances_async(&mh)
        .await
        .unwrap();
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
    assert_eq!(
        wallet::read_balance(home, &m.url).unwrap(),
        7 - held_sats,
        "trade agrees"
    );
}

/// Only mints in the home's configured list AND admitted by the real-mint fence are traded.
#[tokio::test]
async fn shared_home_trades_only_configured_fenced_mints() {
    let m = MintFixture::start(0).await;
    let h = tempfile::tempdir().unwrap();
    let home = h.path();
    // Outside the fixture registry: only the default mint is configured.
    std::fs::write(
        home.join("key"),
        nostr_sdk::Keys::generate().secret_key().to_secret_hex(),
    )
    .unwrap();
    std::fs::write(home.join("config.toml"), "").unwrap();
    let e = wallet::check_mint(home, &m.url).unwrap_err().to_string();
    assert!(e.contains("not one of this home's mints"), "{e}");
    assert!(e.contains("maxplayer wallet mints add"), "{e}");
    assert!(e.contains("accepted_mints"), "{e}");
    let out = trade(
        Some(home),
        &["send", &m.url, "--amount", "5", "--out", "/nonexistent/t"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", err(&out));
    assert!(
        err(&out).contains("not one of this home's mints"),
        "{}",
        err(&out)
    );
    assert!(
        !home.join("trade").exists(),
        "SAFETY: refused before any journal"
    );
    let out = trade(Some(home), &["fund", &m.url, "--amount", "5"]);
    assert!(
        err(&out).contains("not one of this home's mints"),
        "{}",
        err(&out)
    );
    // Configured, but the fence (allow_real_mints = false) refuses a real https mint.
    std::fs::write(
        home.join("config.toml"),
        "allow_real_mints = false\nextra_mints = [\"https://mint.example\"]\n",
    )
    .unwrap();
    let e = wallet::check_mint(home, "https://mint.example")
        .unwrap_err()
        .to_string();
    assert!(e.contains("real-mint fence"), "{e}");
    // Configured loopback fixture mint: admitted.
    std::fs::write(
        home.join("config.toml"),
        format!("extra_mints = [{:?}]\n", m.url),
    )
    .unwrap();
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
        assert!(
            !home.join(f).exists(),
            "SAFETY: {f} created at the home root"
        );
    }
    assert!(
        home.join("wallet/cdk-wallet.sqlite").exists(),
        "the buyer's wallet DB"
    );
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
        vec![
            "list",
            "--give-mint",
            mint,
            "--give",
            "5",
            "--want-mint",
            "https://b.example",
            "--want",
            "5",
        ],
        vec!["take", "abc", "--max-give", "5", "--min-receive", "1"],
        vec!["fund", mint, "--amount", "5"],
        vec!["receive", mint, "--token-file", token],
    ];
    for args in refused {
        let before: Vec<_> = std::fs::read_dir(home)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let out = trade(Some(home), &args);
        let e = err(&out);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {e}");
        assert!(
            e.contains("legacy standalone trading home") && e.contains("deprecated"),
            "{e}"
        );
        assert!(
            e.contains("refused:") && e.contains("maxplayer wallet receive"),
            "{e}"
        );
        let after: Vec<_> = std::fs::read_dir(home)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
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
        vec![
            "send",
            "http://127.0.0.1:9",
            "--amount",
            "5",
            "--out",
            out_file.to_str().unwrap(),
        ],
        vec!["cancel", "abc"],
    ] {
        let out = trade(Some(home), &args);
        let e = err(&out);
        assert!(e.contains("deprecated"), "{args:?}: {e}");
        assert!(
            !e.contains("refused:"),
            "SAFETY: drain path refused: {args:?}: {e}"
        );
    }
    // A directory that is neither home: read-only commands only, nothing created.
    let empty = tempfile::tempdir().unwrap();
    let out = trade(
        Some(empty.path()),
        &["send", mint, "--amount", "5", "--out", "/nonexistent/t"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(err(&out).contains("no maxplayer home at"), "{}", err(&out));
    assert_eq!(
        std::fs::read_dir(empty.path()).unwrap().count(),
        0,
        "nothing created"
    );
    assert!(trade(Some(empty.path()), &["status"]).status.success());
}

/// 4(b): two PROCESSES write the one wallet database at once (cdk-sqlite 0.17.2 opens it with WAL,
/// `busy_timeout = 10000` and `BEGIN IMMEDIATE`). `maxplayer-trade send` subprocesses run while this
/// process selects and confirms core sends in a loop. Every command either completes or refuses
/// retryably (lock busy / inputs changed) — never `database is locked` — and the sats add up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_processes_write_the_shared_wallet_database_concurrently() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path().to_path_buf();
    fund(&home, &m.url, 256).await;
    let w = core_wallet(&home, &m.url).await;
    let wallet_dir = home.join("wallet");
    let mut children = vec![];
    for i in 0..4 {
        let out = h.path().join(format!("cli-token-{i}"));
        let mut c = tokio::process::Command::new(BIN);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("MAXPLAYER_") {
                c.env_remove(k);
            }
        }
        c.arg("--home")
            .arg(&home)
            .args(["send", &m.url, "--amount", "7", "--out"])
            .arg(&out)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        children.push(c.output());
    }
    let cli = futures_util::future::join_all(children);
    let core = async {
        let mut sent = 0u64;
        for _ in 0..12 {
            // Like every production core send: the operation lock, held until confirm returns.
            let _op = shared_wallet::operation_guard(Some(&wallet_dir))
                .await
                .unwrap();
            match shared_wallet::prepare_send_locked(
                &w,
                Some(&wallet_dir),
                Amount::from(3),
                SendOptions::default(),
            )
            .await
            {
                Ok(p) => {
                    p.confirm(None).await.unwrap();
                    sent += 3;
                }
                Err(e) => assert!(e.to_string().contains("busy"), "{e}"),
            }
        }
        sent
    };
    let (outs, core_sent) = tokio::join!(cli, core);
    let mut cli_sent = 0u64;
    for o in outs {
        let o = o.unwrap();
        let e = err(&o);
        assert!(
            !e.contains("database is locked"),
            "SAFETY: SQLITE_BUSY leaked: {e}"
        );
        if o.status.success() {
            cli_sent += 7;
        } else {
            assert!(
                e.contains("busy") || e.contains("changed while this operation chose"),
                "only a retryable refusal: {e}"
            );
        }
    }
    assert!(cli_sent > 0 && core_sent > 0, "both processes spent");
    assert!(
        rows(&home, &m.url, vec![State::Reserved, State::Pending])
            .await
            .is_empty()
    );
    let pending: u64 = rows(&home, &m.url, vec![State::PendingSpent])
        .await
        .iter()
        .map(|p| u64::from(p.proof.amount))
        .sum();
    assert_eq!(pending, core_sent);
    assert_eq!(
        balance(&home, &m.url).await + core_sent + cli_sent,
        256,
        "SAFETY: no proof lost or double-spent across processes"
    );
}

fn invoice(sats: u64) -> String {
    cdk_fake_wallet::create_fake_invoice(sats * 1000, String::new()).to_string()
}
/// A seller's maxplayer home outside the fixture registry: `accepted_mints` exactly as given, no
/// `extra_mints`, a `[seller]` table; `top` is prepended (top-level keys).
fn seller_home(home: &Path, accepted: &[&str], top: &str) {
    if !home.join("key").exists() {
        std::fs::write(
            home.join("key"),
            nostr_sdk::Keys::generate().secret_key().to_secret_hex(),
        )
        .unwrap();
    }
    let list: Vec<_> = accepted.iter().map(|m| format!("{m:?}")).collect();
    std::fs::write(
        home.join("config.toml"),
        format!(
            "{top}accepted_mints = [{}]\n\n[seller]\nagent_command = [\"true\"]\nrate_sats = 1\n\
             git_remote = \"https://relay.example/git/x/y.git\"\n",
            list.join(", ")
        ),
    )
    .unwrap();
}
/// Fund `home` at `mint` without registering the mint in its config.
async fn fund_unregistered(home: &Path, mint: &str, n: u64) {
    let w = wallet::wallet(home, mint).await.unwrap();
    let q = w
        .mint_quote(PaymentMethod::BOLT11, Some(n.into()), None, None)
        .await
        .unwrap();
    for _ in 0..100 {
        if w.check_mint_quote(&q.id).await.unwrap().state == cashu::nuts::nut23::QuoteState::Paid {
            w.mint(&q.id, cdk::amount::SplitTarget::default(), None)
                .await
                .unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("unpaid fake quote")
}

/// A seller is paid at every mint it accepts, into this same wallet, so it trades every one of
/// them: with `accepted_mints = [A, B, C]` and no `extra_mints`, B and C are admitted (B end to end
/// through the CLI) although the buyer's configured set is only A; a mint in neither list is
/// refused before anything is journaled, and the refusal names both lists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seller_home_trades_every_accepted_mint() {
    let m = MintFixture::start(0).await;
    let h = tempfile::tempdir().unwrap();
    let home = h.path();
    let b = m.url.trim_end_matches('/').to_owned();
    seller_home(home, &["https://a.example", &b, "https://c.example/"], "");
    let mh = wallet::maxplayer_home(home).unwrap();
    assert_eq!(
        maxplayer_core::wallet_ops::configured_mints(&mh).unwrap(),
        ["https://a.example"],
        "the buyer's configured set is unchanged: B and C come only from accepted_mints"
    );
    for mint in [
        "https://a.example",
        b.as_str(),
        "https://c.example",
        "https://c.example/",
    ] {
        wallet::check_mint(home, mint).unwrap_or_else(|e| panic!("{mint} must trade: {e:#}"));
    }
    let e = wallet::check_mint(home, "https://d.example")
        .unwrap_err()
        .to_string();
    assert!(e.contains("not one of this home's mints"), "{e}");
    assert!(e.contains("accepted_mints"), "{e}");
    assert!(
        e.contains("maxplayer wallet mints add https://d.example"),
        "{e}"
    );
    let out = trade(
        Some(home),
        &[
            "send",
            "https://d.example",
            "--amount",
            "1",
            "--out",
            "/nonexistent/t",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", err(&out));
    assert!(
        err(&out).contains("not one of this home's mints"),
        "{}",
        err(&out)
    );
    assert!(
        !home.join("trade").exists(),
        "SAFETY: refused before any journal"
    );
    // B trades for real: a CLI send from the seller's second accepted mint (off the runtime, which
    // serves the fixture mint).
    fund_unregistered(home, &b, 9).await;
    let token = home.join("token-b");
    let (cli_home, cli_b, cli_token) = (home.to_path_buf(), b.clone(), token.clone());
    let out = tokio::task::spawn_blocking(move || {
        trade(
            Some(&cli_home),
            &[
                "send",
                &cli_b,
                "--amount",
                "4",
                "--out",
                cli_token.to_str().unwrap(),
            ],
        )
    })
    .await
    .unwrap();
    assert!(out.status.success(), "{}", err(&out));
    assert!(token.exists());
    assert_eq!(balance(home, &b).await, 5);
}

/// Listing a mint in `accepted_mints` never gets it past trade's other fences: no plain http://
/// (outside a loopback test fixture), only https:// or a well-formed nostr:// under the real-mint
/// fence, and `allow_real_mints = false` still refuses a real accepted mint.
#[tokio::test]
async fn accepted_mints_still_pass_the_scheme_and_real_mint_fences() {
    let h = tempfile::tempdir().unwrap();
    let home = h.path();
    seller_home(
        home,
        &[
            "https://a.example",
            "http://mint.example",
            "nostr://not-an-npub",
            "ftp://mint.example",
        ],
        "",
    );
    let e = wallet::check_mint(home, "http://mint.example")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(maxplayer_trade::transport::HTTP_MINT_REFUSED),
        "{e}"
    );
    for mint in ["nostr://not-an-npub", "ftp://mint.example"] {
        let e = wallet::check_mint(home, mint).unwrap_err().to_string();
        assert!(e.contains("real-mint fence"), "{mint}: {e}");
    }
    let out = trade(
        Some(home),
        &[
            "send",
            "http://mint.example",
            "--amount",
            "1",
            "--out",
            "/nonexistent/t",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", err(&out));
    assert!(
        err(&out).contains(maxplayer_trade::transport::HTTP_MINT_REFUSED),
        "{}",
        err(&out)
    );
    seller_home(
        home,
        &["https://a.example", "https://b.example"],
        "allow_real_mints = false\n",
    );
    let e = wallet::check_mint(home, "https://b.example")
        .unwrap_err()
        .to_string();
    assert!(e.contains("real-mint fence"), "{e}");
    assert!(!home.join("trade").exists(), "nothing journaled");
}

/// 4(c) for the seller: the one seller path that selects proofs is the platform-fee remittance
/// melt (`fee_remit::LiveEffects::prepare_melt` -> `wallet_ops::prepare_melt_payment_blocking` ->
/// `shared_wallet::prepare_melt_locked`). Core's wrapper refuses the loopback http fixture mint
/// (real-mint fence), so this drives the primitive it calls (core's
/// `shared_wallet::tests::seller_paths_select_proofs_only_under_the_select_lock` pins that path). While the
/// lock is held it refuses, retryably, reserving nothing; run against trade sends on the same
/// wallet, no proof is reserved twice and none is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seller_fee_melt_and_trade_selection_never_double_reserve_or_lose_proofs() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 256).await;
    let j = Journal::open(home).await.unwrap();
    let w = core_wallet(home, &m.url).await;
    let wallet_dir = home.join("wallet");

    let q = w
        .melt_quote(PaymentMethod::BOLT11, invoice(2), None, None)
        .await
        .unwrap();
    {
        let _held = shared_wallet::acquire(&wallet_dir, shared_wallet::SELECT_LOCK_TIMEOUT)
            .await
            .unwrap();
        let e = match shared_wallet::prepare_melt_locked(&w, Some(&wallet_dir), &q.id).await {
            Ok(_) => {
                panic!("SAFETY: the seller melt must not select while another holder has the lock")
            }
            Err(e) => e.to_string(),
        };
        assert!(e.contains("busy"), "{e}");
        assert!(
            rows(home, &m.url, IN_FLIGHT.to_vec()).await.is_empty(),
            "SAFETY: a refused seller melt reserved nothing"
        );
    }

    let (mut melted, mut trade_sent, mut refusals) = (0u64, 0u64, 0u32);
    for round in 0..8u64 {
        let q = w
            .melt_quote(PaymentMethod::BOLT11, invoice(2 + round % 2), None, None)
            .await
            .unwrap();
        let out = home.join(format!("token-{round}"));
        let seller = async {
            // Like the production fee melt: the operation lock, held until confirm returns.
            let _op = shared_wallet::operation_guard(Some(&wallet_dir))
                .await
                .unwrap();
            match shared_wallet::prepare_melt_locked(&w, Some(&wallet_dir), &q.id).await {
                Ok(prepared) => {
                    let done = prepared.confirm().await.unwrap();
                    assert_eq!(done.state(), MeltQuoteState::Paid);
                    Ok(u64::from(done.amount()) + u64::from(done.fee_paid()))
                }
                Err(e) => Err(e.to_string()),
            }
        };
        let trade = send::send(home, &j, &m.url, 5, &out, None);
        let (s, t) = tokio::join!(seller, trade);
        match s {
            Ok(n) => melted += n,
            Err(e) => {
                assert!(e.contains("busy"), "seller refusal must be retryable: {e}");
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
    assert!(
        melted > 0 && trade_sent > 0,
        "both sides spent: {refusals} refusals"
    );
    assert!(
        rows(home, &m.url, IN_FLIGHT.to_vec()).await.is_empty(),
        "SAFETY: nothing left reserved or pending after both sides finished"
    );
    assert_eq!(
        balance(home, &m.url).await + melted + trade_sent,
        256,
        "SAFETY: no proof lost or spent twice (fee-free mint; melt debits invoice + fee paid)"
    );
    for r in j.all::<send::SendAttempt>("send").await.unwrap() {
        assert_eq!(r.state, send::SendState::Sent, "{}", r.summary());
    }
}

/// 4(d) for the seller: seller-side recovery never releases trade's reservations. A fee-remittance
/// melt is prepared and abandoned (the node died between prepare and confirm), trade reserves
/// every other proof, then the seller recovers: node start-up (`SellerNode::open`,
/// `reconcile_on_start`, a wallet read) and the remittance reconciliation's
/// `check_melt_quote_status` (what `wallet_ops::melt_status_for_quote_async` runs), which
/// compensates the abandoned `ProofsReserved` melt saga. Only the seller's own proofs come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seller_recovery_leaves_trade_reservations_alone() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 32).await;
    let w = core_wallet(home, &m.url).await;
    let wallet_dir = home.join("wallet");
    let db = wallet::database(home, &m.url).await.unwrap();

    let q = w
        .melt_quote(PaymentMethod::BOLT11, invoice(2), None, None)
        .await
        .unwrap();
    let prepared = match shared_wallet::prepare_melt_locked(&w, Some(&wallet_dir), &q.id).await {
        Ok(p) => p,
        Err(e) => panic!("prepare seller melt: {e}"),
    };
    let seller_op = prepared.operation_id();
    drop(prepared);
    let seller_sats: u64 = db
        .get_reserved_proofs(&seller_op)
        .await
        .unwrap()
        .iter()
        .map(|p| u64::from(p.proof.amount))
        .sum();
    assert!(seller_sats > 0);

    let trade_op = uuid::Uuid::new_v4();
    let ys: Vec<_> = rows(home, &m.url, vec![State::Unspent])
        .await
        .into_iter()
        .map(|p| p.y)
        .collect();
    assert!(!ys.is_empty());
    wallet::reserve(home, &m.url, &ys, &trade_op).await.unwrap();

    let node = maxplayer_core::seller_node::SellerNode::open(wallet::maxplayer_home(home).unwrap())
        .await
        .unwrap();
    node.reconcile_on_start(1).unwrap();
    node.status_snapshot().await.unwrap();
    drop(node);
    w.check_melt_quote_status(&q.id).await.unwrap();

    assert!(
        db.get_reserved_proofs(&seller_op).await.unwrap().is_empty(),
        "the seller's abandoned melt was compensated"
    );
    let held = db.get_reserved_proofs(&trade_op).await.unwrap();
    assert_eq!(
        held.len(),
        ys.len(),
        "SAFETY: trade's reservation survives seller recovery"
    );
    assert!(held.iter().all(|p| p.state == State::Reserved));
    assert_eq!(
        balance(home, &m.url).await,
        seller_sats,
        "only the seller's own proofs became spendable again"
    );
}

// ---- Review round 1: swap-saga recovery, lock scope, mixed homes, owner-checked credit ----------

/// A shared home with three owners in flight on one wallet: a buyer swap saga whose swap the mint
/// EXECUTED but whose reply was lost (inputs SPENT at the mint, saga `SwapRequested`), trade's
/// reservation, and a second in-flight cdk saga (a prepared, undecided melt).
struct SwapScene {
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    trade_op: uuid::Uuid,
    trade_ys: Vec<cashu::PublicKey>,
    melt_op: uuid::Uuid,
    melt_ys: Vec<cashu::PublicKey>,
    swap_saga: uuid::Uuid,
    swap_amount: u64,
    /// Spendable (unowned UNSPENT) sats besides the swap's outputs.
    spare: u64,
}
async fn spent_swap_scene(m: &MintFixture) -> SwapScene {
    use std::sync::atomic::Ordering::SeqCst;
    let dir = maxplayer_home();
    let home = dir.path().to_path_buf();
    fund(&home, &m.url, 127).await;
    let w = core_wallet(&home, &m.url).await;
    let wallet_dir = home.join("wallet");
    let db = wallet::database(&home, &m.url).await.unwrap();
    let q = w
        .melt_quote(PaymentMethod::BOLT11, invoice(2), None, None)
        .await
        .unwrap();
    let prepared = match shared_wallet::prepare_melt_locked(&w, Some(&wallet_dir), &q.id).await {
        Ok(p) => p,
        Err(e) => panic!("prepare melt: {e}"),
    };
    let melt_op = prepared.operation_id();
    drop(prepared); // undecided: stays reserved, like a melt whose process died
    let melt_ys: Vec<_> = db
        .get_reserved_proofs(&melt_op)
        .await
        .unwrap()
        .iter()
        .map(|p| p.y)
        .collect();
    assert!(!melt_ys.is_empty());
    let mut free: Vec<_> = rows(&home, &m.url, vec![State::Unspent])
        .await
        .into_iter()
        .filter(|p| p.used_by_operation.is_none())
        .collect();
    free.sort_by_key(|p| std::cmp::Reverse(p.proof.amount));
    assert!(free.len() >= 3, "{}", free.len());
    let swap_input = free[0].clone();
    let trade_ys = vec![free[1].y, free[2].y];
    let trade_op = uuid::Uuid::new_v4();
    wallet::reserve(&home, &m.url, &trade_ys, &trade_op)
        .await
        .unwrap();
    let swap_amount = u64::from(swap_input.proof.amount);
    let spare: u64 = free[3..].iter().map(|p| u64::from(p.proof.amount)).sum();
    m.faults.lose_reply.store(true, SeqCst);
    let lost = w
        .swap(
            None,
            cdk::amount::SplitTarget::default(),
            vec![swap_input.proof.clone()],
            None,
            false,
            false,
        )
        .await;
    m.faults.lose_reply.store(false, SeqCst);
    assert!(lost.is_err(), "the swap reply must be lost");
    let swap_saga = db
        .get_incomplete_sagas()
        .await
        .unwrap()
        .into_iter()
        .find(|s| matches!(s.state, cdk::wallet::types::WalletSagaState::Swap(_)))
        .expect("a stranded swap saga")
        .id;
    assert_eq!(
        db.get_reserved_proofs(&swap_saga).await.unwrap().len(),
        1,
        "the swap's input is reserved under its saga"
    );
    m.faults.restores_seen.store(0, SeqCst);
    m.faults.checkstates_after_restore.store(0, SeqCst);
    SwapScene {
        _dir: dir,
        home,
        trade_op,
        trade_ys,
        melt_op,
        melt_ys,
        swap_saga,
        swap_amount,
        spare,
    }
}
fn clear_recovery_faults(m: &MintFixture) {
    use std::sync::atomic::Ordering::SeqCst;
    m.faults.fail_restores_after.store(0, SeqCst);
    m.faults.fail_checkstate_after_restore.store(false, SeqCst);
    m.faults.hold_second_restore.store(false, SeqCst);
    m.faults.hold_checkstate_after_restore.store(false, SeqCst);
}
/// SAFETY: trade's and the melt saga's proofs are still RESERVED under their own owners, and
/// neither core's selection nor trade's plan can take them: exactly `spendable` sats are selectable.
async fn assert_owners_kept(m: &MintFixture, s: &SwapScene, recovered: bool) {
    let db = wallet::database(&s.home, &m.url).await.unwrap();
    let held = |op: uuid::Uuid| {
        let db = db.clone();
        async move {
            let mut ys: Vec<_> = db
                .get_reserved_proofs(&op)
                .await
                .unwrap()
                .into_iter()
                .inspect(|p| assert_eq!(p.state, State::Reserved, "SAFETY: {op} row not RESERVED"))
                .map(|p| p.y)
                .collect();
            ys.sort();
            ys
        }
    };
    let mut want = s.trade_ys.clone();
    want.sort();
    assert_eq!(
        held(s.trade_op).await,
        want,
        "SAFETY: trade's reservation kept its owner"
    );
    let mut want = s.melt_ys.clone();
    want.sort();
    assert_eq!(
        held(s.melt_op).await,
        want,
        "SAFETY: the melt saga's reservation kept its owner"
    );
    let saga_left = db.get_saga(&s.swap_saga).await.unwrap().is_some();
    assert_eq!(saga_left, !recovered, "swap saga row");
    let spendable = s.spare + if recovered { s.swap_amount } else { 0 };
    assert_eq!(balance(&s.home, &m.url).await, spendable, "spendable sats");
    let w = core_wallet(&s.home, &m.url).await;
    let wallet_dir = s.home.join("wallet");
    assert!(
        shared_wallet::prepare_send_locked(
            &w,
            Some(&wallet_dir),
            Amount::from(spendable + 1),
            SendOptions::default()
        )
        .await
        .is_err(),
        "SAFETY: core cannot select more than the unowned proofs"
    );
    let p = shared_wallet::prepare_send_locked(
        &w,
        Some(&wallet_dir),
        Amount::from(spendable),
        SendOptions::default(),
    )
    .await
    .unwrap_or_else(|e| panic!("select the spendable sats: {e}"));
    let taken: Vec<_> = db
        .get_reserved_proofs(&p.operation_id())
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.y)
        .collect();
    assert!(
        taken
            .iter()
            .all(|y| !s.trade_ys.contains(y) && !s.melt_ys.contains(y)),
        "SAFETY: core selected a proof another owner holds"
    );
    p.cancel().await.unwrap();
    let e = maxplayer_trade::mint::plan(&s.home, &m.url, spendable + 1, 0)
        .await
        .err()
        .expect("SAFETY: trade cannot plan over another owner's proofs");
    // Either the plan finds too little unowned balance, or (review r6 H1) the payment gate already
    // refuses because recovery recorded the dead melt as payment-blocking.
    let e = format!("{e:#}");
    assert!(
        e.contains("insufficient") || e.contains(maxplayer_core::payment_wallet::PAYMENTS_REFUSED),
        "{e}"
    );
}
async fn retire(m: &MintFixture, home: &Path) -> maxplayer_core::payment_wallet::RetireReport {
    let w = core_wallet(home, &m.url).await;
    maxplayer_core::payment_wallet::retire_eligible_incomplete_sagas_in(
        &w,
        Some(&home.join("wallet")),
    )
    .await
    .unwrap()
}

/// H1 (advisor r1): recovering a spent swap saga never hands another owner's reservation back.
/// The mint fails the recovery part-way (its check-state after the restore, then every `/restore`
/// after the first: a full-wallet NUT-13 scan dies on its second batch after committing the first).
/// After each failure, and after the recovery that finally completes, trade's and a second cdk
/// saga's proofs are still reserved under the same owner and not selectable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_recovery_failing_on_the_second_restore_batch_keeps_every_reservation() {
    use std::sync::atomic::Ordering::SeqCst;
    let m = MintFixture::start(0).await;
    let s = spent_swap_scene(&m).await;
    m.faults.fail_checkstate_after_restore.store(true, SeqCst);
    let report = retire(&m, &s.home).await;
    assert_eq!(report.swap_recovered, 0, "{:?}", report.unresolved);
    assert!(!report.unresolved.is_empty());
    clear_recovery_faults(&m);
    assert_owners_kept(&m, &s, false).await;

    m.faults.restores_seen.store(0, SeqCst);
    m.faults.fail_restores_after.store(1, SeqCst);
    let report = retire(&m, &s.home).await;
    clear_recovery_faults(&m);
    let recovered = report.swap_recovered == 1;
    assert_owners_kept(&m, &s, recovered).await;
    if !recovered {
        let report = retire(&m, &s.home).await;
        assert_eq!(report.swap_recovered, 1, "{:?}", report.unresolved);
        assert_owners_kept(&m, &s, true).await;
    }
}

/// H1: the success path with a second in-flight cdk saga (an undecided melt) and trade's
/// reservation on the same wallet: recovery credits only the swap's own outputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_recovery_leaves_a_second_cdk_sagas_inputs_and_trades_reservation_reserved() {
    let m = MintFixture::start(0).await;
    let s = spent_swap_scene(&m).await;
    let report = retire(&m, &s.home).await;
    assert_eq!(report.swap_recovered, 1, "{:?}", report.unresolved);
    assert_owners_kept(&m, &s, true).await;
    // Idempotent: a second pass finds nothing to do and changes nothing.
    let report = retire(&m, &s.home).await;
    assert_eq!(report.swap_recovered, 0);
    assert_owners_kept(&m, &s, true).await;
}

/// Child half of [`swap_recovery_killed_mid_way_keeps_every_reservation`]: runs core's retire pass
/// on the home and mint named by the env var, then exits.
#[test]
fn swap_recovery_kill_child() {
    let Some(spec) = std::env::var_os("TRADE_TEST_SWAP_RECOVERY_CHILD") else {
        return;
    };
    let spec = spec.to_string_lossy().into_owned();
    let (home, mint) = spec.split_once('\n').unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mh = wallet::maxplayer_home(Path::new(home)).unwrap();
            let w = shared_wallet::open_shared_wallet(&mh, mint, None)
                .await
                .unwrap();
            let _ = maxplayer_core::payment_wallet::retire_eligible_incomplete_sagas_in(
                &w,
                Some(&Path::new(home).join("wallet")),
            )
            .await;
        });
}
/// Run the retire pass in a child process; SIGKILL it as soon as the mint holds the armed request.
/// Returns whether it was killed (false: it finished before reaching the hold).
async fn retire_in_child_killed_at_hold(m: &MintFixture, home: &Path) -> bool {
    let mut c = std::process::Command::new(std::env::current_exe().unwrap());
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        if k.starts_with("MAXPLAYER_") || k.to_ascii_lowercase().ends_with("_proxy") {
            c.env_remove(k);
        }
    }
    let mut child = c
        .args(["--exact", "swap_recovery_kill_child", "--nocapture"])
        .env(
            "TRADE_TEST_SWAP_RECOVERY_CHILD",
            format!("{}\n{}", home.display(), m.url),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        tokio::select! {
            _ = m.faults.recovery_held.notified() => {
                // Safety: plain kill(2) of our own child.
                assert_eq!(unsafe { libc_kill(child.id() as i32, 9) }, 0);
                assert!(!child.wait().unwrap().success(), "killed");
                return true;
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "child recovery failed: {status:?}");
                    return false;
                }
                assert!(std::time::Instant::now() < deadline, "child never finished");
            }
        }
    }
}

/// H1: `kill -9` of the recovering process (a) while the mint holds a second `/restore` (a
/// full-wallet scan has committed its first batch by then) and (b) after the mint answered the
/// saga's restore, before the local apply. Either way, trade's and the melt saga's proofs stay
/// reserved under their owners; a later pass then completes the swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_recovery_killed_mid_way_keeps_every_reservation() {
    use std::sync::atomic::Ordering::SeqCst;
    let m = MintFixture::start(0).await;
    let s = spent_swap_scene(&m).await;
    m.faults.hold_second_restore.store(true, SeqCst);
    let killed = retire_in_child_killed_at_hold(&m, &s.home).await;
    clear_recovery_faults(&m);
    let db = wallet::database(&s.home, &m.url).await.unwrap();
    let recovered = db.get_saga(&s.swap_saga).await.unwrap().is_none();
    assert!(
        killed || recovered,
        "the child either was killed or finished"
    );
    assert_owners_kept(&m, &s, recovered).await;

    let s = spent_swap_scene(&m).await;
    m.faults.hold_checkstate_after_restore.store(true, SeqCst);
    assert!(
        retire_in_child_killed_at_hold(&m, &s.home).await,
        "the recovery must reach its post-restore check"
    );
    clear_recovery_faults(&m);
    assert_owners_kept(&m, &s, false).await;
    let report = retire(&m, &s.home).await;
    assert_eq!(report.swap_recovered, 1, "{:?}", report.unresolved);
    assert_owners_kept(&m, &s, true).await;
}

/// M2 (advisor r1): the selection lock is never held across a mint request. With every mint reply
/// delayed 600 ms, core's send and melt selection and a trade send run while another holder takes
/// the lock every few ms with a 250 ms budget: it never finds it busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_select_lock_is_not_held_across_a_slow_mint() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 64).await;
    let j = Journal::open(home).await.unwrap();
    let w = core_wallet(home, &m.url).await;
    let wallet_dir = home.join("wallet");
    let q = w
        .melt_quote(PaymentMethod::BOLT11, invoice(2), None, None)
        .await
        .unwrap();
    m.faults.delay_ms.store(600, SeqCst);
    let done = AtomicBool::new(false);
    let probe = async {
        let (mut busy, mut taken, mut worst) = (0u32, 0u32, Duration::ZERO);
        while !done.load(SeqCst) {
            let started = std::time::Instant::now();
            match shared_wallet::acquire(&wallet_dir, Duration::from_millis(250)).await {
                Ok(g) => {
                    taken += 1;
                    worst = worst.max(started.elapsed());
                    drop(g);
                }
                Err(_) => busy += 1,
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (busy, taken, worst)
    };
    let work = async {
        let started = std::time::Instant::now();
        let p = shared_wallet::prepare_send_locked(
            &w,
            Some(&wallet_dir),
            Amount::from(3),
            SendOptions::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("core send selection: {e}"));
        p.cancel().await.unwrap();
        let p = match shared_wallet::prepare_melt_locked(&w, Some(&wallet_dir), &q.id).await {
            Ok(p) => p,
            Err(e) => panic!("core melt selection: {e}"),
        };
        p.cancel().await.unwrap();
        let r = send::send(home, &j, &m.url, 5, &home.join("token"), None)
            .await
            .unwrap();
        assert_eq!(r.state, send::SendState::Sent, "{}", r.summary());
        done.store(true, SeqCst);
        started.elapsed()
    };
    let ((busy, taken, worst), took) = tokio::join!(probe, work);
    m.faults.delay_ms.store(0, SeqCst);
    assert!(
        took > Duration::from_millis(1800),
        "the mint was slow: {took:?}"
    );
    assert!(taken > 10, "probe ran: {taken}");
    assert_eq!(
        busy, 0,
        "SAFETY/liveness: the selection lock was held across a mint request (worst wait {worst:?})"
    );
}

/// M3 (advisor r1): a maxplayer home (`key`, `config.toml`, `wallet/`) with a legacy root
/// `owner.lock` (and then `wallet.seed`) left by an older binary. Every command refuses, naming the
/// stray files and what to do; nothing is created (no `wallet.seed`, no `trade.key`). The library
/// keeps reading the home's `wallet/cdk-wallet.sqlite` and signing with `key`.
#[tokio::test]
async fn a_maxplayer_home_with_a_legacy_marker_is_refused_and_never_gets_a_second_seed() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 9).await;
    std::fs::write(home.join("owner.lock"), b"").unwrap();
    let listing = || {
        let mut v: Vec<_> = std::fs::read_dir(home)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let before = listing();
    for args in [
        vec!["balance", m.url.as_str()],
        vec!["status"],
        vec!["recover"],
        vec![
            "send",
            m.url.as_str(),
            "--amount",
            "2",
            "--out",
            "/nonexistent/t",
        ],
        vec!["withdraw", m.url.as_str(), "--invoice", "lnbc1"],
        vec!["serve"],
    ] {
        let out = trade(Some(home), &args);
        let e = err(&out);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {e}");
        assert!(
            e.contains("refused:") && e.contains("owner.lock") && e.contains("delete them"),
            "{args:?}: {e}"
        );
        assert_eq!(listing(), before, "{args:?}: SAFETY: nothing created");
    }
    assert_eq!(
        wallet::db_path(home, &m.url),
        home.join("wallet").join("cdk-wallet.sqlite"),
        "the home's wallet, never a per-mint legacy database"
    );
    assert_eq!(wallet::read_balance(home, &m.url).unwrap(), 9);
    let key = std::fs::read_to_string(home.join("key")).unwrap();
    assert_eq!(
        wallet::identity(home).unwrap().secret_key().to_secret_hex(),
        key.trim()
    );
    assert_eq!(balance(home, &m.url).await, 9);
    for f in ["wallet.seed", "trade.key"] {
        assert!(!home.join(f).exists(), "SAFETY: {f} created");
    }
    // With money-bearing legacy files: move them, never delete.
    std::fs::write(home.join("wallet.seed"), [7u8; 64]).unwrap();
    let out = trade(Some(home), &["balance", &m.url]);
    let e = err(&out);
    assert_eq!(out.status.code(), Some(1), "{e}");
    assert!(
        e.contains("wallet.seed")
            && e.contains("Move exactly those files")
            && !e.contains("delete them"),
        "{e}"
    );
    assert!(!home.join("trade.key").exists());
    std::fs::remove_file(home.join("wallet.seed")).unwrap();
    std::fs::remove_file(home.join("owner.lock")).unwrap();
    let out = trade(Some(home), &["balance", &m.url]);
    assert!(out.status.success(), "{}", err(&out));
}

/// L5 (advisor r1): trade's credit of new proofs runs under the selection lock and is owner
/// checked: an existing row keeps its owner (never overwritten), and a spent input is dropped only
/// while this operation (or nobody) owns it; never a row another operation or a cdk saga holds.
#[tokio::test]
async fn trade_credit_never_overwrites_or_drops_another_owners_rows() {
    use cdk_common::wallet::ProofInfo;
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 31).await;
    let db = wallet::database(home, &m.url).await.unwrap();
    let mut ys: Vec<_> = rows(home, &m.url, vec![State::Unspent])
        .await
        .into_iter()
        .map(|p| p.y)
        .collect();
    ys.sort();
    assert!(ys.len() >= 5);
    let (a, b, c, d) = (ys[0], ys[1], ys[2], ys[3]);
    let (other, mine) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    db.reserve_proofs(vec![a], &other).await.unwrap();
    db.reserve_proofs(vec![b], &mine).await.unwrap();
    let row_a = db.get_proofs_by_ys(vec![a]).await.unwrap().remove(0);
    let relabelled = ProofInfo::new_with_operations(
        row_a.proof.clone(),
        row_a.mint_url.clone(),
        State::Reserved,
        cdk::nuts::CurrencyUnit::Sat,
        Some(mine),
        Some(mine),
    )
    .unwrap();
    {
        let _held =
            shared_wallet::acquire(&home.join("wallet"), shared_wallet::SELECT_LOCK_TIMEOUT)
                .await
                .unwrap();
        let e = wallet::credit(
            home,
            &m.url,
            vec![relabelled.clone()],
            &[a, b, c],
            wallet::InputOwner::Op(mine),
        )
        .await
        .unwrap_err();
        assert!(format!("{e:#}").contains("busy"), "{e:#}");
        assert_eq!(db.get_proofs_by_ys(vec![a, b, c]).await.unwrap().len(), 3);
    }
    wallet::credit(
        home,
        &m.url,
        vec![relabelled],
        &[a, b, c],
        wallet::InputOwner::Op(mine),
    )
    .await
    .unwrap();
    let state = |y| {
        let db = db.clone();
        async move {
            db.get_proofs_by_ys(vec![y])
                .await
                .unwrap()
                .first()
                .map(|r| (r.state, r.used_by_operation))
        }
    };
    assert_eq!(
        state(a).await,
        Some((State::Reserved, Some(other))),
        "SAFETY: another owner's row is neither overwritten nor dropped"
    );
    assert_eq!(state(b).await, None, "this operation's spent input dropped");
    assert_eq!(state(c).await, None, "an unowned spent input dropped");
    assert_eq!(state(d).await, Some((State::Unspent, None)), "untouched");
    // A journal without a recorded input owner never drops a cdk saga's row.
    let w = core_wallet(home, &m.url).await;
    let p = shared_wallet::prepare_send_locked(
        &w,
        Some(&home.join("wallet")),
        Amount::from(1),
        SendOptions::default(),
    )
    .await
    .unwrap();
    let saga_ys: Vec<_> = db
        .get_reserved_proofs(&p.operation_id())
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.y)
        .collect();
    wallet::credit(
        home,
        &m.url,
        vec![],
        &saga_ys,
        wallet::InputOwner::NotACdkSaga,
    )
    .await
    .unwrap();
    assert_eq!(
        db.get_reserved_proofs(&p.operation_id())
            .await
            .unwrap()
            .len(),
        saga_ys.len(),
        "SAFETY: a cdk saga's input is never dropped by trade"
    );
    p.cancel().await.unwrap();
}

/// M3 (advisor r2): a journal without a recorded input owner (`InputOwner::NotACdkSaga`) drops a
/// spent input only while NOBODY owns it. A row another trade operation holds (not a cdk saga) is
/// left exactly as it is; an unowned one is dropped.
#[tokio::test]
async fn trade_credit_without_an_input_owner_leaves_another_trade_operations_row() {
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 7).await;
    let db = wallet::database(home, &m.url).await.unwrap();
    let mut ys: Vec<_> = rows(home, &m.url, vec![State::Unspent])
        .await
        .into_iter()
        .map(|p| p.y)
        .collect();
    ys.sort();
    assert!(ys.len() >= 2);
    let (held, free) = (ys[0], ys[1]);
    let trade_op = uuid::Uuid::new_v4();
    wallet::reserve(home, &m.url, &[held], &trade_op)
        .await
        .unwrap();
    assert!(
        db.get_saga(&trade_op).await.unwrap().is_none(),
        "not a cdk saga"
    );
    wallet::credit(
        home,
        &m.url,
        vec![],
        &[held, free],
        wallet::InputOwner::NotACdkSaga,
    )
    .await
    .unwrap();
    let state = |y| {
        let db = db.clone();
        async move {
            db.get_proofs_by_ys(vec![y])
                .await
                .unwrap()
                .first()
                .map(|r| (r.state, r.used_by_operation))
        }
    };
    assert_eq!(
        state(held).await,
        Some((State::Reserved, Some(trade_op))),
        "SAFETY: another trade operation's row survives an owner-less credit"
    );
    assert_eq!(state(free).await, None, "an unowned spent input is dropped");
}

/// H1 (advisor r2): the mint executed the swap but its `/restore` answers empty (a lagging or
/// lost restore). The saga is KEPT (held, retryable), nothing is written, trade's and the melt
/// saga's reservations keep their owners, and the next pass with an answering mint recovers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_recovery_with_an_empty_restore_keeps_the_saga_until_the_mint_answers() {
    use std::sync::atomic::Ordering::SeqCst;
    let m = MintFixture::start(0).await;
    let s = spent_swap_scene(&m).await;
    m.faults.blank_restores.store(1, SeqCst);
    let report = retire(&m, &s.home).await;
    assert_eq!(report.swap_recovered, 0, "{report:?}");
    assert_eq!(report.swap_held.len(), 1, "{report:?}");
    assert_eq!(report.swap_held[0].saga_id, s.swap_saga.to_string());
    assert_eq!(
        m.faults.blank_restores.load(SeqCst),
        0,
        "the blank restore was used"
    );
    assert_owners_kept(&m, &s, false).await;
    let report = retire(&m, &s.home).await;
    assert_eq!(report.swap_recovered, 1, "{report:?}");
    assert_owners_kept(&m, &s, true).await;
}

/// L5: every proof-row write in trade goes through `wallet::{credit, reserve, release}` (selection
/// lock, owner checked); no other module writes the shared wallet's proof table directly.
#[test]
fn trade_writes_proof_rows_only_through_the_locked_wallet_helpers() {
    for (name, source) in [
        ("mint.rs", include_str!("../src/mint.rs")),
        ("money.rs", include_str!("../src/money.rs")),
        ("receive.rs", include_str!("../src/receive.rs")),
        ("send.rs", include_str!("../src/send.rs")),
        ("coordinator.rs", include_str!("../src/coordinator.rs")),
        ("serve.rs", include_str!("../src/serve.rs")),
        ("main.rs", include_str!("../src/main.rs")),
        ("lib.rs", include_str!("../src/lib.rs")),
    ] {
        for call in [
            ".update_proofs(",
            ".update_proofs_state(",
            ".reserve_proofs(",
            ".release_proofs(",
        ] {
            assert!(
                !source.contains(call),
                "{name} calls {call}: write proof rows through wallet::credit/reserve/release"
            );
        }
    }
}

/// Review r6 H1: a buyer send on this shared home crashed after posting its swap: the send saga is
/// `ProofsReserved` (no process holds the operation lock) and its child swap (blinded messages
/// stored, nothing reserved) exists. Returns (send saga, child swap saga).
async fn crashed_core_send(home: &Path, mint: &str) -> (uuid::Uuid, uuid::Uuid) {
    use cdk::wallet::types::{
        OperationData, SwapOperationData, SwapSagaState, WalletSaga, WalletSagaState,
    };
    let keyset = rows(home, mint, vec![State::Unspent]).await[0]
        .proof
        .keyset_id;
    let w = core_wallet(home, mint).await;
    let p = shared_wallet::prepare_send_locked(
        &w,
        Some(&home.join("wallet")),
        Amount::from(3),
        SendOptions::default(),
    )
    .await
    .unwrap_or_else(|e| panic!("core send selection: {e}"));
    let parent = p.operation_id();
    drop(p); // its process died: the send stays ProofsReserved, its inputs reserved
    let child = uuid::Uuid::new_v4();
    wallet::database(home, mint)
        .await
        .unwrap()
        .add_saga(WalletSaga::new(
            child,
            WalletSagaState::Swap(SwapSagaState::SwapRequested),
            Amount::from(3),
            mint.parse().unwrap(),
            cdk::nuts::CurrencyUnit::Sat,
            OperationData::Swap(SwapOperationData {
                input_amount: Amount::from(3),
                output_amount: Amount::from(3),
                counter_start: Some(0),
                counter_end: Some(1),
                blinded_messages: Some(vec![cashu::BlindedMessage::new(
                    Amount::from(1),
                    keyset,
                    cashu::SecretKey::generate().public_key(),
                )]),
            }),
        ))
        .await
        .unwrap();
    (parent, child)
}

/// Review r6 H1: while a crashed buyer send on this mint waits on its child swap (its inputs
/// UNSPENT at the mint, its swap may still commit), every trade selection on the mint is refused by
/// core's payment gate before choosing a proof: `plan`, `reserve_new` of a plan made before the
/// crash, `send` and `withdraw`. Nothing is journaled or reserved. Once the send is gone (only the
/// child is left), trading continues.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trade_selection_is_refused_while_a_crashed_buyer_send_waits_on_its_swap() {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let m = MintFixture::start(0).await;
    let h = maxplayer_home();
    let home = h.path();
    fund(home, &m.url, 64).await;
    let j = Journal::open(home).await.unwrap();
    let early = maxplayer_trade::mint::plan(home, &m.url, 5, 10)
        .await
        .unwrap();
    let (parent, _child) = crashed_core_send(home, &m.url).await;
    let refused = |what: &str, e: anyhow::Error| {
        let e = format!("{e:#}");
        assert!(
            e.contains(maxplayer_core::payment_wallet::PAYMENTS_REFUSED)
                && e.contains(&parent.to_string())
                && e.contains("#1272"),
            "SAFETY (r6 H1): {what} must be refused by the payment gate: {e}"
        );
    };
    refused(
        "plan",
        maxplayer_trade::mint::plan(home, &m.url, 5, 10)
            .await
            .err()
            .expect("plan refused"),
    );
    let journaled = AtomicBool::new(false);
    refused(
        "reserve_new",
        maxplayer_trade::mint::reserve_new(
            home,
            &early,
            &uuid::Uuid::new_v4().to_string(),
            || async {
                journaled.store(true, SeqCst);
                Ok::<(), anyhow::Error>(())
            },
        )
        .await
        .expect_err("reserve_new refused"),
    );
    assert!(!journaled.load(SeqCst), "SAFETY: nothing journaled");
    refused(
        "send",
        send::send(home, &j, &m.url, 5, &home.join("token"), None)
            .await
            .err()
            .expect("send refused"),
    );
    assert!(j.all::<send::SendAttempt>("send").await.unwrap().is_empty());
    refused(
        "withdraw",
        money::withdraw(home, &j, &m.url, &invoice(2))
            .await
            .err()
            .expect("withdraw refused"),
    );
    assert!(
        m.faults.melt_requests.lock().unwrap().is_empty(),
        "SAFETY: no melt posted"
    );
    let reserved = rows(home, &m.url, IN_FLIGHT.to_vec()).await;
    assert!(
        !reserved.is_empty() && reserved.iter().all(|r| r.used_by_operation == Some(parent)),
        "SAFETY: only the crashed send's inputs are reserved: {reserved:?}"
    );

    // The send is gone (cdk compensated it after a definitive refusal); the child alone never
    // refuses, so trading continues.
    let db = wallet::database(home, &m.url).await.unwrap();
    db.release_proofs(&parent).await.unwrap();
    db.delete_saga(&parent).await.unwrap();
    let plan = maxplayer_trade::mint::plan(home, &m.url, 5, 10)
        .await
        .unwrap_or_else(|e| panic!("a child alone must not stop trading: {e:#}"));
    maxplayer_trade::mint::reserve_new(home, &plan, &uuid::Uuid::new_v4().to_string(), || async {
        Ok::<(), anyhow::Error>(())
    })
    .await
    .unwrap();
}
