mod support;
use maxplayer_trade::{
    Status,
    coordinator::{self, Listing, Swap},
    lifecycle, mint,
};
use support::*;
async fn swap_roundtrip(ppk: u64) {
    let mut f = Fixture::new(ppk).await;
    let lot = f.list(false).await;
    assert_eq!(f.mt.discover(None).await.unwrap().len(), 1);
    let id = f.start(&lot).await;
    f.pump(&id, "complete").await;
    let maker = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let taker = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert_eq!(maker.state, "complete");
    assert_eq!(balance(&f.maker, &f.a.url).await, 128 - maker.plan.debit);
    assert_eq!(balance(&f.maker, &f.b.url).await, 24);
    assert_eq!(balance(&f.taker, &f.a.url).await, 32);
    assert_eq!(balance(&f.taker, &f.b.url).await, 128 - taker.plan.debit);
    let l = f.jm.get::<Listing>("listing", &lot).await.unwrap().unwrap();
    assert_eq!(lifecycle(&l.event, &l.statuses).unwrap(), Status::Sold);
    assert!(f.mt.discover(Some(l.event.id)).await.unwrap().is_empty());
    // Reverse direction with acquired assets; top up enough to cover the fixed reverse lot and fees.
    fund(&f.maker, &f.b.url, 32).await;
    fund(&f.taker, &f.a.url, 32).await;
    let before = [
        balance(&f.maker, &f.a.url).await,
        balance(&f.maker, &f.b.url).await,
        balance(&f.taker, &f.a.url).await,
        balance(&f.taker, &f.b.url).await,
    ];
    // A completed trade uses three public events and seven encrypted envelopes:
    // request, quote, first, second, claimed, and one done from each role.
    let mut published =
        f.jm.all::<maxplayer_trade::market::Publication>("publication")
            .await
            .unwrap();
    published.extend(
        f.jt.all::<maxplayer_trade::market::Publication>("publication")
            .await
            .unwrap(),
    );
    assert_eq!(published.len(), 10, "one trade's unique publication volume");
    assert!(
        published
            .iter()
            .all(|p| p.relays.values().all(|d| d.attempts == 1)),
        "healthy trade must publish each event once per relay"
    );
    let reverse = f.list(true).await;
    let rid = f.start(&reverse).await;
    f.pump(&rid, "complete").await;
    let maker = f.jm.get::<Swap>("swap", &rid).await.unwrap().unwrap();
    let taker = f.jt.get::<Swap>("swap", &rid).await.unwrap().unwrap();
    assert_eq!(balance(&f.maker, &f.a.url).await, before[0] + 24);
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        before[1] - maker.plan.debit
    );
    assert_eq!(
        balance(&f.taker, &f.a.url).await,
        before[2] - taker.plan.debit
    );
    assert_eq!(balance(&f.taker, &f.b.url).await, before[3] + 32);
    if ppk > 0 {
        assert!(
            maker.plan.claim_fee > 0
                && maker.plan.lock_fee > 0
                && taker.plan.claim_fee > 0
                && taker.plan.lock_fee > 0
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_mints_same_unit_trade_and_sell_back() {
    swap_roundtrip(0).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fee_bearing_trade_and_sell_back_exact_balances() {
    swap_roundtrip(100).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlisting_and_cancelled_lot_rejected() {
    let f = Fixture::new(0).await;
    let lot = f.list(false).await;
    assert!(mint::plan(&f.maker, &f.a.url, 100, 16).await.is_err());
    coordinator::cancel(&f.maker, &f.jm, &f.mm, &lot)
        .await
        .unwrap();
    assert!(
        coordinator::start_take(&f.taker, &f.jt, &f.mt, &lot, 40, 32, 16)
            .await
            .is_err()
    );
    assert_eq!(balance(&f.maker, &f.a.url).await, 128);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maker_offline_at_claim_recovers_from_mint_witness() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    // Request -> quote -> first -> second -> claim. Do not deliver claim notice to maker.
    for _ in 0..30 {
        f.step(true).await;
        f.step(false).await;
        if f.state(false, &id).await.as_deref() == Some("claimed") {
            break;
        }
    }
    assert_eq!(f.state(false, &id).await.as_deref(), Some("claimed"));
    assert_eq!(f.state(true, &id).await.as_deref(), Some("second_locked"));
    let restarted = maxplayer_trade::journal::Journal::open(&f.maker)
        .await
        .unwrap();
    coordinator::recover(&f.maker, &restarted, &f.mm)
        .await
        .unwrap();
    assert_eq!(f.state(true, &id).await.as_deref(), Some("complete"));
    assert_eq!(balance(&f.maker, &f.b.url).await, 24);
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maker_never_locks_taker_refunds() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    assert_eq!(f.state(false, &id).await.as_deref(), Some("first_locked"));
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.unwrap().long + 1).await;
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(f.state(false, &id).await.as_deref(), Some("refunded"));
    assert_eq!(balance(&f.taker, &f.b.url).await, 128);
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taker_never_claims_maker_refunds_promptly() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    f.step(true).await;
    assert_eq!(f.state(true, &id).await.as_deref(), Some("second_locked"));
    let s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.unwrap().short + 1).await;
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(f.state(true, &id).await.as_deref(), Some("refunded"));
    assert_eq!(balance(&f.maker, &f.a.url).await, 128);
}
#[cfg(feature = "lab")]
async fn wait_past(deadline: u64) {
    while coordinator::now() <= deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_process_after_mint_swap_resumes_exact_journal() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            f.taker.to_str().unwrap(),
            "--relay",
            &f.relay_url,
            "take",
            &lot,
            "--max-give",
            "40",
            "--min-receive",
            "32",
        ])
        .env("TRADE_CRASH_AFTER_SWAP", "lock")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    let status = loop {
        f.step(true).await;
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            start.elapsed().as_secs() < 30,
            "crash injection not reached"
        );
    };
    assert_eq!(status.code(), Some(86));
    let swaps = f.jt.all::<Swap>("swap").await.unwrap();
    assert_eq!(swaps.len(), 1);
    let id = swaps[0].id.clone();
    assert_eq!(swaps[0].state, "accepted");
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    f.pump(&id, "complete").await;
    assert_eq!(balance(&f.taker, &f.b.url).await, 104);
    assert_eq!(balance(&f.taker, &f.a.url).await, 32);
    assert_eq!(balance(&f.maker, &f.a.url).await, 96);
    assert_eq!(balance(&f.maker, &f.b.url).await, 24);
    // Reopening and recovering again cannot add the spent funding proof or repeat either claim.
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(balance(&f.taker, &f.b.url).await, 104);
}
#[cfg(feature = "lab")]
async fn refund_crash_case(before: bool) {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    f.step(true).await;
    let maker = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let taker = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let quote = maker.quote.clone().unwrap();
    assert_eq!(maker.state, "second_locked");
    wait_past(quote.short + quote.margin).await;
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
        .args([
            "--home",
            f.maker.to_str().unwrap(),
            "--relay",
            &f.relay_url,
            "recover",
        ])
        .env(
            if before {
                "TRADE_CRASH_BEFORE_SWAP"
            } else {
                "TRADE_CRASH_AFTER_SWAP"
            },
            format!("{id}-refund"),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .unwrap();
    assert_eq!(status.code(), Some(if before { 87 } else { 86 }));
    if before {
        // NUT-14 permits this late receiver claim. It wins against the journaled but unsent refund.
        mint::redeem(
            &f.taker,
            &f.jt,
            "late-claim",
            &f.a.url,
            &maker.outgoing,
            &taker.key,
            taker.preimage.as_ref().unwrap(),
            16,
            None,
        )
        .await
        .unwrap();
    }
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    if before {
        assert_eq!(f.state(true, &id).await.as_deref(), Some("complete"));
        assert_eq!(balance(&f.maker, &f.b.url).await, 24);
        assert_eq!(balance(&f.taker, &f.a.url).await, 32);
    } else {
        assert_eq!(f.state(true, &id).await.as_deref(), Some("refunded"));
        assert_eq!(balance(&f.maker, &f.a.url).await, 128);
    }
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refund_outputs_survive_process_exit() {
    refund_crash_case(false).await;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_claim_wins_refund_race_maker_recovers_preimage() {
    refund_crash_case(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_lock_validation_rejects_wrong_hash_key_deadline_dleq_and_net() {
    use maxplayer_trade::journal::Journal;
    let f = Fixture::new(100).await;
    let j = Journal::open(&f.taker).await.unwrap();
    let p = mint::plan(&f.taker, &f.b.url, 24, 16).await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    mint::reserve(&f.taker, &p, &id).await.unwrap();
    let recv = cashu::nuts::SecretKey::generate();
    let refund = cashu::nuts::SecretKey::generate();
    let hash = "ab".repeat(32);
    let end = coordinator::now() + 3600;
    let c = mint::conditions(
        &hash,
        &recv.public_key().to_string(),
        &refund.public_key().to_string(),
        end,
    )
    .unwrap();
    let proofs = mint::lock(&f.taker, &j, &id, &p, &c, coordinator::now() + 60)
        .await
        .unwrap();
    mint::validate(
        &f.maker,
        &f.b.url,
        &proofs,
        24,
        p.gross,
        p.claim_fee,
        &c,
        p.ppk,
        p.keyset,
    )
    .await
    .unwrap();
    for wrong in [
        mint::conditions(
            &"ac".repeat(32),
            &recv.public_key().to_string(),
            &refund.public_key().to_string(),
            end,
        )
        .unwrap(),
        mint::conditions(
            &hash,
            &refund.public_key().to_string(),
            &recv.public_key().to_string(),
            end,
        )
        .unwrap(),
        mint::conditions(
            &hash,
            &recv.public_key().to_string(),
            &refund.public_key().to_string(),
            end + 1,
        )
        .unwrap(),
    ] {
        assert!(
            mint::validate(
                &f.maker,
                &f.b.url,
                &proofs,
                24,
                p.gross,
                p.claim_fee,
                &wrong,
                p.ppk,
                p.keyset
            )
            .await
            .is_err()
        );
    }
    let mut missing = proofs.clone();
    missing[0].dleq = None;
    assert!(
        mint::validate(
            &f.maker,
            &f.b.url,
            &missing,
            24,
            p.gross,
            p.claim_fee,
            &c,
            p.ppk,
            p.keyset
        )
        .await
        .is_err()
    );
    let mut forged = proofs.clone();
    forged[0].c = cashu::nuts::SecretKey::generate().public_key();
    assert!(
        mint::validate(
            &f.maker,
            &f.b.url,
            &forged,
            24,
            p.gross,
            p.claim_fee,
            &c,
            p.ppk,
            p.keyset
        )
        .await
        .is_err()
    );
    assert!(
        mint::validate(
            &f.maker,
            &f.b.url,
            &proofs,
            25,
            p.gross,
            p.claim_fee,
            &c,
            p.ppk,
            p.keyset
        )
        .await
        .is_err()
    );
    let mut duplicate = proofs.clone();
    duplicate.push(proofs[0].clone());
    assert!(
        mint::validate(
            &f.maker,
            &f.b.url,
            &duplicate,
            24,
            p.gross,
            p.claim_fee,
            &c,
            p.ppk,
            p.keyset
        )
        .await
        .is_err()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_quote_recovers_interrupted_active_index() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    s.state = "expired".into();
    f.jm.put("swap", &id, &s).await.unwrap();
    // Simulate exit after terminal swap commit but before clearing the lot's private active index.
    assert_eq!(
        f.jm.get::<Listing>("listing", &lot)
            .await
            .unwrap()
            .unwrap()
            .active,
        Some(id)
    );
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    let l = f.jm.get::<Listing>("listing", &lot).await.unwrap().unwrap();
    assert!(l.active.is_none());
    assert_eq!(lifecycle(&l.event, &l.statuses).unwrap(), Status::Available);
    // Inventory is still backed; expiry only releases the soft quote hold.
    assert!(mint::plan(&f.maker, &f.a.url, 100, 16).await.is_err());
}
