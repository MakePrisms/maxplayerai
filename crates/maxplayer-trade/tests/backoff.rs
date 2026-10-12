//! Per-mint backoff and pacing against loopback fake mints (lab: the timing is scaled down and
//! applied to loopback mints, which production exempts). Never contacts a real mint.
#![cfg(feature = "lab")]
mod support;
use maxplayer_trade::{
    backoff::{self, Timing},
    coordinator::{self, Swap},
    transport,
};
use std::{
    sync::atomic::Ordering::SeqCst,
    time::{Duration, Instant},
};
use support::*;

/// Backoff state, timing and the pacing mode are process-global: one lab test at a time even
/// under a parallel `cargo test`.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Scaled lab timing; restores production timing (and clears state) when dropped.
struct Lab(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
impl Lab {
    fn new(urgent_margin: u64) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        backoff::lab_configure(Some(Timing {
            base: Duration::from_millis(300),
            cap: Duration::from_secs(2),
            retry_after_cap: Duration::from_secs(900),
            interval: Duration::from_millis(100),
            burst: Duration::from_millis(500),
            urgent_margin: Some(Duration::from_secs(urgent_margin)),
            claim_retry: Duration::from_secs(2),
            final_retry: Duration::from_secs(1),
            unknown_skew: Duration::ZERO,
            loopback: true,
        }));
        // These tests call `coordinator::recover` directly; give it the CLI `recover`
        // semantics (wait for pacing) so an item is never skipped because a fast runner
        // left pacing debt from the fixture's own setup requests.
        backoff::set_wait_for_pacing(true);
        Lab(serial)
    }
}
impl Drop for Lab {
    fn drop(&mut self) {
        backoff::set_wait_for_pacing(false);
        backoff::lab_configure(None);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_is_honoured_then_success_resets() {
    let _lab = Lab::new(1);
    let m = MintFixture::start(0).await;
    m.faults.rate_limit.store(true, SeqCst);
    m.faults.retry_after.store(2, SeqCst);
    assert!(transport::get(&m.url, "info").await.is_err());
    assert_eq!(m.faults.requests.load(SeqCst), 1);
    let left = backoff::remaining(&m.url).expect("429 starts a backoff");
    assert!(
        left > Duration::from_millis(1500),
        "Retry-After 2 s beats the 300 ms base: {left:?}"
    );
    // The mint recovers at once, but nothing is sent before Retry-After elapses.
    m.faults.rate_limit.store(false, SeqCst);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        let e = transport::get(&m.url, "info").await.unwrap_err();
        assert!(backoff::deferred(&e), "deferred locally: {e:#}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        m.faults.requests.load(SeqCst),
        1,
        "SAFETY: no request during Retry-After"
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    transport::get(&m.url, "info").await.unwrap();
    assert!(
        backoff::remaining(&m.url).is_none(),
        "success resets the backoff"
    );
    // Reset means the next failure starts again at the base delay, not the doubled one.
    m.faults.rate_limit.store(true, SeqCst);
    m.faults.retry_after.store(0, SeqCst);
    assert!(transport::get(&m.url, "info").await.is_err());
    assert!(backoff::remaining(&m.url).unwrap() <= Duration::from_millis(375));
    let log = backoff::lab_log();
    let url = &m.url;
    assert_eq!(
        log.iter().filter(|l| l.contains("(429)")).count(),
        2,
        "one line per state change, not per attempt: {log:?}"
    );
    assert!(
        log.contains(&format!("mint {url} backing off 2s (429)")),
        "{log:?}"
    );
    assert!(
        log.contains(&format!("mint {url} answered; backoff cleared")),
        "{log:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_connections_back_off_exponentially() {
    let _lab = Lab::new(1);
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    assert!(transport::get(&url, "info").await.is_err());
    let first = backoff::remaining(&url).expect("refused connection starts a backoff");
    assert!(first <= Duration::from_millis(375));
    let e = transport::get(&url, "info").await.unwrap_err();
    assert!(backoff::deferred(&e), "{e:#}");
    tokio::time::sleep(first).await;
    assert!(transport::get(&url, "info").await.is_err());
    let second = backoff::remaining(&url).unwrap();
    assert!(
        second > Duration::from_millis(500),
        "doubles: {first:?} -> {second:?}"
    );
    assert_eq!(
        backoff::lab_log(),
        [format!("mint {url} backing off 1s (refused)")],
        "logged once for the state change"
    );
}

/// The live incident: a 3 s recovery tick polling a 429ing mint. Count real requests over a
/// simulated window with every caller retrying as fast as it can.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_request_storm() {
    let _lab = Lab::new(1);
    let m = MintFixture::start(0).await;
    m.faults.rate_limit.store(true, SeqCst);
    let start = Instant::now();
    let (mut attempts, mut deferred) = (0, 0);
    while start.elapsed() < Duration::from_secs(3) {
        for _ in 0..4 {
            // Four items in one process, each retrying every 10 ms.
            if let Err(e) = transport::get(&m.url, "info").await
                && backoff::deferred(&e)
            {
                deferred += 1;
            }
            attempts += 1;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Backoff 0.3, 0.6, 1.2, 2 (cap) s (+ <= 25 % jitter): at most 5 requests in 3 s.
    let sent = m.faults.requests.load(SeqCst);
    assert!(
        sent <= 5,
        "{sent} requests reached a 429ing mint ({attempts} attempts)"
    );
    // Each deferred attempt still builds a TLS client (debug build: ~25 ms on a 4-vCPU box),
    // so the attempt rate is host-dependent: assert relative to what reached the mint (review
    // r2, N-1). Callers kept retrying, and most attempts were stopped locally.
    assert!(sent >= 1, "fixture: the 429 reached the mint once");
    assert!(
        deferred >= 2 * sent,
        "fixture: callers retried hard ({attempts} attempts, {deferred} deferred, {sent} sent)"
    );

    // Healthy mint, no backoff: pacing bounds a tight polling loop to ~1 request / interval.
    m.faults.rate_limit.store(false, SeqCst);
    tokio::time::sleep(Duration::from_millis(2600)).await;
    transport::get(&m.url, "info").await.unwrap(); // clears the backoff
    let before = m.faults.requests.load(SeqCst);
    let start = Instant::now();
    let mut passes = 0;
    while start.elapsed() < Duration::from_secs(3) {
        if backoff::ready("poller", &[&m.url], None).await {
            for _ in 0..3 {
                transport::get(&m.url, "info").await.unwrap();
            }
            passes += 1;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let sent = m.faults.requests.load(SeqCst) - before;
    // 3 s / 100 ms = 30, plus the 500 ms burst (5) and one pass's tail (3).
    assert!(
        sent <= 40,
        "paced: {sent} requests in 3 s ({passes} passes)"
    );
    assert!(sent >= 20, "pacing must not stall polling: {sent}");
}

/// H1 + serve semantics. A maker whose claim mint answers 429 with `Retry-After: 900` on every
/// request, from the moment the preimage is known until inside the final stretch, and whose
/// clock runs 3 s ahead of ours:
/// - (serve) a paced item without a possible claim is skipped; the claim item never is;
/// - the claim mint sees one request per claim round (2 s lab, 30 s production), never idle for
///   the backoff, never hammered; one per 1 s round in the final stretch;
/// - nothing is refused or released; once the mint answers, the claim lands within about one
///   round, before `long` on the MINT's clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maker_claim_retried_through_rate_limit_before_long() {
    assert!(
        std::env::var("TRADE_LAB_SECONDS").is_ok(),
        "run with TRADE_LAB_SECONDS=1"
    );
    // Lab deadlines: long = issue + 48 s. Final stretch: 8 s (mint clock) before long.
    let _lab = Lab::new(8);
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    for _ in 0..30 {
        f.step(true).await;
        f.step(false).await;
        if f.state(false, &id).await.as_deref() == Some("claimed") {
            break;
        }
    }
    assert_eq!(f.state(false, &id).await.as_deref(), Some("claimed"));
    assert_eq!(f.state(true, &id).await.as_deref(), Some("second_locked"));
    let long =
        f.jm.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .quote
            .unwrap()
            .long;
    // The claim mint's clock runs 3 s ahead; the maker sees it in `info.time`.
    let skew = 3;
    f.b.faults.clock_offset.store(skew, SeqCst);
    maxplayer_trade::wallet::action_time(&f.b.url)
        .await
        .unwrap();
    assert!(backoff::mint_now(&f.b.url) >= coordinator::now() + skew);
    assert!(
        coordinator::now() + 20 < long,
        "fixture: time before the final stretch"
    );
    // Serve semantics from here on: a paced item is skipped, not waited for.
    backoff::set_wait_for_pacing(false);
    let own_debt = || {
        for _ in 0..40 {
            backoff::admit(&f.a.url, true).unwrap();
        }
    };
    // (a) No claim possible yet (preimage unknown): the paced item is skipped this tick.
    own_debt();
    let a_before = f.a.faults.requests.load(SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(f.state(true, &id).await.as_deref(), Some("second_locked"));
    assert_eq!(
        f.a.faults.requests.load(SeqCst),
        a_before,
        "paced item skipped in serve: no request"
    );
    // The maker learns the preimage (as the taker's `claimed` message delivers it) while the
    // claim mint starts answering 429 with a 900 s Retry-After to everything.
    let pre =
        f.jt.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .preimage;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    s.preimage = pre;
    s.state = "claiming".into();
    f.jm.put("swap", &id, &s).await.unwrap();
    let swaps = f.b.faults.swaps.load(SeqCst);
    f.b.faults.rate_limit.store(true, SeqCst);
    f.b.faults.retry_after.store(900, SeqCst);
    // One HTTP answer carries the 900 s Retry-After (CDK-path 429s carry none and only
    // extend it); every later request is refused the same way.
    assert!(transport::get(&f.b.url, "info").await.is_err());
    assert!(backoff::remaining(&f.b.url).unwrap() > Duration::from_secs(800));
    // (b) The same pacing debt never skips an item whose claim is possible.
    own_debt();
    let b_before = f.b.faults.requests.load(SeqCst);
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert!(
        f.b.faults.requests.load(SeqCst) > b_before,
        "claim item ran despite pacing"
    );
    assert!(backoff::remaining(&f.b.url).unwrap() > Duration::from_secs(800));
    // Serve ticks every 250 ms while the mint keeps refusing, into the final stretch.
    let final_at = long - skew - 8; // local time of mint-clock `long - 8`
    let refuse_until = long - skew - 5;
    let start = coordinator::now();
    let mut at_final = None;
    while coordinator::now() < refuse_until {
        if at_final.is_none() && coordinator::now() >= final_at {
            at_final = Some(f.b.faults.requests.load(SeqCst));
        }
        coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
        assert_eq!(
            f.state(true, &id).await.as_deref(),
            Some("claiming"),
            "SAFETY: nothing refused or released while the mint refuses"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let at_final = at_final.unwrap();
    let rounds = at_final - b_before;
    let secs = final_at - start;
    // Lower bound with slack (review r2, N-1): each serve tick's pass on a slow debug host
    // stretches a 2 s round; idle-for-the-backoff would be ~0 rounds.
    assert!(
        rounds >= secs / 3,
        "claim rounds every 2 s, not idle for the 900 s backoff: {rounds} in {secs} s"
    );
    assert!(
        rounds <= secs / 2 + 3,
        "one request per round, not every tick: {rounds} in {secs} s"
    );
    let finals = f.b.faults.requests.load(SeqCst) - at_final;
    eprintln!("claim rounds: {rounds} in {secs} s; final stretch: {finals}");
    assert!(
        (1..=6).contains(&finals),
        "final stretch: one request per 1 s round: {finals}"
    );
    assert_eq!(
        f.b.faults.swaps.load(SeqCst),
        swaps,
        "every claim request refused while rate limited"
    );
    // The mint answers again (still inside its 900 s Retry-After): the claim lands at once.
    f.b.faults.rate_limit.store(false, SeqCst);
    let recovered = Instant::now();
    while !matches!(
        f.state(true, &id).await.as_deref(),
        Some("settling" | "complete")
    ) {
        assert!(
            recovered.elapsed() < Duration::from_secs(4),
            "claim must land within about one round once the mint answers"
        );
        coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        coordinator::now() + skew < long,
        "claimed before long on the mint's clock"
    );
    assert_eq!(balance(&f.maker, &f.b.url).await, 24);
    let log = backoff::lab_log();
    assert!(
        log.iter().any(|l| l.contains("a claim to it is possible")),
        "{log:?}"
    );
    assert!(log.iter().any(|l| l.contains("final stretch")), "{log:?}");
}

/// M-A (review r2): the claim mint answers restore and NUT-07 but answers every swap POST with
/// 429 + `Retry-After: 900` (a stricter limit on transaction endpoints). The answers must not
/// reset the claim round: one claim attempt per 2 s lab round (30 s production), not one per
/// serve tick. Then the mint accepts swaps again and the claim lands within about one round.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_only_rate_limit_keeps_claim_rounds() {
    assert!(
        std::env::var("TRADE_LAB_SECONDS").is_ok(),
        "run with TRADE_LAB_SECONDS=1"
    );
    // Final stretch 1 s: the whole window below runs at the 2 s claim cadence.
    let _lab = Lab::new(1);
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    for _ in 0..30 {
        f.step(true).await;
        f.step(false).await;
        if f.state(false, &id).await.as_deref() == Some("claimed") {
            break;
        }
    }
    assert_eq!(f.state(false, &id).await.as_deref(), Some("claimed"));
    assert_eq!(f.state(true, &id).await.as_deref(), Some("second_locked"));
    let long =
        f.jm.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .quote
            .unwrap()
            .long;
    let pre =
        f.jt.get::<Swap>("swap", &id)
            .await
            .unwrap()
            .unwrap()
            .preimage;
    let mut s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    s.preimage = pre;
    s.state = "claiming".into();
    f.jm.put("swap", &id, &s).await.unwrap();
    let window = 8;
    assert!(
        coordinator::now() + window + 4 < long,
        "fixture: the window ends before long"
    );
    backoff::set_wait_for_pacing(false);
    f.b.faults.retry_after.store(900, SeqCst);
    f.b.faults.rate_limit_swap.store(true, SeqCst);
    let swaps = f.b.faults.swaps.load(SeqCst);
    let requests = f.b.faults.requests.load(SeqCst);
    // Serve ticks every 250 ms for `window` seconds.
    let start = Instant::now();
    let mut ticks = 0;
    while start.elapsed() < Duration::from_secs(window) {
        coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
        ticks += 1;
        assert_eq!(
            f.state(true, &id).await.as_deref(),
            Some("claiming"),
            "SAFETY: nothing refused or released while swaps are refused"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let secs = start.elapsed().as_secs();
    let limited = f.b.faults.limited_swaps.load(SeqCst);
    let sent = f.b.faults.requests.load(SeqCst) - requests;
    eprintln!("swap-only 429: {limited} swaps, {sent} requests in {secs} s ({ticks} ticks)");
    assert!(
        backoff::remaining(&f.b.url).unwrap() > Duration::from_secs(800),
        "the swap 429 started the 900 s backoff"
    );
    // The first attempt (no backoff yet) + one per 2 s round. Before the fix: one per tick.
    assert!(
        limited <= secs / 2 + 2,
        "one swap per claim round, not per tick: {limited} swaps / {sent} requests in {secs} s ({ticks} ticks)"
    );
    assert!(
        limited >= 2,
        "never idle for the 900 s Retry-After: {limited} swaps in {secs} s"
    );
    assert!(
        sent <= 6 * (secs / 2 + 2),
        "a round's restore/NUT-07 answers do not open more rounds: {sent} requests in {secs} s"
    );
    assert_eq!(f.b.faults.swaps.load(SeqCst), swaps, "no swap processed");
    let log = backoff::lab_log();
    assert_eq!(
        log.iter()
            .filter(|l| l.contains("a claim to it is possible"))
            .count(),
        1,
        "warned once, not per round: {log:?}"
    );
    // Swaps are accepted again (still inside the 900 s Retry-After): the claim lands.
    f.b.faults.rate_limit_swap.store(false, SeqCst);
    let recovered = Instant::now();
    while !matches!(
        f.state(true, &id).await.as_deref(),
        Some("settling" | "complete")
    ) {
        assert!(
            recovered.elapsed() < Duration::from_secs(4),
            "claim must land within about one round once swaps are accepted"
        );
        coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(coordinator::now() < long, "claimed before long");
    assert_eq!(balance(&f.maker, &f.b.url).await, 24);
}
