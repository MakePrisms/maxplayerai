mod support;
use maxplayer_trade::{coordinator, mint};
use sha2::{Digest, Sha256};
use support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c1_sender_witness_rejected_and_sanitized_both_directions() {
    let f = Fixture::new(0).await;
    for (sender, receiver, url, journal) in [
        (&f.taker, &f.maker, &f.b.url, &f.jt),
        (&f.maker, &f.taker, &f.a.url, &f.jm),
    ] {
        let p = mint::plan(sender, url, 24, 16).await.unwrap();
        let key = cashu::nuts::SecretKey::generate();
        let refund = cashu::nuts::SecretKey::generate();
        let pre = "ab".repeat(32);
        let hash = hex::encode(Sha256::digest(hex::decode(&pre).unwrap()));
        let c = mint::conditions(
            &hash,
            &key.public_key().to_string(),
            &refund.public_key().to_string(),
            coordinator::now() + 3600,
        )
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut proofs = mint::lock(sender, journal, &id, &p, &c, coordinator::now() + 60)
            .await
            .unwrap();
        for proof in &mut proofs {
            proof.witness = Some(cashu::nuts::Witness::HTLCWitness(
                cashu::nuts::nut14::HTLCWitness {
                    preimage: String::new(),
                    signatures: Some(vec!["x".into()]),
                },
            ));
        }
        assert!(
            mint::validate(receiver, url, &proofs, 24, &c, p.ppk, p.keyset)
                .await
                .is_err(),
            "C1 admission must reject sender witness"
        );
        mint::redeem(
            receiver,
            journal,
            &format!("{id}-claim"),
            url,
            &proofs,
            &hex::encode(key.to_secret_bytes()),
            &pre,
            16,
            None,
        )
        .await
        .expect("C1 defense in depth must clear hostile signatures");
        assert_eq!(balance(receiver, url).await, 24);
    }
}

use maxplayer_trade::coordinator::{Listing, Swap};
use std::sync::atomic::Ordering::SeqCst;
async fn through_second(f: &mut Fixture) -> String {
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    f.step(true).await;
    assert_eq!(f.state(true, &id).await.as_deref(), Some("second_locked"));
    id
}
#[cfg(feature = "lab")]
async fn wait_past(t: u64) {
    while coordinator::now() <= t {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c2_partial_claim_recovers_preimage_and_refunds_only_unspent() {
    let mut f = Fixture::new(0).await;
    let lot = coordinator::list(
        &f.maker,
        &f.jm,
        &f.mm,
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.a.url).unwrap(),
            net: 24,
        },
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.b.url).unwrap(),
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
    let mut maker = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let taker = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert!(
        maker.outgoing.len() > 1,
        "partial claim fixture needs multiple proofs"
    );
    let chosen = vec![
        maker
            .outgoing
            .iter()
            .max_by_key(|p| p.amount)
            .unwrap()
            .clone(),
    ];
    mint::redeem(
        &f.taker,
        &f.jt,
        "partial",
        &f.a.url,
        &chosen,
        &taker.key,
        taker.preimage.as_ref().unwrap(),
        16,
        None,
    )
    .await
    .unwrap();
    coordinator::advance(&f.maker, &f.jm, &f.mm, &mut maker)
        .await
        .unwrap();
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        24,
        "C2 maker must immediately claim payment"
    );
    assert_eq!(maker.state, "settling");
    wait_past(maker.quote.as_ref().unwrap().short + 1).await;
    coordinator::advance(&f.maker, &f.jm, &f.mm, &mut maker)
        .await
        .unwrap();
    assert_eq!(
        maker.state, "complete",
        "C2 partial-spend remainder must settle"
    );
    assert_eq!(
        balance(&f.maker, &f.a.url).await + balance(&f.taker, &f.a.url).await,
        128
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h1_maker_offline_past_long_still_claims() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.step(false).await;
    assert_eq!(f.state(false, &id).await.as_deref(), Some("claimed"));
    let s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.unwrap().long).await;
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        24,
        "H1 late maker claim must remain possible"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m1_notice_recovers_without_nut07_witness() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.a.faults.hide_witness.store(true, SeqCst);
    f.step(false).await;
    f.step(true).await;
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("complete"),
        "M1 notice must settle without witness lookup"
    );
    assert_eq!(balance(&f.maker, &f.b.url).await, 24);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m2_l3_preflight_requires_restore_and_dleq() {
    let f = Fixture::new(0).await;
    for nut in [9, 12] {
        f.a.faults.missing_nut.store(nut, SeqCst);
        assert!(
            maxplayer_trade::wallet::preflight(&f.a.url).await.is_err(),
            "missing NUT must refuse preflight"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l2_own_fee_mismatch_not_journalled() {
    let f = Fixture::new(100).await;
    let mut p = mint::plan(&f.taker, &f.b.url, 24, 16).await.unwrap();
    p.claim_fee += 1;
    let key = cashu::nuts::SecretKey::generate().public_key().to_string();
    let c = mint::conditions(&"ab".repeat(32), &key, &key, coordinator::now() + 3600).unwrap();
    assert!(
        mint::lock(&f.taker, &f.jt, "fee", &p, &c, coordinator::now() + 60)
            .await
            .is_err()
    );
    assert!(
        f.jt.get::<mint::Attempt>("attempt", "fee")
            .await
            .unwrap()
            .is_none(),
        "L2 must refuse before journal"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l4_active_quote_cannot_cancel() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let _ = f.start(&lot).await;
    f.step(true).await;
    assert!(
        coordinator::cancel(&f.maker, &f.jm, &f.mm, &lot)
            .await
            .is_err(),
        "L4 active quote cannot publish cancelled"
    );
    assert!(
        !f.jm
            .get::<Listing>("listing", &lot)
            .await
            .unwrap()
            .unwrap()
            .cancelled
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_failed_claim_abandoned_then_taker_refunds() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.a.faults.reject_swap.store(true, SeqCst);
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = f.mt.inbox.recv().await.unwrap();
            let msg = f.mt.decode(&event).unwrap();
            if msg.swap_id == id && msg.step == "second" {
                break event;
            }
        }
    })
    .await
    .unwrap();
    // The injected claim failure is expected even with the old early-return bug.
    // Continue to assert the durable recovery consequence, not the helper's return value.
    let _ = coordinator::handle(&f.taker, &f.jt, &f.mt, &event).await;
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("second_validated")
    );
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let q = s.quote.unwrap();
    wait_past((q.short - q.cutoff + mint::ABANDON_GRACE_SECONDS).max(q.long + q.margin)).await;
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-claim"))
            .await
            .unwrap()
            .unwrap()
            .abandoned,
        "H2 claim must have affirmative abandonment evidence"
    );
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("refunded"),
        "H2 claim error must not block refund"
    );
    assert_eq!(balance(&f.taker, &f.b.url).await, 128);
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_unsent_lock_expires_and_releases_reservation() {
    use cdk::cdk_database::WalletDatabase;
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.b.faults.reject_swap.store(true, SeqCst);
    f.step(false).await;
    assert_eq!(f.state(false, &id).await.as_deref(), Some("accepted"));
    f.b.faults
        .clock_offset
        .store(60 + mint::ABANDON_GRACE_SECONDS, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("expired"),
        "H2 abandoned lock must expire"
    );
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .unwrap()
            .abandoned
    );
    let db = maxplayer_trade::wallet::database(&f.taker, &f.b.url)
        .await
        .unwrap();
    assert!(
        db.get_reserved_proofs(&id.parse().unwrap())
            .await
            .unwrap()
            .is_empty(),
        "H2 lock reservation must release"
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_landed_claim_lost_reply_never_refunds() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.a.faults.lose_reply.store(true, SeqCst);
    f.step(false).await;
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("second_validated")
    );
    f.a.faults.reject_restore.store(true, SeqCst);
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.unwrap().long + 1).await;
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-refund"))
            .await
            .unwrap()
            .is_none(),
        "H2 ambiguous landed claim must never authorize refund"
    );
    assert_eq!(
        balance(&f.taker, &f.b.url).await,
        104,
        "H2 no cheating refund"
    );
    f.a.faults.reject_restore.store(false, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(
        balance(&f.taker, &f.a.url).await,
        32,
        "restore must credit original claim"
    );
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("complete_unclaimed")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h3_uppercase_hash_request_rejected_at_admission() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), f.mm.inbox.recv())
        .await
        .unwrap()
        .unwrap();
    let mut msg = f.mm.decode(&event).unwrap();
    msg.request_id = uuid::Uuid::new_v4().to_string();
    msg.body["hash"] = msg.body["hash"].as_str().unwrap().to_uppercase().into();
    // A malicious peer signs a fresh event, bypassing the honest sender's immutable step outbox.
    let attack_home = f.root.path().join("attacker-outbox");
    std::fs::create_dir(&attack_home).unwrap();
    let attack_journal = maxplayer_trade::journal::Journal::open(&attack_home)
        .await
        .unwrap();
    f.mt.send(&attack_journal, f.mm.keys.public_key(), &msg)
        .await
        .unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = f.mm.inbox.recv().await.unwrap();
            if f.mm.decode(&event).unwrap().request_id == msg.request_id {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        coordinator::handle(&f.maker, &f.jm, &f.mm, &event)
            .await
            .is_err(),
        "H3 uppercase hash must be refused"
    );
    assert!(
        f.jm.get::<Swap>("swap", &id).await.unwrap().is_none(),
        "H3 rejected request must not acquire lot"
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_claimed_terminal_without_refund() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.step(false).await;
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.unwrap().long + 1).await;
    f.b.faults.reject_info.store(true, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("complete_unclaimed")
    );
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-refund"))
            .await
            .unwrap()
            .is_none()
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        coordinator::recover_until_settled(&f.taker, &f.jt, &mut f.mt),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(balance(&f.taker, &f.b.url).await, 104);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l1_taker_lock_deadline_leaves_twenty_seconds_for_delivery() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let a: serde_json::Value =
        f.jt.get("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .unwrap();
    let q = s.quote.unwrap();
    assert_eq!(
        a["send_before"].as_u64(),
        Some((q.exp - 20).min(q.short - q.cutoff))
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l5_mint_clock_controls_claim_cutoff() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.a.faults.clock_offset.store(3600, SeqCst);
    f.step(false).await;
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-claim"))
            .await
            .unwrap()
            .is_none(),
        "L5 fast mint clock must prevent starting an unsafe claim"
    );
    assert_eq!(balance(&f.taker, &f.a.url).await, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m2_missing_dleq_persists_result_and_credits_change() {
    let f = Fixture::new(0).await;
    let p = mint::plan(&f.taker, &f.b.url, 24, 16).await.unwrap();
    let key = cashu::nuts::SecretKey::generate().public_key().to_string();
    let c = mint::conditions(&"ab".repeat(32), &key, &key, coordinator::now() + 3600).unwrap();
    f.b.faults.omit_dleq.store(true, SeqCst);
    assert!(
        mint::lock(
            &f.taker,
            &f.jt,
            "missing-dleq",
            &p,
            &c,
            coordinator::now() + 60
        )
        .await
        .is_err(),
        "M2 missing DLEQ must not be forwarded"
    );
    let a: serde_json::Value = f.jt.get("attempt", "missing-dleq").await.unwrap().unwrap();
    assert!(
        a["result"].is_array(),
        "M2 irreversible result must be durable"
    );
    assert_eq!(
        balance(&f.taker, &f.b.url).await,
        104,
        "M2 owned change must remain usable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m2_invalid_present_dleq_persisted_but_not_credited() {
    let f = Fixture::new(0).await;
    let p = mint::plan(&f.taker, &f.b.url, 24, 16).await.unwrap();
    let key = cashu::nuts::SecretKey::generate().public_key().to_string();
    let c = mint::conditions(&"ab".repeat(32), &key, &key, coordinator::now() + 3600).unwrap();
    f.b.faults.invalid_dleq.store(true, SeqCst);
    assert!(
        mint::lock(
            &f.taker,
            &f.jt,
            "invalid-dleq",
            &p,
            &c,
            coordinator::now() + 60
        )
        .await
        .is_err()
    );
    let a: serde_json::Value = f.jt.get("attempt", "invalid-dleq").await.unwrap().unwrap();
    assert!(
        a["result"].is_array(),
        "invalid evidence must remain journalled"
    );
    assert_eq!(a["done"], false);
    use cashu::nuts::nut00::ProofsMethods;
    use cdk::cdk_database::WalletDatabase;
    let result: cashu::nuts::Proofs = serde_json::from_value(a["result"].clone()).unwrap();
    let invalid_ys = result.ys().unwrap();
    let stored = maxplayer_trade::wallet::database(&f.taker, &f.b.url)
        .await
        .unwrap()
        .get_proofs(None, None, None, None)
        .await
        .unwrap();
    assert!(
        stored.iter().all(|p| !invalid_ys.contains(&p.y)),
        "M2 invalid DLEQ outputs must not enter owned balance"
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_pending_inputs_prevent_abandonment() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.b.faults.reject_swap.store(true, SeqCst);
    f.step(false).await;
    f.b.faults
        .clock_offset
        .store(60 + mint::ABANDON_GRACE_SECONDS, SeqCst);
    f.b.faults.pending_inputs.store(true, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert!(
        !f.jt
            .get::<mint::Attempt>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .unwrap()
            .abandoned,
        "H2 PENDING inputs must retain ambiguous attempt"
    );
    assert_eq!(f.state(false, &id).await.as_deref(), Some("accepted"));
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h2_maker_abandoned_lock_releases_listing() {
    use cdk::cdk_database::WalletDatabase;
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    f.a.faults.reject_swap.store(true, SeqCst);
    f.step(true).await;
    f.a.faults
        .clock_offset
        .store(60 + mint::ABANDON_GRACE_SECONDS, SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("expired"),
        "H2 maker abandoned lock must expire"
    );
    let l = f.jm.get::<Listing>("listing", &lot).await.unwrap().unwrap();
    assert!(l.cancelled && l.active.is_none());
    assert!(
        maxplayer_trade::wallet::database(&f.maker, &f.a.url)
            .await
            .unwrap()
            .get_reserved_proofs(&l.reservation.parse().unwrap())
            .await
            .unwrap()
            .is_empty()
    );
}

#[cfg(feature = "lab")]
async fn second_event(f: &mut Fixture, id: &str) -> nostr_sdk::Event {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let e = f.mt.inbox.recv().await.unwrap();
            let msg = f.mt.decode(&e).unwrap();
            if msg.swap_id == id && msg.step == "second" {
                break e;
            }
        }
    })
    .await
    .unwrap()
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n1_maker_refund_before_taker_observes_abandonment() {
    let mut f = Fixture::new(100).await;
    let id = through_second(&mut f).await;
    f.a.faults.reject_swap.store(true, SeqCst);
    let event = second_event(&mut f, &id).await;
    let _ = coordinator::handle(&f.taker, &f.jt, &f.mt, &event).await;
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let q = s.quote.unwrap();
    f.a.faults.reject_swap.store(false, SeqCst);
    wait_past(q.short + q.margin).await;
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(f.state(true, &id).await.as_deref(), Some("refunded"));
    wait_past(q.long + q.margin).await;
    // Only the evidence clock advances; real HTLC refund eligibility has already passed.
    f.a.faults
        .clock_offset
        .store(mint::ABANDON_GRACE_SECONDS, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("refunded"),
        "N1 maker refund must not strand taker"
    );
    assert_eq!(
        balance(&f.taker, &f.b.url).await,
        128 - s.plan.lock_fee - s.plan.claim_fee,
        "N1 refund restores exact balance net of fees"
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n2_late_claim_after_abandonment_restore_outage_never_refunds() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.a.faults.hold_swap.store(true, SeqCst);
    let event = second_event(&mut f, &id).await;
    let _ = coordinator::handle(&f.taker, &f.jt, &f.mt, &event).await; // actual HTTP timeout
    f.a.faults.swap_entered.notified().await;
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let q = s.quote.unwrap();
    f.a.faults.clock_offset.store(120, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-claim"))
            .await
            .unwrap()
            .unwrap()
            .abandoned,
        "fixture must reach persisted abandonment before landing"
    );
    f.a.faults.swap_release.notify_one();
    f.a.faults.swap_finished.notified().await;
    f.a.faults.reject_restore.store(true, SeqCst);
    wait_past(q.long + q.margin).await;
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-refund"))
            .await
            .unwrap()
            .is_none(),
        "N2 persisted abandonment must not authorize a refund during restore outage"
    );
    assert_eq!(
        balance(&f.taker, &f.b.url).await,
        104,
        "N2 no refund after a late landed claim"
    );
    f.a.faults.reject_restore.store(false, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(
        balance(&f.taker, &f.a.url).await,
        32,
        "N2 late claim credited on recovery"
    );
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("complete_unclaimed")
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n2_grace_covers_swap_timeout_and_deadline_follows_checkstate() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.b.faults.reject_swap.store(true, SeqCst);
    f.step(false).await;
    f.b.faults.reject_swap.store(false, SeqCst);
    let a: serde_json::Value =
        f.jt.get("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .unwrap();
    let deadline = a["send_before"].as_u64().unwrap();
    f.b.faults.hold_checkstate.store(true, SeqCst);
    let control = f.b.faults.clone();
    tokio::join!(coordinator::recover(&f.taker, &f.jt, &f.mt), async {
        control.checkstate_entered.notified().await;
        control
            .clock_offset
            .store(deadline + 1 - coordinator::now(), SeqCst);
        control.checkstate_release.notify_one();
    })
    .0
    .unwrap();
    let a: serde_json::Value =
        f.jt.get("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .unwrap();
    assert!(
        a["result"].is_null(),
        "N2 deadline must be checked after final NUT-07 RPC, immediately before POST"
    );
    f.b.faults
        .clock_offset
        .store(deadline + 30 - coordinator::now(), SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert!(
        !f.jt
            .get::<mint::Attempt>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .unwrap()
            .abandoned,
        "N2 grace must cover at least swap timeout plus margin (60s)"
    );
    assert!(
        mint::ABANDON_GRACE_SECONDS >= 60
            && mint::ABANDON_GRACE_SECONDS >= mint::RPC_TIMEOUT_SECONDS
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n2_late_lock_after_abandonment_is_restored_and_refunded() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.b.faults.hold_swap.store(true, SeqCst);
    f.step(false).await; // lock POST times out but server retains request
    f.b.faults.swap_entered.notified().await;
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    // Simulate the durable abandonment boundary, then make its mandatory recheck fail.
    // No coordinator refund/expiry decision may rely on this saved flag.
    let aid = format!("{id}-lock");
    let mut a: mint::Attempt = f.jt.get("attempt", &aid).await.unwrap().unwrap();
    a.abandoned = true;
    f.jt.put("attempt", &aid, &a).await.unwrap();
    f.b.faults.reject_restore.store(true, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_ne!(
        f.state(false, &id).await.as_deref(),
        Some("expired"),
        "N2 lock cannot expire without post-abandonment restore"
    );
    f.b.faults.swap_release.notify_one();
    f.b.faults.swap_finished.notified().await;
    f.b.faults.reject_restore.store(false, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    let restored = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert!(
        !restored.outgoing.is_empty(),
        "N2 late lock must become recoverable outgoing"
    );
    wait_past(s.quote.unwrap().long + 1).await;
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(f.state(false, &id).await.as_deref(), Some("refunded"));
    assert_eq!(balance(&f.taker, &f.b.url).await, 128);
}
#[cfg(feature = "lab")]
async fn unforwardable_case(maker: bool, invalid: bool) {
    use cdk::cdk_database::WalletDatabase;
    let mut f = Fixture::new(100).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    if maker {
        f.step(false).await;
    }
    let faults = if maker {
        f.a.faults.clone()
    } else {
        f.b.faults.clone()
    };
    if invalid {
        faults.invalid_dleq.store(true, SeqCst);
    } else {
        faults.omit_dleq.store(true, SeqCst);
    }
    // Failure is expected; exercise durable coordinator consequence, not its return value.
    let e = if maker {
        f.mm.inbox.recv().await.unwrap()
    } else {
        f.mt.inbox.recv().await.unwrap()
    };
    let (home, j, m, mint) = if maker {
        (&f.maker, &f.jm, &f.mm, &f.a.url)
    } else {
        (&f.taker, &f.jt, &f.mt, &f.b.url)
    };
    let _ = coordinator::handle(home, j, m, &e).await;
    let s = j.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert_eq!(
        s.state, "lock_unforwardable",
        "N3 explicit nonforwarding refund state"
    );
    assert!(
        !s.outgoing.is_empty(),
        "N3 locked value retained for refund"
    );
    faults.invalid_dleq.store(false, SeqCst);
    faults.omit_dleq.store(false, SeqCst);
    let q = s.quote.as_ref().unwrap();
    wait_past(if maker {
        q.short + q.margin
    } else {
        q.long + q.margin
    })
    .await;
    coordinator::recover(home, j, m).await.unwrap();
    assert_eq!(
        j.get::<Swap>("swap", &id).await.unwrap().unwrap().state,
        "refunded",
        "N3 unforwardable lock must finish refunded"
    );
    assert_eq!(
        balance(home, mint).await,
        128 - s.plan.lock_fee - s.plan.claim_fee,
        "N3 exact refund/change balance net of fees"
    );
    let reservation = if maker {
        f.jm.get::<Listing>("listing", &lot)
            .await
            .unwrap()
            .unwrap()
            .reservation
    } else {
        id
    };
    assert!(
        maxplayer_trade::wallet::database(home, mint)
            .await
            .unwrap()
            .get_reserved_proofs(&reservation.parse().unwrap())
            .await
            .unwrap()
            .is_empty(),
        "N3 no spent funding reservation leak"
    );
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n3_maker_missing_dleq_refunds() {
    unforwardable_case(true, false).await;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n3_maker_invalid_dleq_refunds() {
    unforwardable_case(true, true).await;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n3_taker_missing_dleq_refunds() {
    unforwardable_case(false, false).await;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n3_taker_invalid_dleq_refunds() {
    unforwardable_case(false, true).await;
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n4_partial_claim_wins_nut07_swap_race_reselects_refund() {
    let mut f = Fixture::new(0).await;
    let lot = coordinator::list(
        &f.maker,
        &f.jm,
        &f.mm,
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.a.url).unwrap(),
            net: 24,
        },
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.b.url).unwrap(),
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
    let mut maker = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let taker = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let chosen = vec![
        maker
            .outgoing
            .iter()
            .max_by_key(|p| p.amount)
            .unwrap()
            .clone(),
    ];
    assert_eq!(u64::from(chosen[0].amount), 16);
    wait_past(maker.quote.as_ref().unwrap().short + 1).await;
    f.a.faults.hold_swap.store(true, SeqCst);
    let (recovery, ()) = tokio::join!(
        coordinator::advance(&f.maker, &f.jm, &f.mm, &mut maker),
        async {
            f.a.faults.swap_entered.notified().await;
            mint::redeem(
                &f.taker,
                &f.jt,
                "n4-racing-claim",
                &f.a.url,
                &chosen,
                &taker.key,
                taker.preimage.as_ref().unwrap(),
                16,
                None,
            )
            .await
            .unwrap();
            f.a.faults.swap_release.notify_one();
        }
    );
    recovery.unwrap();
    // The initial witness scan is unavailable. The refund's own fresh-evidence check
    // must preserve the preimage it learns later and claim in THIS recovery step.
    f.a.faults.hide_witness_once.store(true, SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("complete"),
        "N4 rejected refund must not strand unspent remainder"
    );
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        24,
        "N4 learned witness must claim payment"
    );
    assert_eq!(
        balance(&f.maker, &f.a.url).await,
        112,
        "N4 exactly eight unspent units refunded"
    );
    assert_eq!(balance(&f.taker, &f.a.url).await, 16);
    assert_eq!(
        f.jm.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .refund_generation,
        1
    );
    assert!(
        f.jm.get::<mint::Attempt>("attempt", &format!("{id}-refund"))
            .await
            .unwrap()
            .is_some(),
        "N4 original exact outputs retained"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nit_notice_preimage_normalized_before_storage() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.step(false).await;
    let event = f.mm.inbox.recv().await.unwrap();
    let mut msg = f.mm.decode(&event).unwrap();
    assert_eq!(msg.step, "claimed");
    msg.request_id = uuid::Uuid::new_v4().to_string();
    let upper = msg.body["preimage"].as_str().unwrap().to_uppercase();
    msg.body["preimage"] = upper.clone().into();
    let dir = f.root.path().join("uppercase-notice");
    std::fs::create_dir(&dir).unwrap();
    let j = maxplayer_trade::journal::Journal::open(&dir).await.unwrap();
    f.mt.send(&j, f.mm.keys.public_key(), &msg).await.unwrap();
    let event = loop {
        let e = f.mm.inbox.recv().await.unwrap();
        if f.mm.decode(&e).unwrap().request_id == msg.request_id {
            break e;
        }
    };
    coordinator::handle(&f.maker, &f.jm, &f.mm, &event)
        .await
        .unwrap();
    let maker = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert_eq!(
        maker.preimage.as_deref(),
        Some(upper.to_lowercase().as_str()),
        "notice preimage must be canonical"
    );
    assert_eq!(maker.state, "complete");
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n1_restore_after_nut07_catches_landing_claim() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.a.faults.hold_swap.store(true, SeqCst);
    let event = second_event(&mut f, &id).await;
    let _ = coordinator::handle(&f.taker, &f.jt, &f.mt, &event).await;
    f.a.faults.swap_entered.notified().await;
    f.a.faults.clock_offset.store(120, SeqCst);
    f.a.faults.hold_checkstate_reply.store(true, SeqCst);
    tokio::join!(coordinator::recover(&f.taker, &f.jt, &f.mt), async {
        f.a.faults.checkstate_reply_entered.notified().await;
        f.a.faults.swap_release.notify_one();
        f.a.faults.swap_finished.notified().await;
        f.a.faults.checkstate_reply_release.notify_one();
    })
    .0
    .unwrap();
    assert!(
        !f.jt
            .get::<mint::Attempt>("attempt", &format!("{id}-claim"))
            .await
            .unwrap()
            .unwrap()
            .abandoned,
        "N1 restore after NUT-07 must catch signatures that landed after its UNSPENT snapshot"
    );
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(balance(&f.taker, &f.a.url).await, 32);
    assert_eq!(balance(&f.taker, &f.b.url).await, 104);
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n4_lost_refund_reply_reconciles_outputs_before_complete() {
    let mut f = Fixture::new(0).await;
    let lot = coordinator::list(
        &f.maker,
        &f.jm,
        &f.mm,
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.a.url).unwrap(),
            net: 24,
        },
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.b.url).unwrap(),
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
    let mut maker = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let taker = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let chosen = vec![
        maker
            .outgoing
            .iter()
            .max_by_key(|p| p.amount)
            .unwrap()
            .clone(),
    ];
    mint::redeem(
        &f.taker,
        &f.jt,
        "n4-partial",
        &f.a.url,
        &chosen,
        &taker.key,
        taker.preimage.as_ref().unwrap(),
        16,
        None,
    )
    .await
    .unwrap();
    coordinator::advance(&f.maker, &f.jm, &f.mm, &mut maker)
        .await
        .unwrap();
    assert_eq!(maker.state, "settling");
    wait_past(maker.quote.as_ref().unwrap().short + 1).await;
    f.a.faults.lose_reply.store(true, SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("settling"),
        "N4 SPENT refund inputs must not make uncredited outputs terminal"
    );
    assert_eq!(balance(&f.maker, &f.a.url).await, 104);
    f.a.faults.reject_restore.store(true, SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(f.state(true, &id).await.as_deref(), Some("settling"));
    f.a.faults.reject_restore.store(false, SeqCst);
    f.a.faults.lose_reply.store(false, SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(f.state(true, &id).await.as_deref(), Some("complete"));
    assert_eq!(
        balance(&f.maker, &f.a.url).await,
        112,
        "N4 exact lost-reply refund credited"
    );
    assert_eq!(balance(&f.maker, &f.b.url).await, 24);
    assert_eq!(balance(&f.taker, &f.a.url).await, 16);
}

// Appendix A, with the fault deliberately left on for lock restoration. Refund
// replies can independently verify or remain invalid. Both roles must progress.
#[cfg(feature = "lab")]
async fn persistent_invalid(maker: bool, bad_refund: bool) {
    let mut f = Fixture::new(100).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    if maker {
        f.step(false).await;
    }
    let faults = if maker {
        f.a.faults.clone()
    } else {
        f.b.faults.clone()
    };
    faults.invalid_dleq.store(true, SeqCst);
    let e = if maker {
        f.mm.inbox.recv().await.unwrap()
    } else {
        f.mt.inbox.recv().await.unwrap()
    };
    let (home, j, m, url) = if maker {
        (&f.maker, &f.jm, &f.mm, &f.a.url)
    } else {
        (&f.taker, &f.jt, &f.mt, &f.b.url)
    };
    let _ = coordinator::handle(home, j, m, &e).await;
    let s = j.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert_eq!(s.state, "lock_unforwardable");
    faults.invalid_swap_only_off.store(!bad_refund, SeqCst);
    let before = balance(home, url).await;
    let q = s.quote.unwrap();
    wait_past(if maker {
        q.short + q.margin
    } else {
        q.long + q.margin
    })
    .await;
    for _ in 0..3 {
        coordinator::recover(home, j, m).await.unwrap();
    }
    assert_eq!(
        j.get::<Swap>("swap", &id).await.unwrap().unwrap().state,
        if bad_refund {
            "refund_quarantined"
        } else {
            "refunded"
        },
        "SAFETY: persistent invalid DLEQ must not strand the lock"
    );
    let a = j
        .get::<serde_json::Value>("attempt", &format!("{id}-refund"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        a["result"].as_array().is_some_and(|p| !p.is_empty()),
        "exact refund outputs retained"
    );
    assert_eq!(a["quarantined"], bad_refund);
    if bad_refund {
        assert_eq!(
            balance(home, url).await,
            before,
            "unverified outputs never credited"
        );
        faults.reject_info.store(true, SeqCst);
        faults.reject_restore.store(true, SeqCst);
        faults.reject_swap.store(true, SeqCst);
        let error =
            coordinator::recover_until_settled(home, j, &mut if maker { f.mm } else { f.mt })
                .await
                .unwrap_err();
        assert!(error.is::<coordinator::ManualRecovery>());
        assert_eq!(
            j.get::<serde_json::Value>("attempt", &format!("{id}-refund"))
                .await
                .unwrap()
                .unwrap(),
            a,
            "terminal attempt never retried"
        );
    } else {
        assert_eq!(
            balance(home, url).await,
            before + s.plan.gross - s.plan.claim_fee
        );
    }
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_maker_persistent_invalid_dleq() {
    persistent_invalid(true, true).await;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_taker_persistent_invalid_dleq() {
    persistent_invalid(false, true).await;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_maker_invalid_lock_valid_refund() {
    persistent_invalid(true, false).await;
}
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_taker_invalid_lock_valid_refund() {
    persistent_invalid(false, false).await;
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_owned_change_spent_does_not_block_refund() {
    for maker in [false, true] {
        let mut f = Fixture::new(100).await;
        let lot = f.list(false).await;
        let id = f.start(&lot).await;
        f.step(true).await;
        if maker {
            f.step(false).await;
        }
        let faults = if maker {
            f.a.faults.clone()
        } else {
            f.b.faults.clone()
        };
        faults.omit_dleq.store(true, SeqCst);
        faults.reject_restore_after_swap.store(true, SeqCst);
        let e = if maker {
            f.mm.inbox.recv().await.unwrap()
        } else {
            f.mt.inbox.recv().await.unwrap()
        };
        let (home, j, m, url) = if maker {
            (&f.maker, &f.jm, &f.mm, &f.a.url)
        } else {
            (&f.taker, &f.jt, &f.mt, &f.b.url)
        };
        let _ = coordinator::handle(home, j, m, &e).await;
        let s = j.get::<Swap>("swap", &id).await.unwrap().unwrap();
        assert_eq!(s.state, "lock_unforwardable");
        faults.omit_dleq.store(false, SeqCst);
        faults.reject_restore.store(false, SeqCst);
        let a = j
            .get::<serde_json::Value>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .unwrap();
        let result: cashu::nuts::Proofs = serde_json::from_value(a["result"].clone()).unwrap();
        let owned: cashu::nuts::Proofs = result
            .into_iter()
            .zip(a["outputs"].as_array().unwrap())
            .filter(|(_, o)| o["owned"] == true)
            .map(|(p, _)| p)
            .collect();
        assert!(!owned.is_empty());
        maxplayer_trade::wallet::wallet(home, url)
            .await
            .unwrap()
            .swap(
                None,
                cdk::amount::SplitTarget::default(),
                owned,
                None,
                false,
                false,
            )
            .await
            .unwrap();
        assert!(
            mint::settle_unforwardable(home, j, &format!("{id}-lock"))
                .await
                .is_err(),
            "spent change reproduces settlement failure"
        );
        let before = balance(home, url).await;
        let q = s.quote.unwrap();
        wait_past(if maker {
            q.short + q.margin
        } else {
            q.long + q.margin
        })
        .await;
        coordinator::recover(home, j, m).await.unwrap();
        assert_eq!(
            j.get::<Swap>("swap", &id).await.unwrap().unwrap().state,
            "refunded",
            "spent change cannot block timed refund"
        );
        assert_eq!(
            balance(home, url).await,
            before + s.plan.gross - s.plan.claim_fee
        );
    }
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_claim_quarantine_and_witness_normalization() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.a.faults.invalid_dleq.store(true, SeqCst);
    f.step(false).await;
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert_eq!(s.state, "claim_quarantined");
    assert_eq!(
        balance(&f.taker, &f.a.url).await,
        0,
        "unverified claim never credited"
    );
    f.a.faults.uppercase_witness.store(true, SeqCst);
    let maker = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let hash = &s.quote.as_ref().unwrap().request.hash;
    let observed = mint::states(&f.a.url, &maker.outgoing).await.unwrap();
    assert!(observed.states.iter().all(|state| matches!(&state.witness,
        Some(cashu::nuts::Witness::HTLCWitness(w)) if w.preimage == s.preimage.as_ref().unwrap().to_uppercase())));
    assert_eq!(
        mint::witness(&f.a.url, &maker.outgoing, hash)
            .await
            .unwrap(),
        s.preimage
    );
    f.a.faults.hide_witness.store(true, SeqCst);
    assert!(
        !mint::claim_not_landed(&f.jt, "no-such-attempt", &f.a.url, &maker.outgoing, hash)
            .await
            .unwrap(),
        "SPENT without witness must fail closed"
    );
    f.a.faults.hide_witness.store(false, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert!(
        f.jt.get::<mint::Attempt>("attempt", &format!("{id}-refund"))
            .await
            .unwrap()
            .is_none()
    );
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        24,
        "maker still recovers revealed preimage"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_reports_witness_emission_unverified() {
    let f = Fixture::new(0).await;
    f.a.faults.hide_witness.store(true, SeqCst);
    let binary = env!("CARGO_BIN_EXE_maxplayer-trade");
    let out = tokio::process::Command::new(binary)
        .args(["--home", f.maker.to_str().unwrap(), "preflight", &f.a.url])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        v["nut07_witnesses"]
            .as_str()
            .unwrap()
            .starts_with("unverified:")
    );
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_unlanded_and_ambiguous_claims_stay_retryable() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.a.faults.reject_swap.store(true, SeqCst);
    f.step(false).await;
    let claim = format!("{id}-claim");
    let a =
        f.jt.get::<serde_json::Value>("attempt", &claim)
            .await
            .unwrap()
            .unwrap();
    assert!(a["result"].is_null());
    assert_eq!(a["quarantined"], false);
    f.a.faults.reject_swap.store(false, SeqCst);
    f.a.faults.invalid_dleq.store(true, SeqCst);
    f.a.faults.reject_restore_after_swap.store(true, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    let a =
        f.jt.get::<serde_json::Value>("attempt", &claim)
            .await
            .unwrap()
            .unwrap();
    assert!(a["result"].is_array());
    assert_eq!(
        a["quarantined"], false,
        "restore outage cannot establish commit"
    );
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("second_validated")
    );
    f.a.faults.reject_restore.store(false, SeqCst);
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("claim_quarantined")
    );
    assert_eq!(balance(&f.taker, &f.a.url).await, 0);
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_maker_claim_quarantined() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    f.step(false).await;
    f.b.faults.invalid_dleq.store(true, SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("claim_quarantined")
    );
    assert_eq!(balance(&f.maker, &f.b.url).await, 0);
    let a =
        f.jm.get::<serde_json::Value>("attempt", &format!("{id}-claim"))
            .await
            .unwrap()
            .unwrap();
    assert!(a["result"].is_array());
    assert_eq!(a["quarantined"], true);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        f.jm.get::<serde_json::Value>("attempt", &format!("{id}-claim"))
            .await
            .unwrap()
            .unwrap(),
        a
    );
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_all_relays_down_after_both_locks_still_refunds() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    let s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let q = s.quote.unwrap();
    f.relay.shutdown();
    wait_past(q.short + q.margin).await;
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("refunded"),
        "SAFETY: relay outage must not block mint-only maker refund"
    );
    wait_past(q.long + q.margin).await;
    coordinator::recover(&f.taker, &f.jt, &f.mt).await.unwrap();
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("refunded"),
        "SAFETY: relay outage must not block mint-only taker refund"
    );
    assert_eq!(balance(&f.maker, &f.a.url).await, 128);
    assert_eq!(balance(&f.taker, &f.b.url).await, 128);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_missing_quote_ack_cannot_authorize_maker_lock() {
    use maxplayer_trade::market::{Delivery, Publication};
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    // Model a relay delivering the quote while losing every OK reply. The
    // taker's first lock is genuine; only the maker's receipt is absent.
    let outbox = f.jm.all::<nostr_sdk::Event>("outbox").await.unwrap();
    let quote = outbox
        .into_iter()
        .find(|e| f.mt.decode(e).is_ok_and(|m| m.step == "quote"))
        .unwrap();
    let mut row =
        f.jm.get::<Publication>("publication", &quote.id.to_hex())
            .await
            .unwrap()
            .unwrap();
    for d in row.relays.values_mut() {
        *d = Delivery {
            ack: false,
            attempts: 12,
            next: u64::MAX,
        };
    }
    f.jm.put("publication", &quote.id.to_hex(), &row)
        .await
        .unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), f.mm.inbox.recv())
        .await
        .unwrap()
        .unwrap();
    let _ = coordinator::handle(&f.maker, &f.jm, &f.mm, &event).await;
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("quoted"),
        "SAFETY: missing ACK must not advance maker money state"
    );
    assert!(
        f.jm.get::<mint::Attempt>("attempt", &format!("{id}-lock"))
            .await
            .unwrap()
            .is_none(),
        "SAFETY: missing ACK must not create lock authorization"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_rechecks_preflight_after_planning_before_effects() {
    let f = Fixture::new(0).await;
    let p = mint::plan(&f.taker, &f.b.url, 24, 16).await.unwrap();
    let key = cashu::nuts::SecretKey::generate().public_key().to_string();
    let c = mint::conditions(&"ab".repeat(32), &key, &key, coordinator::now() + 3600).unwrap();
    f.b.faults.missing_nut.store(14, SeqCst);
    assert!(
        mint::lock(
            &f.taker,
            &f.jt,
            "preflight-lock",
            &p,
            &c,
            coordinator::now() + 60
        )
        .await
        .is_err()
    );
    assert!(
        f.jt.get::<mint::Attempt>("attempt", "preflight-lock")
            .await
            .unwrap()
            .is_none(),
        "SAFETY: changed preflight must refuse lock before intent or funds move"
    );
    assert_eq!(balance(&f.taker, &f.b.url).await, 128);
    mint::unspent(&f.b.url, &p.inputs).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_rechecks_preflight_before_reserving_money() {
    let f = Fixture::new(0).await;
    let lot = f.list(false).await;
    f.b.faults.missing_nut.store(7, SeqCst);
    assert!(
        coordinator::start_take(&f.taker, &f.jt, &f.mt, &lot, 128, 1, 16)
            .await
            .is_err()
    );
    assert!(f.jt.all::<Swap>("swap").await.unwrap().is_empty());
    assert_eq!(balance(&f.taker, &f.b.url).await, 128);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lock_cap_applies_at_planning_and_submission() {
    let h = tempfile::tempdir().unwrap();
    let m = MintFixture::start(0).await;
    fund(h.path(), &m.url, 100_001).await;
    let j = maxplayer_trade::journal::Journal::open(h.path())
        .await
        .unwrap();
    let mut p = mint::plan(h.path(), &m.url, 100_000, 0).await.unwrap();
    assert!(
        mint::plan(h.path(), &m.url, 100_001, 0).await.is_err(),
        "SAFETY: planning must reject gross above 100,000"
    );
    let key = cashu::nuts::SecretKey::generate().public_key().to_string();
    let c = mint::conditions(&"ab".repeat(32), &key, &key, coordinator::now() + 3600).unwrap();
    p.gross = 100_001;
    assert!(
        mint::lock(h.path(), &j, "cap", &p, &c, coordinator::now() + 60)
            .await
            .is_err(),
        "SAFETY: submission must reject gross above 100,000"
    );
    assert!(
        j.get::<mint::Attempt>("attempt", "cap")
            .await
            .unwrap()
            .is_none()
    );
    p.gross = 100_000;
    let proofs = mint::lock(h.path(), &j, "cap", &p, &c, coordinator::now() + 60)
        .await
        .unwrap();
    assert_eq!(
        proofs.iter().map(|p| u64::from(p.amount)).sum::<u64>(),
        100_000
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayed_lock_rechecks_preflight_without_losing_intent() {
    let f = Fixture::new(0).await;
    let p = mint::plan(&f.taker, &f.b.url, 24, 16).await.unwrap();
    let key = cashu::nuts::SecretKey::generate().public_key().to_string();
    let c = mint::conditions(&"ab".repeat(32), &key, &key, coordinator::now() + 3600).unwrap();
    f.b.faults.reject_swap.store(true, SeqCst);
    assert!(
        mint::lock(
            &f.taker,
            &f.jt,
            "replay-preflight",
            &p,
            &c,
            coordinator::now() + 60
        )
        .await
        .is_err()
    );
    assert!(
        f.jt.get::<mint::Attempt>("attempt", "replay-preflight")
            .await
            .unwrap()
            .is_some()
    );
    f.b.faults.reject_swap.store(false, SeqCst);
    f.b.faults.missing_nut.store(14, SeqCst);
    assert!(
        mint::execute(&f.taker, &f.jt, "replay-preflight")
            .await
            .is_err(),
        "SAFETY: replayed lock must recheck preflight before swap POST"
    );
    mint::unspent(&f.b.url, &p.inputs).await.unwrap();
    assert_eq!(balance(&f.taker, &f.b.url).await, 128);
    f.b.faults.missing_nut.store(0, SeqCst);
    assert_eq!(
        mint::execute(&f.taker, &f.jt, "replay-preflight")
            .await
            .unwrap()
            .iter()
            .map(|p| u64::from(p.amount))
            .sum::<u64>(),
        24
    );
}

#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r4_recover_unreachable_withdrawal_does_not_block_other_mint_refund_or_exit() {
    let mut f = Fixture::new(0).await;
    let id = through_second(&mut f).await;
    let s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let q = s.quote.unwrap();
    // Synthetic authorization, never a real funded home or invoice.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let stuck_id = "00000000-0000-4000-8000-000000000001";
    let stuck = serde_json::json!({
        "id": stuck_id, "mint":dead, "invoice":"fixture-only", "state":"quote_created",
        "quote":{"quote":"fixture-quote", "amount":1, "fee_reserve":1, "expiry":coordinator::now()+3600, "state":"UNPAID"}, "input_fee":0, "inputs":[], "outputs":[],
        "final_reply":false, "seen_pending":false, "result":null, "change":null
    });
    f.jm.put("withdrawal", stuck_id, &stuck).await.unwrap();
    wait_past(q.short + q.margin).await;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-trade"))
            .args([
                "--home",
                f.maker.to_str().unwrap(),
                "--relay",
                &f.relay_url,
                "recover",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("SAFETY: recover must exit despite unreachable authorization")
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "SAFETY: distinct incomplete recovery exit"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(stuck_id));
    assert!(stdout.contains("recovery_incomplete"));
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("refunded"),
        "SAFETY: unrelated reachable mint must refund in same pass"
    );
    assert_eq!(
        f.jm.get::<serde_json::Value>("withdrawal", stuck_id)
            .await
            .unwrap()
            .unwrap(),
        stuck,
        "SAFETY: unreachable item must be untouched"
    );
    assert_eq!(balance(&f.maker, &f.a.url).await, 128);
}
