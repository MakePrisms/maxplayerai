//! collect integrity gate — real git-over-HTTPS fetch (closes the tip-match gap end to end).
//!
//! The buyer's single-call `collect` verifies the delivered branch actually tips at the accepted
//! commit before it pays. This drives that gate against the real in-process smart-HTTP verifier
//! (git2 + reqwest, NIP-98 header injected up front — the SAME path authorize_pay uses) fetching a
//! loopback fixture whose `main` tip does NOT equal the oid bound in the accept-bind. The pay path
//! must refuse at the delivery tip-match with ZERO spend, and collect must materialize NO files.
//!
//! Red-on-revert: rewiring collect to pay/materialize regardless of the tip-match would flip
//! `spent()==0` and the "no results" assertion.
#![cfg(all(unix, feature = "wallet"))]

// This binary needs the fixture only to serve the repo (`spawn` + `repo_url`); its
// request-recording surface is what `relay_git_http_auth` asserts the NIP-98 wire against. Each
// integration binary compiles its own copy of the module, so the parts this one does not call read
// as dead here. Allowed at this `mod` site rather than inside the fixture, so dead-code analysis
// stays live in the binary that does use them.
#[allow(dead_code)]
mod git_http_fixture;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

use git_http_fixture::GitHttpAuthServer;
use mobee_core::budget::BudgetGate;
use mobee_core::collect::{collect_async, CollectError, CollectRequest};
use mobee_core::home;
use mobee_core::job_lifecycle::AcceptedBind;
use mobee_core::receipt::{ReceiptPreimage, DeliveryKind, EXEC_METADATA_COMMITMENT_EMPTY};

use nostr_sdk::secp256k1::Message;
use nostr_sdk::Keys;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("mobee-collect-itest-{label}-{}-{id}", std::process::id()))
}

static ENV_INIT: Once = Once::new();

/// Accept the fixture's self-signed cert (the in-process reqwest transport honors
/// `GIT_SSL_NO_VERIFY`) and bypass any ambient proxy for loopback. Staged once, before any fetch.
fn init_test_env() {
    ENV_INIT.call_once(|| {
        // SAFETY (edition 2024 set_var): funnels through this Once before any git/reqwest fetch, so
        // no racing test thread observes a partial update.
        unsafe {
            std::env::set_var("GIT_SSL_NO_VERIFY", "1");
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
            std::env::set_var("no_proxy", "127.0.0.1,localhost");
        }
    });
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed in {}", dir.display());
    String::from_utf8(out.stdout).expect("utf8").trim().to_owned()
}

/// Upstream repo the fixture serves: two commits on `main`. Returns (dir, tip_oid, base_oid) where
/// base_oid is the FIRST commit — a real oid that is NOT the branch tip.
fn make_upstream(label: &str) -> (PathBuf, String, String) {
    let dir = temp(label);
    fs::create_dir_all(&dir).expect("upstream dir");
    git_in(&dir, &["init", "--initial-branch=main"]);
    git_in(&dir, &["config", "user.name", "Upstream Author"]);
    git_in(&dir, &["config", "user.email", "upstream@example.invalid"]);
    fs::write(dir.join("README.md"), "base\n").expect("write base");
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-m", "base commit"]);
    let base_oid = git_stdout(&dir, &["rev-parse", "HEAD"]);
    fs::write(dir.join("README.md"), "tip\n").expect("write tip");
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-m", "tip commit"]);
    let tip_oid = git_stdout(&dir, &["rev-parse", "HEAD"]);
    (dir, tip_oid, base_oid)
}

/// The seller co-signature over the receipt preimage, built with the SAME fields authorize_pay's
/// `receipt_preimage_for` binds (buyer == seller == the home key in this test), so the pre-pay cosig
/// tooth PASSES and the refusal lands at the delivery tip-match, not the cosig.
fn seller_cosig(secret_hex: &str, pubkey_hex: &str, bind: &AcceptedBind) -> String {
    let preimage = ReceiptPreimage {
        job_hash: bind.job_hash.clone(),
        offer_id: bind.job_id.clone(),
        amount: bind.amount_sats,
        unit: "sat".to_owned(),
        buyer_pubkey: pubkey_hex.to_owned(),
        seller_pubkey: pubkey_hex.to_owned(),
        delivery_integrity_hash: bind.commit_oid.clone(),
        delivery_kind: DeliveryKind::Fork.as_str().to_owned(),
        exec_metadata_commitment: EXEC_METADATA_COMMITMENT_EMPTY.to_owned(),
        creq_hash: None,
    };
    let keys = Keys::parse(secret_hex).expect("keys");
    keys.sign_schnorr(&Message::from_digest(preimage.digest_bytes()))
        .to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn collect_refuses_pay_when_delivered_tip_differs_from_bound_oid() {
    init_test_env();
    let (upstream, tip_oid, base_oid) = make_upstream("upstream");
    // Sanity: the delivered tip and the oid we will bind are genuinely different.
    assert_ne!(tip_oid, base_oid);

    let mount = format!("/git/{}/repo.git", "ab".repeat(32));
    let server = GitHttpAuthServer::spawn(&upstream, &mount);
    let repo_url = server.repo_url();

    let root = temp("home");
    let _ = fs::remove_dir_all(&root);
    let home = home::bootstrap(&root).expect("home");
    let secret_hex = home::read_secret_key_hex(&home).expect("secret");
    let pubkey_hex = home::public_key_hex(&home).expect("pubkey");

    // Accept-bind pins base_oid (NOT the delivered tip). The seller cosig is valid so the refusal
    // lands at the tip-match. buyer == seller == the home key.
    let job_id = "a".repeat(64);
    let mut bind = AcceptedBind {
        job_id: job_id.clone(),
        claim_id: "c".repeat(64),
        result_id: "d".repeat(64),
        seller_pubkey: pubkey_hex.clone(),
        commit_oid: base_oid.clone(),
        repo: repo_url,
        branch: "main".into(),
        job_hash: "e".repeat(64),
        amount_sats: 2,
        accept_event_id: "f".repeat(64),
        accepted_at: 1,
        seller_signature: String::new(),
        creq_hash: None,
        accepted_mints: Vec::new(),
        realized_mint: None,
        agent_used: None,
        model_used: None,
        contribution: None,
    };
    bind.seller_signature = seller_cosig(&secret_hex, &pubkey_hex, &bind);

    let jobs = home.root.join("jobs");
    fs::create_dir_all(&jobs).expect("jobs dir");
    fs::write(
        jobs.join(format!("{job_id}.json")),
        serde_json::to_string(&bind).expect("serialize bind"),
    )
    .expect("write bind");

    let mut gate = BudgetGate::from_home(&home).expect("gate");
    let error = collect_async(
        &home,
        &mut gate,
        CollectRequest { job_id: job_id.clone(), out: None },
    )
    .await
    .expect_err("delivered tip != bound oid must refuse the pay");

    assert!(matches!(error, CollectError::Pay(_)), "must be a pay refusal: {error}");
    // The delivery verifier tip-matches the fetched branch tip against the accepted commit and
    // refuses the mismatch before returning — the machine integrity check the whole call rests on.
    let message = error.to_string();
    assert!(
        message.contains("delivery verification refused")
            && message.contains("does not match advertised"),
        "must refuse at the delivery tip-match (delivered tip != bound oid), got: {error}"
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
        "collect must NOT materialize files when the integrity check fails"
    );

    drop(server);
    let _ = fs::remove_dir_all(&upstream);
    let _ = fs::remove_dir_all(&root);
}
