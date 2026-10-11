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

/// Scaled lab timing; restores production timing (and clears state) when dropped.
struct Lab;
impl Lab {
    fn new(urgent_margin: u64) -> Self {
        backoff::lab_configure(Some(Timing {
            base: Duration::from_millis(300),
            cap: Duration::from_secs(2),
            retry_after_cap: Duration::from_secs(900),
            interval: Duration::from_millis(100),
            burst: Duration::from_millis(500),
            urgent_margin: Some(Duration::from_secs(urgent_margin)),
            loopback: true,
        }));
        // These tests call `coordinator::recover` directly; give it the CLI `recover`
        // semantics (wait for pacing) so an item is never skipped because a fast runner
        // left pacing debt from the fixture's own setup requests.
        backoff::set_wait_for_pacing(true);
        Lab
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
    let mut attempts = 0;
    while start.elapsed() < Duration::from_secs(3) {
        for _ in 0..4 {
            // Four items in one process, each retrying every 10 ms.
            let _ = transport::get(&m.url, "info").await;
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
    // Each deferred attempt still builds a TLS client (debug build: several ms), so count
    // attempts rather than assume a rate; they must far exceed what reached the mint.
    assert!(attempts >= 40, "fixture: callers retried hard ({attempts})");

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

/// A maker whose claim mint is backing off (900 s Retry-After) does not claim early, never marks
/// anything refused or releases anything, and still claims at the last safe moment before the
/// taker's refund locktime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_near_deadline_still_happens() {
    assert!(
        std::env::var("TRADE_LAB_SECONDS").is_ok(),
        "run with TRADE_LAB_SECONDS=1"
    );
    // Lab deadlines: long = issue + 48 s. Urgent from long - 30 s.
    let _lab = Lab::new(30);
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
    // The maker's claim mint (b) rate-limits once with a 900 s Retry-After, then recovers:
    // only our own backoff now stands between the maker and its claim.
    f.b.faults.rate_limit.store(true, SeqCst);
    f.b.faults.retry_after.store(900, SeqCst);
    assert!(transport::get(&f.b.url, "info").await.is_err());
    f.b.faults.rate_limit.store(false, SeqCst);
    let swaps = f.b.faults.swaps.load(SeqCst);
    let s = f.jm.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let long = s.quote.as_ref().unwrap().long;
    assert!(
        coordinator::now() + 30 < long,
        "fixture: before the last safe moment"
    );
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert_eq!(
        f.state(true, &id).await.as_deref(),
        Some("claiming"),
        "preimage learned on the healthy own mint; claim deferred, nothing refused"
    );
    assert_eq!(
        f.b.faults.swaps.load(SeqCst),
        swaps,
        "no claim sent during backoff"
    );
    assert_eq!(balance(&f.maker, &f.b.url).await, 0);
    assert!(
        backoff::lab_log()
            .iter()
            .any(|l| l.contains("would pass a claim's safe window")),
        "{:?}",
        backoff::lab_log()
    );
    // At the last safe moment the claim goes out despite the 900 s backoff.
    wait_past(long - 30).await;
    coordinator::recover(&f.maker, &f.jm, &f.mm).await.unwrap();
    assert!(
        coordinator::now() < long,
        "claimed before the taker could refund"
    );
    assert_eq!(f.state(true, &id).await.as_deref(), Some("complete"));
    assert_eq!(balance(&f.maker, &f.b.url).await, 24);
    assert!(
        backoff::lab_log()
            .iter()
            .any(|l| l.contains("last safe moment, trying now")),
        "{:?}",
        backoff::lab_log()
    );
}
