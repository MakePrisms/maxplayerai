//! FREE JOB LANE — a `payment=none` post from a buyer that has NO WALLET, over a real relay.
//!
//! This is the post half of the lane made user-reachable. The daemon's `post_job` RPC now reads a
//! `payment` parameter and threads it into [`PostJobRequest::payment_mode`]; the mapping from the
//! RPC body to that field is unit-tested next to the RPC (`buyer::tests`), and what THIS binary
//! proves is the property that mapping exists for: with the mode set to `none`, the post path a
//! wallet-less buyer drives actually completes, and the offer that lands on the relay says `none`.
//!
//! The negative is asserted before AND after every post: `wallet/cdk-wallet.sqlite` must not exist.
//! `open_wallet_async` creates that file the moment it is called, so its continued absence is
//! machine evidence that no wallet was opened — not an inference from reading the code.
//!
//! The PAID control on the SAME wallet-less home must REFUSE. Without it, "the free post succeeded"
//! would be compatible with a post path that never needed a wallet in the first place; with it, the
//! mode is demonstrably the thing that decided.
//!
//! Relay is an in-process NIP-01 [`LocalRelay`] on loopback. No public relay, no mint, no money.
#![cfg(all(unix, feature = "wallet"))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use maxplayer_core::gateway::PaymentMode;
use maxplayer_core::home;
use maxplayer_core::job_lifecycle::{self, JobKind, PostJobRequest};

use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::{Client, Filter, Keys};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// An unroutable loopback mint URL that refuses the TCP connect instantly — the deterministic
/// dead-mint stand-in the pay-path tests use (`payment_wallet.rs`'s `DEAD_MINT`). No live network,
/// no real mint, no money, and no dependence on anyone else's uptime.
const DEAD_MINT: &str = "https://127.0.0.1:1";

fn temp(label: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("maxplayer-freepost-{label}-{}-{id}", std::process::id()))
}

/// The buyer's CDK wallet store. Its absence is the no-wallet gate.
fn wallet_store(home: &home::MaxplayerHome) -> PathBuf {
    home.wallet_dir.join("cdk-wallet.sqlite")
}

/// A from-scratch post request in `mode`, priced at `amount_sats`.
fn request(mode: PaymentMode, amount_sats: u64, seller_hex: &str) -> PostJobRequest {
    PostJobRequest {
        task: "say hello".to_owned(),
        output: "text/plain".to_owned(),
        amount_sats,
        seller_pubkey: Some(seller_hex.to_owned()),
        untargeted: false,
        deadline_unix: None,
        repo: None,
        branch: None,
        job: JobKind::FromScratch,
        requested_agent: None,
        requested_harness_family: None,
        requested_model: None,
        required_capabilities: Vec::new(),
        payment_mode: mode,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_free_post_succeeds_with_no_wallet_and_a_priced_one_on_the_same_home_refuses() {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("relay run");
    let relay_url = relay.url().await.to_string();

    let root = temp("home");
    let _ = std::fs::remove_dir_all(&root);
    let mut buyer_home = home::bootstrap(&root).expect("home");
    buyer_home.config.relay_url = relay_url.clone();
    // The mint is the LOOPBACK dead-mint stand-in — a URL string the free path never contacts, and
    // one that refuses the TCP connect instantly if anything does. Explicit rather than left to
    // `default_mint()`'s fallback: that fallback is a REAL network mint, so a control that reached
    // it would make this test both slow and dependent on someone else's uptime. `allow_real_mints`
    // must be on for the fence to admit a non-default URL; pointing it at 127.0.0.1:1 opts in to no
    // real money (the same stand-in `authorize_pay`'s own dead-mint test uses).
    buyer_home.config.accepted_mints = vec![DEAD_MINT.to_owned()];
    buyer_home.config.allow_real_mints = true;

    let seller_hex = Keys::generate().public_key().to_hex();

    // ── The gate, BEFORE ─────────────────────────────────────────────────────────────────────
    assert!(
        !wallet_store(&buyer_home).exists(),
        "the buyer must start with NO wallet store"
    );

    // ── FREE POST ────────────────────────────────────────────────────────────────────────────
    let outcome = job_lifecycle::post_job_async(&buyer_home, request(PaymentMode::None, 0, &seller_hex))
        .await
        .expect("a payment=none post at amount 0 must succeed with no wallet and no mint");
    assert_eq!(outcome.amount_sats, 0, "a free job is priced at zero");
    assert!(!outcome.job_id.is_empty(), "the offer must have a real event id");

    // ── The gate, AFTER ──────────────────────────────────────────────────────────────────────
    assert!(
        !wallet_store(&buyer_home).exists(),
        "a free post must not open a wallet — no wallet store may appear"
    );

    // ── The WIRE: read the published offer back off the relay ────────────────────────────────
    // Not the local outcome struct — the actual event a seller would see. `Sat` is stated by
    // absence, so a free offer has to state `none` explicitly or no seller can tell the difference.
    let reader = Client::new(Keys::generate());
    reader.add_relay(&relay_url).await.expect("add relay");
    reader.connect().await;
    reader
        .wait_for_connection(std::time::Duration::from_secs(5))
        .await;
    let events = reader
        .fetch_events(
            Filter::new().id(outcome.job_id.parse().expect("event id")),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("fetch the published offer");
    let offer = events.first().expect("the offer must be on the relay");
    let payment_tags: Vec<Vec<String>> = offer
        .tags
        .iter()
        .map(|tag| tag.clone().to_vec())
        .filter(|tag| tag.first().map(String::as_str) == Some("param"))
        .filter(|tag| tag.get(1).map(String::as_str) == Some("payment"))
        .collect();
    assert_eq!(
        payment_tags,
        vec![vec![
            "param".to_owned(),
            "payment".to_owned(),
            "none".to_owned()
        ]],
        "the published free offer must carry exactly one payment param stating none: {:?}",
        offer.tags
    );

    // ── PAID CONTROL — the same post, priced, on its OWN home ────────────────────────────────
    // What separates the two modes at post time is the WALLET, not a refusal: `post_job_async`'s
    // priced arm calls `open_wallet_async`, which creates `wallet/cdk-wallet.sqlite` on the spot,
    // and only THEN runs the dust guard. So the control is that the store APPEARS here and does
    // NOT appear on the free home — the free branch skipped the wallet open, and that open is the
    // one thing a buyer with no wallet could not have survived.
    //
    // The dust guard then refuses, because the mint is loopback-dead. That refusal is incidental to
    // the control (the store already exists by then) but it is asserted, because it is what keeps
    // this test off the network: allowed to fall back to `default_mint()`, the guard would query a
    // REAL mint over the internet — slow, flaky, and not something a test should reach for.
    //
    // Run on a separate home so the free home above stays pristine for its after-gate.
    let paid_root = temp("home-paid-control");
    let _ = std::fs::remove_dir_all(&paid_root);
    let mut paid_home = home::bootstrap(&paid_root).expect("paid home");
    paid_home.config.relay_url = relay_url.clone();
    paid_home.config.accepted_mints = vec![DEAD_MINT.to_owned()];
    paid_home.config.allow_real_mints = true;
    assert!(
        !wallet_store(&paid_home).exists(),
        "the paid control's home must also start with no wallet store"
    );
    let paid_refusal = job_lifecycle::post_job_async(&paid_home, request(PaymentMode::Sat, 7, &seller_hex))
        .await
        .expect_err("a priced post against a dead mint refuses at the dust guard");
    assert!(
        paid_refusal.to_string().contains("mint_unreachable"),
        "the priced arm must refuse at the mint, having already opened the wallet: {paid_refusal}"
    );
    assert!(
        wallet_store(&paid_home).exists(),
        "the PRICED branch opens a wallet BEFORE the dust guard, so its store must exist — this is \
         the control that makes the free home's missing store meaningful rather than incidental"
    );

    // ── The library's own mirror of the RPC refusal ───────────────────────────────────────────
    // `payment=none` above zero never reaches the relay. Asserted here as well as at the RPC so a
    // future caller that bypasses the RPC cannot post the contradiction either.
    let priced_free = job_lifecycle::post_job_async(&buyer_home, request(PaymentMode::None, 3, &seller_hex))
        .await
        .expect_err("payment=none at a non-zero amount must refuse");
    assert!(
        priced_free.to_string().contains("payment=none requires amount_sats = 0"),
        "the refusal must name the rule, got: {priced_free}"
    );

    // The free home's final state: still no wallet store, after a successful free post and a
    // refused free-at-a-price post.
    assert!(
        !wallet_store(&buyer_home).exists(),
        "no wallet store may exist on the free home after any of these posts"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&paid_root);
    drop(relay);
}
