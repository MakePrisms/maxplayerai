//! Round 7 (final review of c5f06cf): L-a deadline-based maker advance budgets.
//! Own test binary: the lab budget override is process-global.
#![cfg(feature = "lab")]
mod support;
use maxplayer_trade::coordinator::{self, Swap};
use std::sync::atomic::Ordering::SeqCst;
use support::*;

/// Slow-but-live counterparty mint. Scaled from the review scenario (each CDK/HTTP call
/// ~13 s against a 50 s fixed claim slice): every request to B is delayed, refund work on
/// our own mint A is fast, and the claim needs more than the refund cap (the old fixed
/// per-phase slice) but less than the overall advance deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r7_slow_counterparty_claim_inherits_unused_refund_budget() {
    const OVERALL_MS: u64 = 30_000;
    const REFUND_MS: u64 = 4_000;
    const DELAY_MS: u64 = 1_200;
    let (f, id) = partial_claim_fixture().await;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    wait_past(s.quote.as_ref().unwrap().short + s.quote.as_ref().unwrap().margin).await;
    coordinator::lab_set_advance_budgets_ms(Some((OVERALL_MS, REFUND_MS)));
    f.b.faults.delay_ms.store(DELAY_MS, SeqCst);
    let before = f.b.faults.requests.load(SeqCst);
    let started = std::time::Instant::now();
    let result = coordinator::advance(&f.maker, &f.jm, &f.mm, &mut s).await;
    let elapsed = started.elapsed();
    let slow_requests = f.b.faults.requests.load(SeqCst) - before;
    f.b.faults.delay_ms.store(0, SeqCst);
    coordinator::lab_set_advance_budgets_ms(None);
    eprintln!("r7 advance: {elapsed:?}, {slow_requests} delayed counterparty requests, {result:?}");
    assert!(
        f.jm.get::<serde_json::Value>("attempt", &format!("{id}-claim"))
            .await
            .unwrap()
            .is_some(),
        "SAFETY: slow-but-live counterparty claim attempt must be journaled within the deadline"
    );
    assert_eq!(
        balance(&f.maker, &f.b.url).await,
        24,
        "SAFETY: claim on a slow-but-live counterparty mint must land when the refund was fast"
    );
    assert!(
        ["settling", "complete"].contains(&s.state.as_str()),
        "SAFETY: maker claim settled; state={}",
        s.state
    );
    assert_eq!(
        balance(&f.maker, &f.a.url).await,
        112,
        "SAFETY: own-mint remainder refunded first"
    );
    assert!(
        elapsed > std::time::Duration::from_millis(REFUND_MS),
        "fixture: claim must need more than the refund cap (old fixed slice); took {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(OVERALL_MS),
        "fixture: claim must fit the overall deadline; took {elapsed:?}"
    );
    result.unwrap();
}
