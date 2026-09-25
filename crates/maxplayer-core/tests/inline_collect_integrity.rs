//! Inline delivery (§6.4) through the real `collect` — the money seam and the free seam.
//!
//! The git twin of this file (`collect_integrity.rs`) drives the tip-match: the buyer must refuse
//! to pay when the delivered branch does not tip at the accepted commit. An inline delivery has no
//! branch and no tip, so its integrity gate is a re-derivation instead: the buyer hashes the answer
//! it holds and requires that digest to equal the one the seller co-signed.
//!
//! Both rounds of adversarial review on this feature found the same class of defect — a seam that
//! reads correctly and is never executed. Reading is what missed that `fetch_job_view_async`
//! parsed the inline answer and dropped it, which alone made every inline result unacceptable.
//! These tests execute the seam.
//!
//! Red-on-revert: routing an inline bind past the digest re-derivation flips `spent()==0`;
//! dropping the free arm's materialize or its §7.0 record flips the second test.
#![cfg(all(unix, feature = "wallet"))]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use maxplayer_core::budget::BudgetGate;
use maxplayer_core::collect::{CollectError, CollectRequest, collect_async};
use maxplayer_core::home;
use maxplayer_core::job_lifecycle::AcceptedBind;
use maxplayer_core::receipt::{DeliveryKind, result_content_hash_hex};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "maxplayer-inline-itest-{label}-{}-{id}",
        std::process::id()
    ))
}

/// An accept-bind for an INLINE delivery, exactly as `accept_claim_async` writes one: the answer
/// on the bind, its digest in the integrity slot, no remote and no branch, kind `inline`.
fn inline_bind(job_id: &str, seller_pubkey: &str, answer: &str, free: bool) -> AcceptedBind {
    AcceptedBind {
        private_evidence: None,
        delivery_kind: Some(DeliveryKind::Inline.as_str().to_owned()),
        inline_answer: Some(answer.to_owned()),
        payment_mode: if free {
            maxplayer_core::gateway::PaymentMode::None
        } else {
            maxplayer_core::gateway::PaymentMode::Sat
        },
        job_id: job_id.to_owned(),
        claim_id: "c".repeat(64),
        result_id: "d".repeat(64),
        seller_pubkey: seller_pubkey.to_owned(),
        // The inline analogue of the delivered commit oid.
        commit_oid: result_content_hash_hex(answer),
        // An inline delivery has neither.
        repo: String::new(),
        branch: String::new(),
        job_hash: "e".repeat(64),
        amount_sats: if free { 0 } else { 2 },
        accept_event_id: "f".repeat(64),
        accepted_at: 1,
        seller_signature: "ab".repeat(32),
        creq_hash: None,
        accepted_mints: Vec::new(),
        funding_mint: None,
        delivery_mint: None,
        agent_used: None,
        model_used: None,
        contribution: None,
    }
}

fn write_bind(home: &home::MaxplayerHome, bind: &AcceptedBind) {
    let jobs = home.root.join("jobs");
    fs::create_dir_all(&jobs).expect("jobs dir");
    fs::write(
        jobs.join(format!("{}.json", bind.job_id)),
        serde_json::to_string(bind).expect("serialize bind"),
    )
    .expect("write bind");
}

/// THE INLINE INTEGRITY GATE. The answer on the bind is tampered with after the digest was
/// recorded, so re-deriving it no longer reproduces what the seller co-signed. The buyer must
/// refuse before the wallet opens, and burn nothing — the same guarantee the git tip-match gives.
#[tokio::test(flavor = "current_thread")]
async fn collect_refuses_to_pay_when_the_answer_does_not_reproduce_the_bound_digest() {
    let root = temp("tampered");
    let _ = fs::remove_dir_all(&root);
    let home = home::bootstrap(&root).expect("home");
    let pubkey_hex = home::public_key_hex(&home).expect("pubkey");

    let job_id = "a".repeat(64);
    let mut bind = inline_bind(&job_id, &pubkey_hex, "Europe/Zagreb", false);
    // The digest slot still holds the digest of the ORIGINAL answer.
    bind.inline_answer = Some("Europe/Belgrade".to_owned());
    write_bind(&home, &bind);

    let mut gate = BudgetGate::from_home(&home).expect("gate");
    let error = collect_async(
        &home,
        Some(&mut gate),
        CollectRequest {
            job_id: job_id.clone(),
            out: None,
        },
    )
    .await
    .expect_err("a tampered answer must refuse the pay");

    assert!(
        matches!(error, CollectError::Pay(_)),
        "must be a pay refusal: {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains("inline answer digest") && message.contains("does not match"),
        "must refuse at the inline digest re-derivation, got: {error}"
    );
    assert_eq!(gate.spent(), 0, "an integrity mismatch must burn ZERO spend");
    assert_eq!(
        BudgetGate::from_home(&home).expect("reload").spent(),
        0,
        "durable spent must stay 0 after an integrity refusal"
    );
    assert!(
        !home.root.join("payment-journal").exists(),
        "no payment journal may be created on an integrity refusal"
    );
    assert!(
        !home.root.join("results").join(&job_id).exists(),
        "a refused delivery must materialize nothing"
    );
}

/// The FREE inline path end to end: the answer is written as `answer.txt`, and §7.0's collect
/// record is written beside it. The record is what makes a completed free trade visible at all;
/// the git free arm writes one, and this arm does not go through it.
#[tokio::test(flavor = "current_thread")]
async fn a_free_inline_collect_materializes_the_answer_and_records_it() {
    let root = temp("free");
    let _ = fs::remove_dir_all(&root);
    let home = home::bootstrap(&root).expect("home");
    let pubkey_hex = home::public_key_hex(&home).expect("pubkey");

    let job_id = "b".repeat(64);
    let answer = "Europe/Zagreb";
    write_bind(&home, &inline_bind(&job_id, &pubkey_hex, answer, true));

    let outcome = collect_async(
        &home,
        None,
        CollectRequest {
            job_id: job_id.clone(),
            out: None,
        },
    )
    .await
    .expect("a free inline collect must succeed");

    assert_eq!(outcome.files, vec!["answer.txt".to_owned()]);
    let written = fs::read_to_string(PathBuf::from(&outcome.path).join("answer.txt"))
        .expect("answer.txt must exist");
    assert_eq!(written, answer, "the answer is written verbatim");

    let record = home.root.join("collects").join(format!("{job_id}.json"));
    assert!(
        record.is_file(),
        "§7.0 — a completed free trade must leave a record, never silence"
    );
}

/// A bind that disagrees with ITSELF about the delivery mode is refused at load, not half-honoured.
/// The pay path routes on `inline_answer` and `collect` routes on `delivery_kind`; a bind carrying
/// one without the other pays down the inline route and then materializes down the git route,
/// after the spend.
#[tokio::test(flavor = "current_thread")]
async fn a_bind_that_contradicts_its_own_delivery_mode_is_refused() {
    let root = temp("split");
    let _ = fs::remove_dir_all(&root);
    let home = home::bootstrap(&root).expect("home");
    let pubkey_hex = home::public_key_hex(&home).expect("pubkey");

    let job_id = "c".repeat(64);
    let mut bind = inline_bind(&job_id, &pubkey_hex, "Europe/Zagreb", false);
    // The answer stays; the kind is dropped. Each reader would now pick a different mode.
    bind.delivery_kind = None;
    write_bind(&home, &bind);

    let mut gate = BudgetGate::from_home(&home).expect("gate");
    let error = collect_async(
        &home,
        Some(&mut gate),
        CollectRequest {
            job_id: job_id.clone(),
            out: None,
        },
    )
    .await
    .expect_err("a self-contradictory bind must be refused");

    assert!(
        error.to_string().contains("disagrees with itself"),
        "must refuse the ambiguous bind at load, got: {error}"
    );
    assert_eq!(gate.spent(), 0, "an ambiguous bind must burn ZERO spend");
}
