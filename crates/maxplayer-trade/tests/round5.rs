//! Final-review regressions. Appendix A probes inverted at their safety outcomes.
mod support;
use maxplayer_trade::{
    Asset, Leg,
    coordinator::{self, Swap},
    journal::Journal,
    market::Market,
    mint,
    money::{self, MeltState, Withdrawal},
    wallet,
};
use nostr_relay_builder::prelude::*;
use std::sync::atomic::Ordering::SeqCst;
use support::*;
fn invoice(n: u64) -> String {
    cdk_fake_wallet::create_fake_invoice(n * 1000, String::new()).to_string()
}
async fn asymmetric() -> Fixture {
    asymmetric_fees(0, 100).await
}
async fn asymmetric_fees(a_fee: u64, b_fee: u64) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let a = MintFixture::start(a_fee).await;
    let b = MintFixture::start(b_fee).await;
    let maker = root.path().join("maker");
    let taker = root.path().join("taker");
    fund(&maker, &a.url, 128).await;
    fund(&taker, &b.url, 128).await;
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    let relay_url = relay.url().await.to_string();
    let mm = Market::connect(
        wallet::identity(&maker).unwrap(),
        std::slice::from_ref(&relay_url),
    )
    .await
    .unwrap();
    let mt = Market::connect(
        wallet::identity(&taker).unwrap(),
        std::slice::from_ref(&relay_url),
    )
    .await
    .unwrap();
    let jm = Journal::open(&maker).await.unwrap();
    let jt = Journal::open(&taker).await.unwrap();
    Fixture {
        root,
        a,
        b,
        maker,
        taker,
        jm,
        jt,
        mm,
        mt,
        relay,
        relay_url,
    }
}
#[tokio::test]
async fn r5_probe_maker_cap_rejects_before_quote_or_lock() {
    let mut f = asymmetric().await;
    let lot = coordinator::list(
        &f.maker,
        &f.jm,
        &f.mm,
        Leg {
            asset: Asset::new(&f.a.url).unwrap(),
            net: 32,
        },
        Leg {
            asset: Asset::new(&f.b.url).unwrap(),
            net: 24,
        },
        0,
    )
    .await
    .unwrap();
    let id = f.start(&lot).await;
    let e = tokio::time::timeout(std::time::Duration::from_secs(3), f.mm.inbox.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        coordinator::handle(&f.maker, &f.jm, &f.mm, &e)
            .await
            .is_err(),
        "SAFETY: maker must refuse incoming fee before quoting"
    );
    assert!(f.jm.get::<Swap>("swap", &id).await.unwrap().is_none());
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(balance(&f.taker, &f.a.url).await, 0);
}
#[tokio::test]
async fn r5_pinned_sender_fee_claims_even_with_legacy_receiver_cap() {
    let mut f = asymmetric().await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.pump(&id, "claimed").await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    s.max_fees = 0;
    f.jm.put("swap", &id, &s).await.unwrap();
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        24,
        "SAFETY: sender-funded pinned claim must not be blocked by receiver cap"
    );
    assert_eq!(f.state(true, &id).await.as_deref(), Some("complete"));
    assert_eq!(balance(&f.taker, &f.a.url).await, 32);
}
#[tokio::test]
async fn r5_probe_late_quote_never_resurrects_expired_taker() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    // Same late-delivery window as Appendix A, without a 61-second wall-clock wait.
    let mut s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    s.created = coordinator::now() - 61;
    f.jt.put("swap", &id, &s).await.unwrap();
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(f.state(false, &id).await.as_deref(), Some("expired"));
    f.step(false).await;
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("expired"),
        "SAFETY: terminal swap cannot resurrect from late quote"
    );
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .is_none(),
        "SAFETY: released inputs must not lock"
    );
}
#[tokio::test]
async fn r5_probe_reserve_refusal_terminal_and_same_invoice_retry() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::with_melt_reserve(0, 0.03).await;
    fund(h.path(), &m.url, 8000).await;
    let j = Journal::open(h.path()).await.unwrap();
    let inv = invoice(4000);
    assert!(money::withdraw(h.path(), &j, &m.url, &inv).await.is_err());
    assert_eq!(
        j.all::<Withdrawal>("withdrawal").await.unwrap()[0].state,
        MeltState::Refused,
        "SAFETY: refused quote must be terminal"
    );
    for _ in 0..3 {
        assert!(
            !money::recover(h.path(), &j).await.unwrap(),
            "SAFETY: refusal must not wedge recovery"
        );
    }
    assert!(money::withdraw(h.path(), &j, &m.url, &inv).await.is_err());
    assert_eq!(
        j.all::<Withdrawal>("withdrawal").await.unwrap().len(),
        2,
        "same invoice can request a fresh quote after safe refusal"
    );
    assert_eq!(balance(h.path(), &m.url).await, 8000);
}
#[tokio::test]
async fn r5_hundred_thousand_at_two_percent_and_max_debit() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::with_melt_reserve(0, 0.02).await;
    fund(h.path(), &m.url, 110000).await;
    let j = Journal::open(h.path()).await.unwrap();
    let inv = invoice(100000);
    assert!(
        money::withdraw_bounded(h.path(), &j, &m.url, &inv, Some(101999))
            .await
            .is_err()
    );
    assert_eq!(
        j.all::<Withdrawal>("withdrawal").await.unwrap()[0].state,
        MeltState::Refused
    );
    assert!(
        m.faults.melt_requests.lock().unwrap().is_empty(),
        "SAFETY: max-debit must refuse before POST"
    );
    let a = money::withdraw_bounded(h.path(), &j, &m.url, &inv, Some(102000))
        .await
        .unwrap();
    assert_eq!(
        a.state,
        MeltState::Done,
        "SAFETY: 100,000 sats with 2% reserve must be payable"
    );
    assert_eq!(a.summary()["fee_reserve"], 2000);
}
#[tokio::test]
async fn r5_lost_quote_and_near_expiry_are_terminal_without_inputs() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    fund(h.path(), &m.url, 128).await;
    let j = Journal::open(h.path()).await.unwrap();
    m.faults.quote_error.store(true, SeqCst);
    assert!(
        money::withdraw(h.path(), &j, &m.url, &invoice(31))
            .await
            .is_err()
    );
    m.faults.quote_error.store(false, SeqCst);
    m.faults.near_expiry.store(true, SeqCst);
    assert!(
        money::withdraw(h.path(), &j, &m.url, &invoice(31))
            .await
            .is_err()
    );
    assert!(
        j.all::<Withdrawal>("withdrawal")
            .await
            .unwrap()
            .iter()
            .all(|a| a.state == MeltState::Refused),
        "SAFETY: no-input quote failures terminal"
    );
    assert!(!money::recover(h.path(), &j).await.unwrap());
    assert!(m.faults.melt_requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn r5_recover_never_authorizes_quote_created() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    let inv = invoice(31);
    assert!(money::withdraw(h.path(), &j, &m.url, &inv).await.is_err());
    let a = j
        .all::<Withdrawal>("withdrawal")
        .await
        .unwrap()
        .pop()
        .unwrap();
    // Model a crash after saving the quote but before preparation/failure handling.
    let mut v = serde_json::to_value(&a).unwrap();
    v["state"] = "quote_created".into();
    j.put("withdrawal", &a.id, &v).await.unwrap();
    fund(h.path(), &m.url, 128).await;
    let recovery = money::recover(h.path(), &j).await;
    assert!(
        m.faults.melt_requests.lock().unwrap().is_empty(),
        "SAFETY: no implicit POST"
    );
    let saved: serde_json::Value = j.get("withdrawal", &a.id).await.unwrap().unwrap();
    assert!(
        saved["inputs"].as_array().unwrap().is_empty(),
        "SAFETY: passive recovery cannot reserve inputs for an unsent invoice"
    );
    assert_eq!(
        balance(h.path(), &m.url).await,
        128,
        "SAFETY: passive recovery cannot pay an unsent invoice after funds arrive"
    );
    assert!(recovery.unwrap());
    assert_eq!(
        money::withdraw(h.path(), &j, &m.url, &inv)
            .await
            .unwrap()
            .state,
        MeltState::Done
    );
}
#[tokio::test]
async fn r5_nut_error_final_and_proxy_failure_replays_identical_request() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    fund(h.path(), &m.url, 128).await;
    let j = Journal::open(h.path()).await.unwrap();
    m.faults.melt_error.store(20007, SeqCst);
    let a = money::withdraw(h.path(), &j, &m.url, &invoice(31))
        .await
        .unwrap();
    assert_eq!(
        a.state,
        MeltState::UnpaidReleased,
        "SAFETY: numeric NUT error plus fresh UNPAID/UNSPENT releases"
    );
    m.faults.melt_error.store(0, SeqCst);
    m.faults.melt_proxy_error.store(true, SeqCst);
    let inv = invoice(31);
    let a = money::withdraw(h.path(), &j, &m.url, &inv).await.unwrap();
    assert_eq!(a.state, MeltState::RequestSent);
    money::recover(h.path(), &j).await.unwrap();
    m.faults.melt_proxy_error.store(false, SeqCst);
    money::recover(h.path(), &j).await.unwrap();
    let requests = m.faults.melt_requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[1..].windows(2).all(|p| p[0] == p[1]),
        "SAFETY: ambiguous replay must preserve exact request bytes"
    );
    assert_eq!(balance(h.path(), &m.url).await, 96);
}
#[tokio::test]
async fn r5_payment_hash_dedupes_case_and_mints() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let other = MintFixture::start(0).await;
    fund(h.path(), &m.url, 128).await;
    let j = Journal::open(h.path()).await.unwrap();
    let inv = invoice(31);
    let a = money::withdraw(h.path(), &j, &m.url, &inv).await.unwrap();
    assert_eq!(
        money::withdraw(h.path(), &j, &m.url, &inv.to_uppercase())
            .await
            .unwrap()
            .id,
        a.id
    );
    assert!(
        money::withdraw(h.path(), &j, &other.url, &inv)
            .await
            .is_err()
    );
    assert_eq!(j.all::<Withdrawal>("withdrawal").await.unwrap().len(), 1);
    assert!(other.faults.melt_requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn r5_expired_unpaid_funding_and_paid_issuance_without_preflight() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    let mut f = money::fund(h.path(), &j, &m.url, 32).await.unwrap();
    m.faults.expired_funding.store(true, SeqCst);
    assert!(
        !money::recover(h.path(), &j).await.unwrap(),
        "SAFETY: expired unpaid invoice is not perpetual pending work"
    );
    f = j.get("funding", &f.id).await.unwrap().unwrap();
    assert!(f.expired_unpaid);
    m.faults.expired_funding.store(false, SeqCst);
    m.faults.clock_offset.store(61, SeqCst);
    m.faults.missing_nut.store(14, SeqCst);
    for _ in 0..30 {
        money::resume_fund(h.path(), &j, &mut f).await.unwrap();
        if f.done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        f.done,
        "SAFETY: late paid issuance must not require admission preflight"
    );
    assert_eq!(balance(h.path(), &m.url).await, 32);
}
#[cfg(feature = "lab")]
async fn partial_claim_fixture() -> (Fixture, String) {
    let mut f = Fixture::new(0).await;
    let lot = coordinator::list(
        &f.maker,
        &f.jm,
        &f.mm,
        Leg {
            asset: Asset::new(&f.a.url).unwrap(),
            net: 24,
        },
        Leg {
            asset: Asset::new(&f.b.url).unwrap(),
            net: 24,
        },
        16,
    )
    .await
    .unwrap();
    let id = coordinator::start_take(&f.taker, &f.jt, &f.mt, &lot, 40, 24, 16)
        .await
        .unwrap();
    f.step(true).await;
    f.step(false).await;
    f.step(true).await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let t = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let chosen = vec![s.outgoing.iter().max_by_key(|p| p.amount).unwrap().clone()];
    mint::redeem(
        &f.taker,
        &f.jt,
        "partial",
        &f.a.url,
        &chosen,
        &t.key,
        t.preimage.as_ref().unwrap(),
        16,
        None,
    )
    .await
    .unwrap();
    s.preimage = t.preimage;
    s.state = "claiming".into();
    f.jm.put("swap", &id, &s).await.unwrap();
    (f, id)
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_refund_precedes_stalled_claim() {
    let (f, id) = partial_claim_fixture().await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.as_ref().unwrap().short + s.quote.as_ref().unwrap().margin).await;
    f.b.faults.hang_keysets.store(true, SeqCst);
    // Cancelling a recovery budget while the incoming mint accepts but never answers
    // must still leave our independently reachable own-mint refund committed.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        coordinator::advance(&f.maker, &f.jm, &f.mm, &mut s),
    )
    .await;
    assert!(result.is_err(), "fixture must stall in counterparty claim");
    assert_eq!(
        balance(&f.maker, &f.a.url).await,
        112,
        "SAFETY: own-mint remainder refunded BEFORE stalled claim consumes budget"
    );
    assert!(
        f.jm.get::<serde_json::Value>("attempt", &format!("{id}-refund"))
            .await
            .unwrap()
            .unwrap()["done"]
            == true
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_refund_quarantine_keeps_known_preimage_claim_live() {
    let (f, id) = partial_claim_fixture().await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.as_ref().unwrap().short + s.quote.as_ref().unwrap().margin).await;
    f.a.faults.invalid_dleq.store(true, SeqCst);
    f.b.faults.reject_swap.store(true, SeqCst);
    let _ = coordinator::advance(&f.maker, &f.jm, &f.mm, &mut s).await;
    assert_eq!(
        s.state, "claiming",
        "SAFETY: refund quarantine cannot terminate live incoming claim"
    );
    f.b.faults.reject_swap.store(false, SeqCst);
    coordinator::advance(&f.maker, &f.jm, &f.mm, &mut s)
        .await
        .unwrap();
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        24,
        "SAFETY: claim retried despite quarantined refund"
    );
    assert_eq!(s.state, "refund_quarantined");
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_detached_serve_stalled_mint_and_info_less_refund() {
    use std::process::{Command, Stdio};
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    f.step(true).await;
    assert_eq!(f.state(true, &id).await.as_deref(), Some("second_locked"));
    // A second peer sends a genuine request to a mint which then stops answering.
    let c = MintFixture::start(0).await;
    let other = f.root.path().join("other");
    fund(&other, &c.url, 128).await;
    let jt = Journal::open(&other).await.unwrap();
    let mt = Market::connect(
        wallet::identity(&other).unwrap(),
        std::slice::from_ref(&f.relay_url),
    )
    .await
    .unwrap();
    let other_lot = coordinator::list(
        &f.maker,
        &f.jm,
        &f.mm,
        Leg {
            asset: Asset::new(&f.a.url).unwrap(),
            net: 16,
        },
        Leg {
            asset: Asset::new(&c.url).unwrap(),
            net: 16,
        },
        16,
    )
    .await
    .unwrap();
    coordinator::start_take(&other, &jt, &mt, &other_lot, 32, 16, 16)
        .await
        .unwrap();
    c.faults.hang_keysets.store(true, SeqCst);
    f.a.faults.no_time.store(true, SeqCst);
    let log_path = f.root.path().join("serve.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = Command::new("setsid")
        .arg(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            f.maker.to_str().unwrap(),
            "--relay",
            &f.relay_url,
            "serve",
        ])
        .env("TRADE_LAB_SECONDS", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let pid = child.id();
    // setsid must give the final binary its own session, not merely background a shell.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let ps = Command::new("ps")
        .args(["-o", "pid=,pgid=,sid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let ids = String::from_utf8(ps.stdout)
        .unwrap()
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![pid.to_string(); 3], "detached PID/PGID/SID");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(75);
    let mut refunded = false;
    while tokio::time::Instant::now() < deadline {
        if f.state(true, &id).await.as_deref() == Some("refunded") {
            refunded = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let readonly = Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args(["--home", f.maker.to_str().unwrap(), "balance", &f.a.url])
        .output()
        .unwrap();
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let exit = child.wait().unwrap();
    let log = std::fs::read_to_string(log_path).unwrap();
    assert!(
        refunded,
        "SAFETY: stalled message mint must not starve another swap's automatic refund; {log}"
    );
    assert!(alive);
    assert!(readonly.status.success());
    // A second listing still reserves 16; the refunded first swap returns exactly 32.
    assert_eq!(
        wallet::read_balance(&f.maker, &f.a.url).unwrap(),
        112,
        "SAFETY: exact spendable balance after refund minus second lot reservation"
    );
    assert_eq!(balance(&f.maker, &f.b.url).await, 0);
    assert_eq!(balance(&f.taker, &f.a.url).await, 0);
    assert!(
        f.jm.get::<serde_json::Value>("attempt", &format!("{id}-refund"))
            .await
            .unwrap()
            .unwrap()["done"]
            == true
    );
    eprintln!(
        "DETACHED_REFUND pid={pid} pgid={pid} sid={pid} alive_until_refunded={alive} state=refunded maker_A=112 maker_B=0 taker_A=0 reserved_second_lot=16 no_manual_recover=true exit={exit}"
    );
}

#[cfg(feature = "lab")]
async fn wait_past(t: u64) {
    while coordinator::now() <= t {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
#[tokio::test]
async fn r5_taker_incoming_fee_cap_before_acceptance() {
    let mut f = asymmetric_fees(100, 0).await;
    let lot = f.list(false).await;
    let id = coordinator::start_take(&f.taker, &f.jt, &f.mt, &lot, 40, 32, 0)
        .await
        .unwrap();
    f.step(true).await;
    let e = tokio::time::timeout(std::time::Duration::from_secs(3), f.mt.inbox.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        coordinator::handle(&f.taker, &f.jt, &f.mt, &e)
            .await
            .is_err(),
        "SAFETY: incoming fee cap enforced before accepting quote"
    );
    assert_eq!(f.state(false, &id).await.as_deref(), Some("requested"));
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn r5_publication_exhaustion_not_unresolved_money_and_scan_pruned() {
    use maxplayer_trade::market::Publication;
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.pump(&id, "complete").await;
    let mut l =
        f.jm.get::<coordinator::Listing>("listing", &lot)
            .await
            .unwrap()
            .unwrap();
    l.published -= 1;
    f.jm.put("listing", &lot, &l).await.unwrap();
    let event_id = l.statuses.last().unwrap().id.to_hex();
    let mut row =
        f.jm.get::<Publication>("publication", &event_id)
            .await
            .unwrap()
            .unwrap();
    for d in row.relays.values_mut() {
        d.ack = false;
        d.attempts = 12;
    }
    f.jm.put("publication", &event_id, &row).await.unwrap();
    coordinator::recover_until_settled(&f.maker, &f.jm, &mut f.mm)
        .await
        .expect("SAFETY: exhausted status publication is not unresolved money");
    assert!(
        f.jm.all::<String>("publication_pending")
            .await
            .unwrap()
            .is_empty(),
        "SAFETY: completed/exhausted records leave recurring scan"
    );
    assert!(
        f.jm.get::<Publication>("publication", &event_id)
            .await
            .unwrap()
            .is_some(),
        "receipts retained"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_inbox_buffers_more_than_256_messages_across_peers() {
    let relay = LocalRelay::new(RelayBuilder::default().rate_limit(RateLimit {
        notes_per_minute: 1000,
        ..Default::default()
    }));
    relay.run().await.unwrap();
    let keys = Keys::generate();
    let mut m = Market::connect(keys.clone(), &[relay.url().await.to_string()])
        .await
        .unwrap();
    for _ in 0..5 {
        let peer = Keys::generate();
        for _ in 0..60 {
            let e = EventBuilder::new(
                Kind::Custom(maxplayer_trade::TRADE),
                uuid::Uuid::new_v4().to_string(),
            )
            .tag(Tag::public_key(keys.public_key()))
            .sign_with_keys(&peer)
            .unwrap();
            m.publish(&e).await.unwrap();
        }
    }
    let mut n = 0;
    while tokio::time::timeout(std::time::Duration::from_secs(2), m.inbox.recv())
        .await
        .is_ok_and(|e| e.is_some())
    {
        n += 1;
        if n == 300 {
            break;
        }
    }
    assert_eq!(
        n, 300,
        "SAFETY: slow recovery cannot silently drop legitimate peers behind 256 inbox slots"
    );
}
#[tokio::test]
async fn r5_withdraw_preflight_requires_nut05() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    m.faults.missing_nut.store(5, SeqCst);
    let error = money::withdraw(h.path(), &j, &m.url, &invoice(31))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("NUT-5"));
    assert!(m.faults.melt_requests.lock().unwrap().is_empty());
    assert!(!money::pending(&j).await.unwrap());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r5_cli_nonfinal_withdraw_is_exit_three() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    fund(h.path(), &m.url, 128).await;
    m.faults.melt_proxy_error.store(true, SeqCst);
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "withdraw",
            &m.url,
            "--invoice",
            &invoice(31),
            "--max-debit",
            "62",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "SAFETY: ambiguous submitted payment must not exit success"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["state"],
        "request_sent"
    );
}
#[tokio::test]
async fn r5_cli_terminal_quarantine_is_exit_four() {
    let h = tempfile::tempdir().unwrap();
    let j = Journal::open(h.path()).await.unwrap();
    // A normal fixture gives a valid swap row; quarantine requires no network to report.
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    s.state = "claim_quarantined".into();
    j.put("swap", &id, &s).await.unwrap();
    let error = coordinator::recovery_status(&j, false).await.unwrap_err();
    assert!(
        error.is::<coordinator::ManualRecovery>(),
        "SAFETY: quarantine not reported successful"
    );
}
// ---------------------------------------------------------------------------
// Round 6 (re-review of b3372b1): N1, N2, L1, L2.
// ---------------------------------------------------------------------------
/// Test-only malicious client: lock `amount` on `mint` under `c` with an explicit,
/// NON-canonical split. Models a dishonest counterparty; production code has no switch.
async fn noncanonical_lock(
    home: &std::path::Path,
    mint_url: &str,
    amount: u64,
    split: &[u64],
    c: &cashu::nuts::SpendingConditions,
) -> cashu::nuts::Proofs {
    use cashu::nuts::{PreMintSecrets, SwapRequest, SwapResponse};
    use cdk::amount::SplitTarget;
    fund(home, mint_url, 64).await;
    let w = wallet::wallet(home, mint_url).await.unwrap();
    let inputs = w.get_unspent_proofs().await.unwrap();
    let keyset = w.fetch_active_keyset().await.unwrap().id;
    let f = w.get_keyset_fees_and_amounts_by_id(keyset).await.unwrap();
    let fee = u64::from(w.get_proofs_fee(&inputs).await.unwrap().total);
    let total: u64 = inputs.iter().map(|p| u64::from(p.amount)).sum();
    let locked = PreMintSecrets::with_conditions(
        keyset,
        amount.into(),
        &SplitTarget::Values(split.iter().map(|v| cdk::Amount::from(*v)).collect()),
        c,
        &f,
    )
    .unwrap();
    let change = PreMintSecrets::random(
        keyset,
        (total - fee - amount).into(),
        &SplitTarget::default(),
        &f,
    )
    .unwrap();
    let mut outputs = locked.blinded_messages();
    outputs.extend(change.blinded_messages());
    let r: SwapResponse = mint::rpc(mint_url, "swap", &SwapRequest::new(inputs, outputs))
        .await
        .unwrap();
    let keys = w.load_keyset_keys(keyset).await.unwrap();
    cashu::dhke::construct_proofs(
        r.signatures[..locked.len()].to_vec(),
        locked.rs(),
        locked.secrets(),
        &keys,
    )
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r6_noncanonical_sender_split_rejected_before_maker_locks() {
    use nostr_sdk::prelude::*;
    // Maker gives A (0 ppk) 32, wants B (100 ppk) 24: quoted gross 25, claim_fee 1.
    let mut f = asymmetric_fees(0, 100).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await; // maker: request -> quote
    f.step(false).await; // taker: quote -> accepted -> honest first lock sent
    let honest = tokio::time::timeout(std::time::Duration::from_secs(10), f.mm.inbox.recv())
        .await
        .unwrap()
        .unwrap();
    let mut env = f.mm.decode(&honest).unwrap();
    assert_eq!(env.step, "first");
    let q =
        f.jt.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .quote
            .unwrap();
    assert_eq!(
        (q.request.funding.gross, q.request.funding.claim_fee),
        (25, 1)
    );
    let c = mint::conditions(&q.request.hash, &q.maker_key, &q.request.taker_key, q.long).unwrap();
    // 26 sats in 11 proofs: fee ceil(11*100/1000) = 2, net 24 still "correct".
    let evil = noncanonical_lock(
        &f.root.path().join("attacker"),
        &f.b.url,
        26,
        &[8, 4, 4, 2, 2, 1, 1, 1, 1, 1, 1],
        &c,
    )
    .await;
    assert_eq!(evil.len(), 11);
    env.request_id = uuid::Uuid::new_v4().to_string();
    env.body = serde_json::to_value(&evil).unwrap();
    let maker = f.mm.keys.public_key();
    let content = nip44::encrypt(
        f.mt.keys.secret_key(),
        &maker,
        serde_json::to_string(&env).unwrap(),
        nip44::Version::V2,
    )
    .unwrap();
    let forged = EventBuilder::new(Kind::Custom(maxplayer_trade::TRADE), content)
        .tags([
            Tag::public_key(maker),
            Tag::hashtag("maxplayer"),
            Tag::parse(["v", "1"]).unwrap(),
        ])
        .sign_with_keys(&f.mt.keys)
        .unwrap();
    let result = coordinator::handle(&f.maker, &f.jm, &f.mm, &forged).await;
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("quoted"),
        "SAFETY: lock rejected at validation (maker must not accept a non-canonical split)"
    );
    assert!(
        f.jm.get::<serde_json::Value>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .is_none(),
        "SAFETY: lock rejected at validation (maker never locks its own leg)"
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("non-canonical lock split"), "{error}");
}
#[cfg(feature = "lab")]
async fn own_mint_outage_still_claims(blackhole: bool) {
    let (f, id) = partial_claim_fixture().await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.as_ref().unwrap().short + s.quote.as_ref().unwrap().margin).await;
    // Own mint A: NUT-07 down. Counterparty B healthy. Preimage known, state claiming.
    if blackhole {
        f.a.faults.blackhole_checkstate.store(true, SeqCst);
    } else {
        f.a.faults.reject_checkstate.store(true, SeqCst);
    }
    let result = coordinator::advance(&f.maker, &f.jm, &f.mm, &mut s).await;
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        24,
        "SAFETY: own-mint NUT-07 outage must not skip the live maker claim"
    );
    assert_eq!(s.state, "settling");
    // Own-mint settlement bookkeeping may still report the outage; the claim landed.
    let _ = result;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r6_own_mint_checkstate_503_still_claims() {
    own_mint_outage_still_claims(false).await;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r6_own_mint_checkstate_blackhole_still_claims() {
    own_mint_outage_still_claims(true).await;
}
#[tokio::test]
async fn r6_expired_quote_created_is_refused_and_same_invoice_retries() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    let j = Journal::open(h.path()).await.unwrap();
    let inv = invoice(31);
    assert!(money::withdraw(h.path(), &j, &m.url, &inv).await.is_err());
    let a = j
        .all::<Withdrawal>("withdrawal")
        .await
        .unwrap()
        .pop()
        .unwrap();
    // Crash-retained, never-submitted QuoteCreated whose quote has expired.
    let mut v = serde_json::to_value(&a).unwrap();
    v["state"] = "quote_created".into();
    if v["quote"].is_object() {
        v["quote"]["expiry"] = 1.into();
    }
    j.put("withdrawal", &a.id, &v).await.unwrap();
    fund(h.path(), &m.url, 128).await;
    let first = money::withdraw(h.path(), &j, &m.url, &inv).await;
    let saved: Withdrawal = j.get("withdrawal", &a.id).await.unwrap().unwrap();
    assert_eq!(
        saved.state,
        MeltState::Refused,
        "SAFETY: expired unsent quote is refused (retryable), not unpaid_released"
    );
    assert!(
        m.faults.melt_requests.lock().unwrap().is_empty(),
        "SAFETY: expired quote never submitted"
    );
    assert_eq!(first.unwrap().state, MeltState::Refused);
    assert_eq!(
        money::withdraw(h.path(), &j, &m.url, &inv)
            .await
            .unwrap()
            .state,
        MeltState::Done,
        "same invoice retries with a fresh quote"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r6_cli_mint_dropped_after_post_is_exit_three() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    fund(h.path(), &m.url, 128).await;
    m.faults.die_after_melt.store(true, SeqCst);
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            h.path().to_str().unwrap(),
            "withdraw",
            &m.url,
            "--invoice",
            &invoice(31),
            "--max-debit",
            "62",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        m.faults.melt_requests.lock().unwrap().len(),
        1,
        "fixture: the POST reached the mint before it dropped"
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "SAFETY: submitted-but-unresolved withdrawal must exit 3, never 1 (refusal); stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
