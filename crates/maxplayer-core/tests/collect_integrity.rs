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
use maxplayer_core::budget::BudgetGate;
use maxplayer_core::collect::{collect_async, CollectError, CollectRequest};
use maxplayer_core::home;
use maxplayer_core::job_lifecycle::AcceptedBind;
use maxplayer_core::receipt::{ReceiptPreimage, DeliveryKind, EXEC_METADATA_COMMITMENT_EMPTY};

use nostr_sdk::secp256k1::Message;
use nostr_sdk::Keys;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("maxplayer-collect-itest-{label}-{}-{id}", std::process::id()))
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
        payment_mode: maxplayer_core::gateway::PaymentMode::Sat,
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
        funding_mint: None,
        delivery_mint: None,
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

// ── #374 §19 buyer-side execution-sentinel gate — full-path red-prove over a real HTTPS fetch ────
//
// These bind the DELIVERED TIP (so tip-match PASSES) with a valid cosig (so the cosig tooth PASSES),
// leaving the §19 execution-sentinel gate as the one thing that can refuse. A sentinel-less or
// replayed delivery must refuse `no_sentinel` with ZERO spend, no journal, no files; a delivery
// carrying THIS job's sentinel must get PAST the gate (whatever happens downstream is not the gate's
// concern). Red-on-revert: neuter `delivery_tree_carries_sentinel` to a blanket accept and the two
// refusals below pay a sentinel-less / replayed delivery — they go red.
//
// WHY collect_async IS the watcher proof: the buyer's auto-settle delivery watcher pays through
// `settle_job` → `settle_after_pay(|| collect_async(...))` (buyer/mod.rs:2139) — the SAME pay closure
// manual collect uses (buyer/mod.rs:946), and `collect_async` is the ONLY caller of
// `authorize_pay_async`, the sole ecash-melt chokepoint. So a refusal here refuses on the watcher's
// own melt path; and `settle_after_pay` flips `reserved → spent` ONLY on Ok, so `spent_total` cannot
// move on this Err.
//
// NOTE: integration binary — NOT run in the implementer's sandbox (integration tests group-signal on
// this box). Verified to COMPILE (`cargo test --test collect_integrity --features wallet --no-run`);
// the unblinded CI gate (pr-feedback) runs it.

/// An upstream repo whose `main` tip is a from-scratch delivery tree (README + optionally the
/// execution-sentinel manifest at its well-known path). Returns (dir, tip_oid).
fn make_upstream_delivery(label: &str, sentinel: Option<&str>) -> (PathBuf, String) {
    let dir = temp(label);
    fs::create_dir_all(&dir).expect("upstream dir");
    git_in(&dir, &["init", "--initial-branch=main"]);
    git_in(&dir, &["config", "user.name", "Upstream Author"]);
    git_in(&dir, &["config", "user.email", "upstream@example.invalid"]);
    fs::write(dir.join("README.md"), "delivered\n").expect("write readme");
    if let Some(content) = sentinel {
        fs::write(
            dir.join(maxplayer_core::delivery_sentinel::SENTINEL_FILE),
            content,
        )
        .expect("write sentinel");
    }
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-m", "delivery"]);
    let tip_oid = git_stdout(&dir, &["rev-parse", "HEAD"]);
    (dir, tip_oid)
}

/// A from-scratch accept-bind pinning the DELIVERED tip (tip-match passes) with the given job hash.
fn from_scratch_bind(
    job_id: &str,
    pubkey_hex: &str,
    tip_oid: &str,
    repo_url: &str,
    job_hash: &str,
) -> AcceptedBind {
    AcceptedBind {
        payment_mode: maxplayer_core::gateway::PaymentMode::Sat,
        job_id: job_id.to_owned(),
        claim_id: "c".repeat(64),
        result_id: "d".repeat(64),
        seller_pubkey: pubkey_hex.to_owned(),
        commit_oid: tip_oid.to_owned(),
        repo: repo_url.to_owned(),
        branch: "main".into(),
        job_hash: job_hash.to_owned(),
        amount_sats: 2,
        accept_event_id: "f".repeat(64),
        accepted_at: 1,
        seller_signature: String::new(),
        creq_hash: None,
        accepted_mints: Vec::new(),
        funding_mint: None,
        delivery_mint: None,
        agent_used: None,
        model_used: None,
        contribution: None,
    }
}

fn write_bind(home: &maxplayer_core::home::MaxplayerHome, bind: &AcceptedBind) {
    let jobs = home.root.join("jobs");
    fs::create_dir_all(&jobs).expect("jobs dir");
    fs::write(
        jobs.join(format!("{}.json", bind.job_id)),
        serde_json::to_string(bind).expect("serialize bind"),
    )
    .expect("write bind");
}

/// Drive collect against a delivery whose tip carries `sentinel` (or none), pinned + cosigned so the
/// only refusal point is the §19 gate. Returns the collect result plus the gate for spend assertions.
async fn collect_over_delivery(
    label: &str,
    mount_seed: &str,
    job_hash: &str,
    sentinel: Option<&str>,
) -> (Result<maxplayer_core::collect::CollectOutcome, CollectError>, BudgetGate, PathBuf) {
    init_test_env();
    let (upstream, tip_oid) = make_upstream_delivery(label, sentinel);
    let mount = format!("/git/{}/repo.git", mount_seed);
    let server = GitHttpAuthServer::spawn(&upstream, &mount);
    let repo_url = server.repo_url();

    let root = temp(&format!("home-{label}"));
    let _ = fs::remove_dir_all(&root);
    let home = home::bootstrap(&root).expect("home");
    let secret_hex = home::read_secret_key_hex(&home).expect("secret");
    let pubkey_hex = home::public_key_hex(&home).expect("pubkey");

    let job_id = "a".repeat(64);
    let mut bind = from_scratch_bind(&job_id, &pubkey_hex, &tip_oid, &repo_url, job_hash);
    bind.seller_signature = seller_cosig(&secret_hex, &pubkey_hex, &bind);
    write_bind(&home, &bind);

    let mut gate = BudgetGate::from_home(&home).expect("gate");
    let result = collect_async(&home, &mut gate, CollectRequest { job_id, out: None }).await;

    drop(server);
    let _ = fs::remove_dir_all(&upstream);
    (result, gate, root)
}

// RED-PROVE (missing) — a sentinel-less delivery refuses `no_sentinel` with ZERO spend, no journal,
// no files; the refusal is durably journalled for §17.
#[tokio::test(flavor = "current_thread")]
async fn collect_refuses_no_sentinel_when_delivery_carries_none() {
    let job_hash = "1a".repeat(32);
    let (result, gate, root) = collect_over_delivery("nosentinel", &"cd".repeat(32), &job_hash, None).await;

    let error = result.expect_err("a sentinel-less delivery must refuse");
    assert!(matches!(error, CollectError::Pay(_)), "must be a pay refusal: {error}");
    assert!(error.to_string().contains("no_sentinel"), "refuses no_sentinel, got: {error}");
    assert_eq!(gate.spent(), 0, "a no_sentinel refusal must burn ZERO spend");
    assert_eq!(
        BudgetGate::from_home(&home::bootstrap(&root).expect("reload home")).expect("reload").spent(),
        0,
        "durable spent must stay 0"
    );
    assert!(!root.join("payment-journal").exists(), "no payment journal on a no_sentinel refusal");
    assert!(!root.join("results").join("a".repeat(64)).exists(), "no files materialized on refusal");
    assert!(root.join("sentinel-refusals").exists(), "the refusal is journalled for §17 (the artifact, not silence)");
    let _ = fs::remove_dir_all(&root);
}

// RED-PROVE (replay) — a delivery carrying a VALID sentinel minted for a DIFFERENT job still refuses:
// job-binding, not mere presence.
#[tokio::test(flavor = "current_thread")]
async fn collect_refuses_no_sentinel_on_a_replayed_sentinel_from_another_job() {
    let this_job = "1a".repeat(32);
    let other_job = "bb".repeat(32);
    let replayed = maxplayer_core::delivery_sentinel::render_manifest(
        &other_job,
        maxplayer_core::delivery_sentinel::DeliveryMode::FromScratch,
        1,
        12,
    );
    let (result, gate, root) =
        collect_over_delivery("replay", &"ef".repeat(32), &this_job, Some(&replayed)).await;

    let error = result.expect_err("a replayed sentinel must refuse");
    assert!(matches!(error, CollectError::Pay(_)), "must be a pay refusal: {error}");
    assert!(error.to_string().contains("no_sentinel"), "refuses no_sentinel (replay), got: {error}");
    assert_eq!(gate.spent(), 0, "a replay refusal must burn ZERO spend");
    let _ = fs::remove_dir_all(&root);
}

// POSITIVE CONTROL — a delivery carrying THIS job's sentinel gets PAST the §19 gate. Whatever refuses
// downstream (no live wallet/mint in this test) is NOT a no_sentinel refusal — proving the gate does
// not false-refuse a genuine delivery.
#[tokio::test(flavor = "current_thread")]
async fn collect_passes_the_sentinel_gate_for_a_valid_delivery() {
    let job_hash = "1a".repeat(32);
    let manifest = maxplayer_core::delivery_sentinel::render_manifest(
        &job_hash,
        maxplayer_core::delivery_sentinel::DeliveryMode::FromScratch,
        1,
        12,
    );
    let (result, _gate, root) =
        collect_over_delivery("valid", &"12".repeat(32), &job_hash, Some(&manifest)).await;

    // It may still fail downstream (no funded wallet here), but it must NOT be the sentinel gate.
    if let Err(error) = &result {
        assert!(
            !error.to_string().contains("no_sentinel"),
            "a valid job-bound sentinel must pass the gate (downstream failure is fine), got: {error}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

// ── #387 preflight-before-reserve ORDER gate — a dead mint burns ZERO budget, over the REAL path ───
//
// The see-saw (2eda85d → ef7a49e): the cross-runtime deadlock fix unparked the hang but LEAKED budget
// — a dead mint left `gate.spent()==charged` because `authorize_then_attempt` reserves BEFORE the
// effect and never rolls back. Option A (ef7a49e) runs `require_fee_safe_amount` as a WORKER-side
// `PreflightFee` in `authorize_pay_async` AFTER `spawn_effects` and BEFORE `gate.authorize_then_attempt`,
// so a dead mint refuses BEFORE the budget reserve (zero spend). A primitive unit test
// (`pay_path_timeout_refuses_bounded_without_charging_the_budget`, payment_wallet.rs) pins that ORDER
// at the unit level; here the SAME order is an EXECUTING guard over the real `collect_async` →
// `authorize_pay_async` entrypoint — the buyer watcher's own melt path (see the §19 note above) — driven
// through a real HTTPS delivery fetch.
//
// The delivery is pinned + cosigned + carries THIS job's execution sentinel, so the cosig tooth, the
// tip-match and the §19 gate all PASS and the flow reaches the wallet; the wallet then opens at a DEAD
// realized mint (127.0.0.1:1, TCP connect refused), so the pre-reserve preflight is the one thing left
// to refuse. It must cancel `CancelledMintUnreachable` ("no funds moved") with ZERO spend, bounded
// (no park), no journal.
//
// Red-on-reorder (measured, non-vacuous): move the `effects.preflight_fee(..)?` guard BELOW
// `gate.authorize_then_attempt` and the dead mint reaches the gate first — the reserve commits
// (`gate.spent()==charged`, and a journal is written) before the preflight ever runs, so every ZERO
// assertion below flips and this test goes RED. That is the exact budget leak Option A closes.
//
// `allow_real_mints=true` is ISOLATED to this test home so the pay path will resolve + open the wallet
// at the (dead) real mint; 127.0.0.1:1 refuses the connect, so NO real mint is contacted and no money
// can move. The cosig is unaffected — the `ReceiptPreimage` binds no mint
// (`receipt_preimage_digest_is_independent_of_realized_mint`, authorize_pay.rs).
#[tokio::test(flavor = "current_thread")]
async fn collect_refuses_dead_mint_at_preflight_before_the_budget_reserve() {
    init_test_env();
    // An unroutable mint URL that refuses the TCP connect instantly — the deterministic dead-mint
    // stand-in the pay-path dust guard's own unit test uses (payment_wallet.rs `DEAD_MINT`); no live
    // network, no real hang wait.
    const DEAD_MINT: &str = "https://127.0.0.1:1";

    // A from-scratch delivery whose tip carries THIS job's execution sentinel, so cosig + tip-match +
    // §19 all pass and the only remaining refusal point is the wallet.
    let job_hash = "1a".repeat(32);
    let manifest = maxplayer_core::delivery_sentinel::render_manifest(
        &job_hash,
        maxplayer_core::delivery_sentinel::DeliveryMode::FromScratch,
        1,
        12,
    );
    let (upstream, tip_oid) = make_upstream_delivery("deadmint", Some(&manifest));
    let mount = format!("/git/{}/repo.git", "34".repeat(32));
    let server = GitHttpAuthServer::spawn(&upstream, &mount);
    let repo_url = server.repo_url();

    let root = temp("home-deadmint");
    let _ = fs::remove_dir_all(&root);
    let mut home = home::bootstrap(&root).expect("home");
    // Real-mint opt-in ISOLATED to this test home: lets `plan_payment` + `open_wallet_at_mint_async`
    // resolve/open the wallet at the (dead) real mint so the preflight is actually exercised. The mint
    // is connection-refused loopback, so this opts in to no real money.
    home.config.allow_real_mints = true;
    let secret_hex = home::read_secret_key_hex(&home).expect("secret");
    let pubkey_hex = home::public_key_hex(&home).expect("pubkey");

    let job_id = "a".repeat(64);
    let mut bind = from_scratch_bind(&job_id, &pubkey_hex, &tip_oid, &repo_url, &job_hash);
    // Seal the DEAD mint as the funding (source) mint AND put it in the accepted set, so `plan_payment`
    // resolves a DIRECT pay at it (no cross-mint hop — that would touch other mints). The cosig is
    // computed AFTER, but the receipt preimage binds no mint, so it still passes.
    bind.funding_mint = Some(DEAD_MINT.to_owned());
    bind.accepted_mints = vec![DEAD_MINT.to_owned()];
    bind.seller_signature = seller_cosig(&secret_hex, &pubkey_hex, &bind);
    write_bind(&home, &bind);

    let mut gate = BudgetGate::from_home(&home).expect("gate");
    let started = std::time::Instant::now();
    let result =
        collect_async(&home, &mut gate, CollectRequest { job_id: job_id.clone(), out: None }).await;
    let elapsed = started.elapsed();

    drop(server);
    let _ = fs::remove_dir_all(&upstream);

    // (a) The pre-reserve preflight cancels on the dead mint with the typed
    // `CancelledMintUnreachable` identity ("authorize_pay cancelled: mint … no funds moved") — NOT
    // success, NOT the §19 `no_sentinel` gate (that PASSED, this job's sentinel is present), NOT a
    // hang.
    let error = result.expect_err("a dead realized mint must refuse the pay BEFORE the budget reserve");
    assert!(matches!(error, CollectError::Pay(_)), "must be a pay refusal: {error}");
    let message = error.to_string();
    assert!(
        message.contains("authorize_pay cancelled: mint") && message.contains("no funds moved"),
        "must cancel at the pre-reserve fee-safe preflight (dead mint), got: {error}"
    );
    assert!(
        !message.contains("no_sentinel"),
        "the §19 gate must have PASSED (job-bound sentinel present) — the refusal is the wallet \
         preflight, not the sentinel gate, got: {error}"
    );
    // (b) ZERO SPEND — the preflight refused BEFORE `gate.authorize_then_attempt` reserved any budget.
    // In-memory AND durable, and NO journal is written (the journal is created only after the preflight
    // passes). Reorder the preflight below the gate and the dead mint reaches the reserve first: this
    // flips to `spent()==charged` (the phantom spend #387 closes).
    assert_eq!(gate.spent(), 0, "a dead-mint preflight refusal must burn ZERO spend (see the #387 see-saw)");
    assert_eq!(
        BudgetGate::from_home(&home::bootstrap(&root).expect("reload home")).expect("reload").spent(),
        0,
        "durable spent must stay 0 after a pre-reserve preflight refusal"
    );
    assert!(!root.join("payment-journal").exists(), "no payment journal on a pre-reserve refusal");
    // (c) Bounded — no park. The preflight is `MINT_TOUCH_TIMEOUT`-bounded and 127.0.0.1:1 refuses the
    // connect fast; the whole worker round-trip is capped by the bridge recv ceiling (20s). A
    // regression to the pre-#387 caller-runtime deadlock would park forever (the see-saw's other end).
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "must be bounded (no park), took {elapsed:?}"
    );

    let _ = fs::remove_dir_all(&root);
}
