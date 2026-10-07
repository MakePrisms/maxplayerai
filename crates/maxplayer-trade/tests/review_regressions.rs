mod support;
use maxplayer_trade::{coordinator, mint};
use sha2::{Digest, Sha256};
use support::*;

#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
    wait_past((q.short - q.cutoff + 20).max(q.long + q.margin)).await;
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
#[tokio::test(flavor = "multi_thread")]
async fn h2_unsent_lock_expires_and_releases_reservation() {
    use cdk::cdk_database::WalletDatabase;
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.b.faults.reject_swap.store(true, SeqCst);
    f.step(false).await;
    assert_eq!(f.state(false, &id).await.as_deref(), Some("accepted"));
    f.b.faults.clock_offset.store(60, SeqCst);
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
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

#[tokio::test(flavor = "multi_thread")]
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
#[tokio::test(flavor = "multi_thread")]
async fn h2_pending_inputs_prevent_abandonment() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.b.faults.reject_swap.store(true, SeqCst);
    f.step(false).await;
    f.b.faults.clock_offset.store(60, SeqCst);
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
#[tokio::test(flavor = "multi_thread")]
async fn h2_maker_abandoned_lock_releases_listing() {
    use cdk::cdk_database::WalletDatabase;
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    f.a.faults.reject_swap.store(true, SeqCst);
    f.step(true).await;
    f.a.faults.clock_offset.store(60, SeqCst);
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
