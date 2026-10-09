mod support;
use maxplayer_trade::{
    journal::Journal,
    money::{self, MeltState, Withdrawal},
};
use std::sync::atomic::Ordering::SeqCst;
use support::*;
fn invoice(n: u64) -> String {
    cdk_fake_wallet::create_fake_invoice(n * 1000, String::new()).to_string()
}
async fn setup() -> (tempfile::TempDir, MintFixture, Journal) {
    let home = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    fund(home.path(), &m.url, 128).await;
    let j = Journal::open(home.path()).await.unwrap();
    (home, m, j)
}
#[tokio::test]
async fn melt_exact_change_and_idempotent_recovery() {
    let (h, m, j) = setup().await;
    let a = money::withdraw(h.path(), &j, &m.url, &invoice(31))
        .await
        .unwrap();
    assert_eq!(a.state, MeltState::Done);
    let accounting = a.summary();
    assert_eq!(
        a.change,
        Some(accounting["input_total"].as_u64().unwrap() - 32)
    );
    assert_eq!(accounting["lightning_fee"], 1);
    assert_eq!(balance(h.path(), &m.url).await, 96);
    money::recover(h.path(), &j).await.unwrap();
    assert_eq!(balance(h.path(), &m.url).await, 96);
}
#[tokio::test]
async fn melt_restore_failure_keeps_change_until_reopen() {
    let (h, m, j) = setup().await;
    m.faults.reject_restore.store(true, SeqCst);
    let _ = money::withdraw(h.path(), &j, &m.url, &invoice(31)).await;
    let a = j
        .all::<Withdrawal>("withdrawal")
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        a.state,
        MeltState::PaidChangeUnreconciled,
        "SAFETY: restore failure must not mark done"
    );
    assert_eq!(
        a.change, None,
        "SAFETY: missing restore must not be accounted as zero change"
    );
    drop(j);
    m.faults.reject_restore.store(false, SeqCst);
    let j = Journal::open(h.path()).await.unwrap();
    money::recover(h.path(), &j).await.unwrap();
    assert_eq!(
        balance(h.path(), &m.url).await,
        96,
        "SAFETY: all change must survive reopen"
    );
    assert_eq!(
        j.all::<Withdrawal>("withdrawal").await.unwrap()[0].state,
        MeltState::Done
    );
}
#[tokio::test]
async fn melt_lost_reply_restores_exact_change() {
    let (h, m, j) = setup().await;
    m.faults.lose_melt_reply.store(true, SeqCst);
    let a = money::withdraw(h.path(), &j, &m.url, &invoice(31))
        .await
        .unwrap();
    assert_eq!(
        a.state,
        MeltState::Done,
        "lost reply can reconcile immediately via GET and restore"
    );
    drop(j);
    let j = Journal::open(h.path()).await.unwrap();
    money::recover(h.path(), &j).await.unwrap();
    assert_eq!(balance(h.path(), &m.url).await, 96);
}
#[tokio::test]
async fn cumulative_funding_includes_unissued_quotes_across_reopen() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    money::fund(h.path(), &j, &m.url, 40_000).await.unwrap();
    money::fund(h.path(), &j, &m.url, 40_001).await.unwrap();
    drop(j);
    let j = Journal::open(h.path()).await.unwrap();
    assert!(money::fund(h.path(), &j, &m.url, 20_000).await.is_err());
    assert_eq!(j.all::<money::Funding>("funding").await.unwrap().len(), 2);
    money::fund(h.path(), &j, &m.url, 19_999).await.unwrap();
    assert!(money::fund(h.path(), &j, &m.url, 1).await.is_err());
}
#[tokio::test]
async fn funding_recover_issues_paid_quote_once() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    let f = money::fund(h.path(), &j, &m.url, 100).await.unwrap();
    assert!(f.invoice.is_some());
    drop(j);
    let j = Journal::open(h.path()).await.unwrap();
    for _ in 0..30 {
        money::recover(h.path(), &j).await.unwrap();
        if !money::pending(&j).await.unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(balance(h.path(), &m.url).await, 100);
    money::recover(h.path(), &j).await.unwrap();
    assert_eq!(balance(h.path(), &m.url).await, 100);
}
#[cfg(feature = "lab")]
#[tokio::test]
async fn crash_after_melt_reply_recovers_without_resubmission() {
    let (h, m, j) = setup().await;
    let home = h.path().to_owned();
    let url = m.url.clone();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
            .args([
                "--home",
                home.to_str().unwrap(),
                "withdraw",
                &url,
                "--invoice",
                &invoice(31),
            ])
            .env("TRADE_CRASH_AFTER_MELT", "1")
            .output()
            .unwrap()
            .status
    })
    .await
    .unwrap();
    assert_eq!(status.code(), Some(86));
    money::recover(h.path(), &j).await.unwrap();
    assert_eq!(balance(h.path(), &m.url).await, 96);
}
#[tokio::test]
async fn unpaid_response_releases_reserved_inputs() {
    use cashu::nuts::MeltQuoteState;
    let (h, m, j) = setup().await;
    let description = cdk_fake_wallet::FakeInvoiceDescription {
        pay_invoice_state: MeltQuoteState::Unpaid,
        check_payment_state: MeltQuoteState::Unpaid,
        pay_err: false,
        check_err: false,
    };
    let invoice =
        cdk_fake_wallet::create_fake_invoice(31_000, serde_json::to_string(&description).unwrap())
            .to_string();
    let _ = money::withdraw(h.path(), &j, &m.url, &invoice).await;
    money::recover(h.path(), &j).await.unwrap();
    let a = j
        .all::<Withdrawal>("withdrawal")
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(a.state, MeltState::UnpaidReleased);
    assert_eq!(balance(h.path(), &m.url).await, 128);
}
#[tokio::test]
async fn pending_then_paid_waits_and_reconciles_change() {
    use cashu::nuts::MeltQuoteState;
    let (h, m, j) = setup().await;
    m.faults.prefer_async_melt.store(true, SeqCst);
    let description = cdk_fake_wallet::FakeInvoiceDescription {
        pay_invoice_state: MeltQuoteState::Pending,
        check_payment_state: MeltQuoteState::Paid,
        pay_err: false,
        check_err: false,
    };
    let invoice =
        cdk_fake_wallet::create_fake_invoice(31_000, serde_json::to_string(&description).unwrap())
            .to_string();
    let a = money::withdraw(h.path(), &j, &m.url, &invoice)
        .await
        .unwrap();
    assert_eq!(a.state, MeltState::Pending);
    for _ in 0..30 {
        money::recover(h.path(), &j).await.unwrap();
        if !money::pending(&j).await.unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        j.all::<Withdrawal>("withdrawal").await.unwrap()[0].state,
        MeltState::Done
    );
    assert_eq!(balance(h.path(), &m.url).await, 97);
}
#[test]
fn cli_refuses_funding_above_cap_without_network() {
    let h = tempfile::tempdir().unwrap();
    let mint = "https://mint.minibits.cash/Bitcoin";
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "fund",
            mint,
            "--amount",
            "100001",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("100,000 sats"));
    assert!(!h.path().join("wallet.seed").exists());
}
#[tokio::test]
async fn published_funding_quote_cannot_be_issued_without_its_key() {
    use cashu::nuts::{MintRequest, MintResponse, PreMintSecrets};
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    let f = money::fund(h.path(), &j, &m.url, 100).await.unwrap();
    let w = maxplayer_trade::wallet::wallet(h.path(), &m.url)
        .await
        .unwrap();
    w.refresh_keysets().await.unwrap();
    let k = w.fetch_active_keyset().await.unwrap();
    let fees = w.get_keyset_fees_and_amounts_by_id(k.id).await.unwrap();
    let outputs = PreMintSecrets::random(k.id, 100.into(), &Default::default(), &fees).unwrap();
    let req = MintRequest {
        quote: f.quote.unwrap(),
        outputs: outputs
            .secrets
            .into_iter()
            .map(|o| o.blinded_message)
            .collect(),
        signature: None,
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let mut paid = false;
    for _ in 0..30 {
        let q: serde_json::Value = client
            .get(format!("{}/v1/mint/quote/bolt11/{}", m.url, req.quote))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if q["state"] == "PAID" {
            paid = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        paid,
        "fixture must be paid before testing unauthorized issuance"
    );
    let unsigned: anyhow::Result<MintResponse> =
        maxplayer_trade::mint::rpc(&m.url, "mint/bolt11", &req).await;
    assert!(
        unsigned.is_err(),
        "SAFETY: quote ID alone must not authorize issuance"
    );
    for _ in 0..30 {
        money::recover(h.path(), &j).await.unwrap();
        if !money::pending(&j).await.unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(balance(h.path(), &m.url).await, 100);
}
#[tokio::test]
async fn balance_does_not_issue_pending_funding_authorization() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    money::fund(h.path(), &j, &m.url, 100).await.unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args(["--home", h.path().to_str().unwrap(), "balance", &m.url])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["balance"],
        0
    );
    assert!(!j.all::<money::Funding>("funding").await.unwrap()[0].done);
}
#[tokio::test]
async fn coordinator_recovery_resumes_funding_without_new_requests() {
    let f = Fixture::new(0).await;
    money::fund(&f.maker, &f.jm, &f.b.url, 100).await.unwrap();
    for _ in 0..100 {
        maxplayer_trade::coordinator::recover(&f.maker, &f.jm, &f.mm)
            .await
            .unwrap();
        if !money::pending(&f.jm).await.unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !money::pending(&f.jm).await.unwrap(),
        "coordinator tick must resume money journals without admitting a new trade"
    );
    assert_eq!(balance(&f.maker, &f.b.url).await, 100);
}

#[tokio::test]
async fn funding_exact_cap_and_one_over() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    assert!(
        money::fund(h.path(), &j, &m.url, 100_001).await.is_err(),
        "SAFETY: funding cap must reject 100,001"
    );
    assert!(
        j.all::<money::Funding>("funding").await.unwrap().is_empty(),
        "SAFETY: over-cap funding must not create an intent"
    );
    money::fund(h.path(), &j, &m.url, 100_000).await.unwrap();
    drop(j);
    let j = Journal::open(h.path()).await.unwrap();
    assert!(
        money::fund(h.path(), &j, &m.url, 1).await.is_err(),
        "SAFETY: cumulative funding cap must survive restart"
    );
    assert_eq!(j.all::<money::Funding>("funding").await.unwrap().len(), 1);
}

#[tokio::test]
async fn failed_preflight_refuses_funding_before_intent() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    for nut in [4, 7, 9, 11, 12, 14, 20] {
        m.faults.missing_nut.store(nut, SeqCst);
        assert!(money::fund(h.path(), &j, &m.url, 1).await.is_err());
        assert!(
            j.all::<money::Funding>("funding").await.unwrap().is_empty(),
            "SAFETY: failed preflight must not authorize funding"
        );
        assert!(!h.path().join("wallet.seed").exists());
    }
    m.faults.missing_nut.store(0, SeqCst);
    m.faults.clock_offset.store(61, SeqCst);
    assert!(money::fund(h.path(), &j, &m.url, 1).await.is_err());
    assert!(j.all::<money::Funding>("funding").await.unwrap().is_empty());
}

#[tokio::test]
async fn preflight_requires_active_sat_keyset_before_funding() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    m.faults.no_sat_keyset.store(true, SeqCst);
    let e = money::fund(h.path(), &j, &m.url, 1).await.err().unwrap();
    assert!(e.to_string().contains("active sat keyset"));
    assert!(j.all::<money::Funding>("funding").await.unwrap().is_empty());
}

#[tokio::test]
async fn recover_reports_unreachable_item_and_reconciles_other_withdrawal() {
    let (h, m, j) = setup().await;
    m.faults.reject_restore.store(true, SeqCst);
    let _ = money::withdraw(h.path(), &j, &m.url, &invoice(31)).await;
    let paid = j
        .all::<Withdrawal>("withdrawal")
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(paid.state, MeltState::PaidChangeUnreconciled);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mut stuck = serde_json::to_value(&paid).unwrap();
    let id = "00000000-0000-4000-8000-000000000002";
    stuck["id"] = id.into();
    stuck["mint"] = dead.into();
    j.put("withdrawal", id, &stuck).await.unwrap();
    m.faults.reject_restore.store(false, SeqCst);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
            .args(["--home", h.path().to_str().unwrap(), "recover"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("SAFETY: money-only recovery must return despite unreachable mint")
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8(output.stdout).unwrap().contains(id));
    assert_eq!(
        j.get::<Withdrawal>("withdrawal", &paid.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        MeltState::Done,
        "SAFETY: unreachable item must not prevent another withdrawal reconciliation"
    );
    assert_eq!(
        j.get::<serde_json::Value>("withdrawal", id)
            .await
            .unwrap()
            .unwrap(),
        stuck,
        "SAFETY: stuck authorization unchanged"
    );
    assert_eq!(balance(h.path(), &m.url).await, 96);
}

#[tokio::test]
async fn withdrawal_exact_cap_and_one_over() {
    let h = tempfile::tempdir().unwrap();
    // Keep the existing 32-sat reserve policy: this mint charges one sat.
    let m = MintFixture::with_melt_reserve(0, 0.0).await;
    // Fixture issuance represents a trading balance above the lifetime funding cap.
    fund(h.path(), &m.url, 100_032).await;
    let j = Journal::open(h.path()).await.unwrap();
    let before = balance(h.path(), &m.url).await;
    let rejected = money::withdraw(h.path(), &j, &m.url, &invoice(100_001)).await;
    assert!(
        rejected.is_err(),
        "SAFETY: withdrawal cap must reject 100,001"
    );
    assert!(
        j.all::<Withdrawal>("withdrawal").await.unwrap().is_empty(),
        "SAFETY: over-cap invoice must not create a payment authorization"
    );
    assert_eq!(
        balance(h.path(), &m.url).await,
        before,
        "SAFETY: over-cap invoice must not move funds"
    );
    let paid = money::withdraw(h.path(), &j, &m.url, &invoice(100_000))
        .await
        .unwrap();
    assert_eq!(
        paid.state,
        MeltState::Done,
        "exact withdrawal cap must be payable"
    );
    assert_eq!(paid.summary()["lightning_fee"], 1);
    assert_eq!(balance(h.path(), &m.url).await, 31);
    drop(j);
    let j = Journal::open(h.path()).await.unwrap();
    money::recover(h.path(), &j).await.unwrap();
    assert_eq!(
        balance(h.path(), &m.url).await,
        31,
        "SAFETY: exact-cap withdrawal change must survive restart"
    );
}
