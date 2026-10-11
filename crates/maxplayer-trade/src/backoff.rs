//! Per-mint backoff and request pacing, shared by every item in one process.
//!
//! Why: on 2026-10-11 (binary from `bc41510`) one `serve` sent about 14 NUT-07 checkstates a
//! minute to a mint that was answering HTTP 429, and four serve/take processes kept retrying two
//! mints that had blocked this host. Every mint request this crate sends now passes [`admit`]
//! first and reports its outcome back (HTTP status, connection error, timeout).
//!
//! **Backoff** (per mint URL, per process, shared by all items):
//! - trigger: HTTP 429 or 503 (an integer `Retry-After` is honoured, capped at 15 min), a refused
//!   or reset connection, or a timeout (HTTP request, CDK wallet call, or the `nostr://` bound);
//! - delay: 5 s, doubling per consecutive failure, plus 0–25 % jitter, capped at 10 min; a longer
//!   `Retry-After` wins (up to 15 min);
//! - while it runs nothing in the process contacts that mint: the request fails locally with
//!   [`Deferred`] before anything is sent. That is the same as a request that got no answer, which
//!   every money path already treats as ambiguous (journal kept, retried later, never `refused`);
//! - any other HTTP answer (a NUT error included) or a `nostr://` reply clears it.
//!
//! **Pacing** (no backoff active): every request to a mint adds 6 s of debt; a recovery item that
//! needs the mint starts only while that debt is at most 30 s ahead of now. An admitted item runs
//! its whole sequence (restore, NUT-07, swap are never split by pacing). Long run: at most one
//! request per 6 s per mint per process (10/min), after a burst of at most 5 requests plus one
//! item's sequence. `serve` skips a paced item until a later tick; `recover` (one bounded pass)
//! waits for it. Items take turns: the one served longest ago goes first.
//!
//! **Deadline safety:** a claim phase runs inside [`with_deadline`] (maker: the taker's refund
//! locktime `long`; taker: the claim send deadline `short - cutoff`). From `deadline - margin`
//! (margin: two abandonment graces, 120 s over HTTP) its requests ignore backoff and pacing: the
//! claim is tried at the last safe moment however long the backoff. Earlier, when the backoff
//! would run past that moment, that is logged once. Backoff never reorders, skips, releases or
//! refuses anything: it only delays a request, exactly as an outage would.
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, MutexGuard, RwLock},
    time::{Duration, Instant},
};

/// Tunables. Production values are [`Timing::PRODUCTION`]; lab builds may scale them.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// First backoff delay.
    pub base: Duration,
    /// Largest computed (exponential + jitter) delay.
    pub cap: Duration,
    /// Largest honoured `Retry-After`.
    pub retry_after_cap: Duration,
    /// Pacing debt added per request.
    pub interval: Duration,
    /// Debt allowed ahead of now before a paced item must wait.
    pub burst: Duration,
    /// Urgency margin before a claim deadline; `None` = two abandonment graces for the mint.
    pub urgent_margin: Option<Duration>,
    /// Apply backoff and pacing to loopback (test) mints too.
    pub loopback: bool,
}
impl Timing {
    pub const PRODUCTION: Timing = Timing {
        base: Duration::from_secs(5),
        cap: Duration::from_secs(600),
        retry_after_cap: Duration::from_secs(900),
        interval: Duration::from_secs(6),
        burst: Duration::from_secs(30),
        urgent_margin: None,
        loopback: false,
    };
}

/// Why a mint is being backed off. The label is what the log shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    RateLimited,
    Unavailable,
    Refused,
    Timeout,
}
impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::RateLimited => "429",
            Reason::Unavailable => "503",
            Reason::Refused => "refused",
            Reason::Timeout => "timeout",
        }
    }
}

/// The outcome of one request, as far as mint health goes.
#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    /// The mint answered (any status other than 429/503, NUT errors included).
    Answered,
    Failed(Reason, Option<Duration>),
}

/// A request that was NOT sent because its mint is backing off. Ambiguous like any unanswered
/// request: callers keep their journal and retry; nothing may treat it as a refusal.
#[derive(Debug)]
pub struct Deferred {
    pub mint: String,
    pub reason: Reason,
    pub remaining: Duration,
}
impl std::fmt::Display for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mint {} backing off ({}), {}s left; request not sent",
            self.mint,
            self.reason.label(),
            self.remaining.as_secs()
        )
    }
}
impl std::error::Error for Deferred {}

/// Whether an error is (or wraps) a [`Deferred`]: nothing was sent.
pub fn deferred(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Deferred>())
}

/// A claim deadline, as seen by one request.
#[derive(Clone, Copy, Debug)]
struct Urgency {
    deadline: u64,
    now_unix: u64,
    margin: u64,
}
impl Urgency {
    /// The bypass window is `[deadline - margin, deadline + margin)`. After it, normal backoff
    /// applies again (still retried, never dropped), so a stuck claim cannot hammer forever.
    fn over(&self) -> bool {
        self.now_unix >= self.deadline.saturating_add(self.margin)
    }
    fn due(&self) -> bool {
        !self.over() && self.now_unix.saturating_add(self.margin) >= self.deadline
    }
}

#[derive(Default)]
struct MintState {
    failures: u32,
    until: Option<Instant>,
    reason: Option<Reason>,
    debt: Option<Instant>,
    /// (deadline, stage) of the last deadline warning: 1 = "would pass", 2 = "trying now".
    warned: Option<(u64, u8)>,
}

#[derive(Default)]
struct Book {
    mints: HashMap<String, MintState>,
    turns: HashMap<String, Instant>,
}

fn key(mint: &str) -> String {
    mint.trim_end_matches('/').to_ascii_lowercase()
}

impl Book {
    fn admit(
        &mut self,
        t: &Timing,
        mint: &str,
        now: Instant,
        charge: bool,
        urgent: Option<Urgency>,
        log: &mut Vec<String>,
    ) -> Result<(), Deferred> {
        let m = self.mints.entry(key(mint)).or_default();
        if let (Some(until), Some(reason)) = (m.until, m.reason) {
            if now < until {
                let remaining = until - now;
                let deferred = || Deferred {
                    mint: mint.to_owned(),
                    reason,
                    remaining,
                };
                let Some(u) = urgent.filter(|u| !u.over()) else {
                    return Err(deferred());
                };
                let last_safe = u.deadline.saturating_sub(u.margin);
                if u.now_unix < last_safe {
                    if u.now_unix.saturating_add(remaining.as_secs()) >= last_safe
                        && m.warned < Some((u.deadline, 1))
                    {
                        m.warned = Some((u.deadline, 1));
                        log.push(format!(
                            "mint {mint} backoff ({}, {}s left) would pass a claim's safe window: the claim will be tried anyway at the last safe moment (unix {last_safe}, deadline {})",
                            reason.label(),
                            remaining.as_secs(),
                            u.deadline
                        ));
                    }
                    return Err(deferred());
                }
                if m.warned < Some((u.deadline, 2)) {
                    m.warned = Some((u.deadline, 2));
                    log.push(format!(
                        "mint {mint} backing off ({}) but a claim must reach it before unix {}: last safe moment, trying now",
                        reason.label(),
                        u.deadline
                    ));
                }
            }
        }
        if charge {
            let from = m.debt.filter(|d| *d > now).unwrap_or(now);
            m.debt = Some(from + t.interval);
        }
        Ok(())
    }

    /// `jitter` in [0, 1]: the fraction of the 25 % jitter band to add.
    fn record(
        &mut self,
        t: &Timing,
        mint: &str,
        now: Instant,
        outcome: Outcome,
        jitter: f64,
        log: &mut Vec<String>,
    ) {
        let m = self.mints.entry(key(mint)).or_default();
        match outcome {
            Outcome::Answered => {
                if m.reason.is_some() {
                    log.push(format!("mint {mint} answered; backoff cleared"));
                }
                m.failures = 0;
                m.until = None;
                m.reason = None;
                m.warned = None;
            }
            Outcome::Failed(reason, retry_after) => {
                m.failures = m.failures.saturating_add(1);
                let factor = 1u32.checked_shl(m.failures - 1).unwrap_or(u32::MAX);
                let exp = t.base.saturating_mul(factor).min(t.cap);
                let jittered = (exp + exp.mul_f64(jitter.clamp(0.0, 1.0) * 0.25)).min(t.cap);
                let delay =
                    jittered.max(retry_after.map_or(Duration::ZERO, |r| r.min(t.retry_after_cap)));
                let until = now + delay;
                m.until = Some(m.until.map_or(until, |u| u.max(until)));
                if m.reason != Some(reason) {
                    log.push(format!(
                        "mint {mint} backing off {}s ({})",
                        delay.as_secs_f64().ceil() as u64,
                        reason.label()
                    ));
                }
                m.reason = Some(reason);
            }
        }
    }

    /// How long a paced item touching `mints` must wait before it may start.
    fn wait(&self, t: &Timing, mints: &[&str], now: Instant) -> Duration {
        mints
            .iter()
            .filter_map(|mint| self.mints.get(&key(mint))?.debt)
            .map(|debt| debt.saturating_duration_since(now + t.burst))
            .max()
            .unwrap_or_default()
    }

    fn remaining(&self, mint: &str, now: Instant) -> Option<Duration> {
        let until = self.mints.get(&key(mint))?.until?;
        (until > now).then(|| until - now)
    }
}

static BOOK: LazyLock<Mutex<Book>> = LazyLock::new(Default::default);
static TIMING: RwLock<Timing> = RwLock::new(Timing::PRODUCTION);
static WAIT_FOR_PACING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(feature = "lab")]
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

tokio::task_local! {
    static DEADLINE: u64;
}

fn book() -> MutexGuard<'static, Book> {
    BOOK.lock().unwrap_or_else(|e| e.into_inner())
}
fn timing() -> Timing {
    *TIMING.read().unwrap_or_else(|e| e.into_inner())
}
fn emit(lines: Vec<String>) {
    for line in lines {
        eprintln!("{line}");
        #[cfg(feature = "lab")]
        LOG.lock().unwrap_or_else(|e| e.into_inner()).push(line);
    }
}
fn exempt(mint: &str) -> bool {
    crate::transport::loopback_only(mint) && !timing().loopback
}

/// Seconds before a claim deadline from which backoff and pacing no longer delay its requests.
pub fn margin(mint: &str) -> u64 {
    timing()
        .urgent_margin
        .map(|m| m.as_secs())
        .unwrap_or_else(|| 2 * crate::transport::abandon_grace_seconds(mint))
}

/// Run `f` as a claim phase that must reach its mint before `deadline` (unix seconds).
pub async fn with_deadline<F: std::future::Future>(deadline: u64, f: F) -> F::Output {
    DEADLINE.scope(deadline, f).await
}

fn urgency(mint: &str) -> Option<Urgency> {
    let deadline = DEADLINE.try_with(|d| *d).ok()?;
    Some(Urgency {
        deadline,
        now_unix: crate::coordinator::now(),
        margin: margin(mint),
    })
}

/// `recover` waits for pacing; `serve` (the default) skips a paced item until a later tick.
pub fn set_wait_for_pacing(wait: bool) {
    WAIT_FOR_PACING.store(wait, std::sync::atomic::Ordering::SeqCst);
}

/// Gate one request to `mint`. `charge` adds pacing debt (every real request does; CDK wallet
/// calls, often served from cache, do not).
pub fn admit(mint: &str, charge: bool) -> Result<(), Deferred> {
    if exempt(mint) {
        return Ok(());
    }
    let mut log = vec![];
    let result = book().admit(
        &timing(),
        mint,
        Instant::now(),
        charge,
        urgency(mint),
        &mut log,
    );
    emit(log);
    result
}

/// Report one request's outcome.
pub fn record(mint: &str, outcome: Outcome) {
    if exempt(mint) {
        return;
    }
    let jitter = f64::from(uuid::Uuid::new_v4().as_bytes()[0]) / 255.0;
    let mut log = vec![];
    book().record(&timing(), mint, Instant::now(), outcome, jitter, &mut log);
    emit(log);
}

/// Report an HTTP status.
pub fn record_status(mint: &str, status: u16, retry_after: Option<Duration>) {
    record(
        mint,
        match status {
            429 => Outcome::Failed(Reason::RateLimited, retry_after),
            503 => Outcome::Failed(Reason::Unavailable, retry_after),
            _ => Outcome::Answered,
        },
    );
}

fn classify_reqwest(e: &reqwest::Error) -> Option<Reason> {
    if e.is_timeout() {
        return Some(Reason::Timeout);
    }
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            match io.kind() {
                ConnectionRefused | ConnectionReset | ConnectionAborted => {
                    return Some(Reason::Refused);
                }
                TimedOut => return Some(Reason::Timeout),
                _ => {}
            }
        }
        source = s.source();
    }
    e.is_connect().then_some(Reason::Refused)
}

/// Report a failed HTTP request (no response).
pub fn record_reqwest(mint: &str, e: &reqwest::Error) {
    if let Some(reason) = classify_reqwest(e) {
        record(mint, Outcome::Failed(reason, None));
    }
}

/// Report a CDK (wallet or `nostr://` connector) failure. Successes are not reported here: a CDK
/// call may be answered from its cache without reaching the mint.
pub fn record_cdk(mint: &str, e: &cdk::Error) {
    let reason = match e {
        cdk::Error::HttpError(Some(429), _) => Reason::RateLimited,
        cdk::Error::HttpError(Some(503), _) => Reason::Unavailable,
        cdk::Error::Timeout => Reason::Timeout,
        cdk::Error::HttpError(None, msg) if msg.contains("error sending request") => {
            Reason::Refused
        }
        _ => return,
    };
    record(mint, Outcome::Failed(reason, None));
}

fn retry_after(r: &reqwest::Response) -> Option<Duration> {
    r.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Send one HTTP request to `mint` through the gate, recording its outcome.
pub async fn send(
    mint: &str,
    request: reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::Response> {
    admit(mint, true)?;
    match request.send().await {
        Ok(r) => {
            record_status(mint, r.status().as_u16(), retry_after(&r));
            Ok(r)
        }
        Err(e) => {
            record_reqwest(mint, &e);
            Err(e.into())
        }
    }
}

/// Pacing gate for one recovery item touching `mints`. True: run it now. False (serve): skip it
/// this tick. `recover` waits instead of returning false. An item whose claim deadline is within
/// [`margin`] is never paced.
pub async fn ready(item: &str, mints: &[&str], deadline: Option<u64>) -> bool {
    let mints: Vec<&str> = mints.iter().copied().filter(|m| !exempt(m)).collect();
    let urgent = deadline.is_some_and(|deadline| {
        mints.iter().any(|m| {
            Urgency {
                deadline,
                now_unix: crate::coordinator::now(),
                margin: margin(m),
            }
            .due()
        })
    });
    loop {
        let wait = if urgent {
            Duration::ZERO
        } else {
            book().wait(&timing(), &mints, Instant::now())
        };
        if wait.is_zero() {
            book().turns.insert(item.to_owned(), Instant::now());
            return true;
        }
        if !WAIT_FOR_PACING.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        tokio::time::sleep(wait).await;
    }
}

/// Order items so the one served longest ago (or never) goes first. Each item's own steps are
/// untouched; only independent items take turns.
pub fn order<T>(items: &mut [T], id: impl Fn(&T) -> &str) {
    let b = book();
    items.sort_by_key(|i| b.turns.get(id(i)).copied());
}

/// Time left in `mint`'s backoff, if any.
pub fn remaining(mint: &str) -> Option<Duration> {
    book().remaining(mint, Instant::now())
}

/// Lab-only: scale timing (and apply it to loopback mints), or restore production with `None`.
/// Clears all backoff/pacing state and the recorded log either way.
#[cfg(feature = "lab")]
pub fn lab_configure(t: Option<Timing>) {
    *TIMING.write().unwrap_or_else(|e| e.into_inner()) = t.unwrap_or(Timing::PRODUCTION);
    *book() = Book::default();
    LOG.lock().unwrap_or_else(|e| e.into_inner()).clear();
}
/// Lab-only: the backoff lines logged since the last [`lab_configure`].
#[cfg(feature = "lab")]
pub fn lab_log() -> Vec<String> {
    LOG.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    const M: &str = "https://mint.example";
    const T: Timing = Timing::PRODUCTION;

    fn fail(b: &mut Book, now: Instant, r: Reason, ra: Option<u64>, log: &mut Vec<String>) {
        b.record(
            &T,
            M,
            now,
            Outcome::Failed(r, ra.map(Duration::from_secs)),
            1.0,
            log,
        );
    }

    #[test]
    fn exponential_with_jitter_and_cap_then_reset() {
        let mut b = Book::default();
        let now = Instant::now();
        let mut log = vec![];
        let mut seen = vec![];
        for jitter in [0.0, 1.0] {
            for _ in 0..12 {
                b.record(
                    &T,
                    M,
                    now,
                    Outcome::Failed(Reason::Timeout, None),
                    jitter,
                    &mut log,
                );
                let m = b.mints.get_mut(&key(M)).unwrap();
                seen.push(m.until.unwrap() - now);
                m.until = None; // the next failure is a fresh attempt after expiry
            }
            b.record(&T, M, now, Outcome::Answered, 0.0, &mut log);
        }
        let secs: Vec<u64> = seen.iter().map(|d| d.as_secs()).collect();
        assert_eq!(
            &secs[..12],
            &[5, 10, 20, 40, 80, 160, 320, 600, 600, 600, 600, 600]
        );
        // Full jitter: +25 %, never past the 10 min cap; success reset back to 5 s.
        assert_eq!(
            &secs[12..],
            &[6, 12, 25, 50, 100, 200, 400, 600, 600, 600, 600, 600]
        );
        assert!(seen.iter().all(|d| *d <= T.cap));
    }

    #[test]
    fn retry_after_honoured_and_capped() {
        let mut b = Book::default();
        let now = Instant::now();
        let mut log = vec![];
        fail(&mut b, now, Reason::RateLimited, Some(120), &mut log);
        assert_eq!(
            b.remaining(M, now).unwrap().as_secs(),
            120,
            "Retry-After wins"
        );
        b.record(&T, M, now, Outcome::Answered, 0.0, &mut log);
        fail(&mut b, now, Reason::RateLimited, Some(86_400), &mut log);
        assert_eq!(
            b.remaining(M, now).unwrap().as_secs(),
            900,
            "capped at 15 min"
        );
        b.record(&T, M, now, Outcome::Answered, 0.0, &mut log);
        fail(&mut b, now, Reason::Unavailable, Some(1), &mut log);
        assert_eq!(
            b.remaining(M, now).unwrap().as_millis(),
            6250,
            "a short Retry-After never undercuts the backoff"
        );
    }

    #[test]
    fn deferred_while_backing_off_and_logged_once_per_state_change() {
        let mut b = Book::default();
        let now = Instant::now();
        let mut log = vec![];
        assert!(b.admit(&T, M, now, true, None, &mut log).is_ok());
        fail(&mut b, now, Reason::RateLimited, None, &mut log);
        for i in 0..50 {
            let at = now + Duration::from_millis(100 * i);
            assert!(b.admit(&T, M, at, true, None, &mut log).is_err());
        }
        // Repeated same-reason failures extend silently; a new reason or clearing logs again.
        for _ in 0..5 {
            fail(&mut b, now, Reason::RateLimited, None, &mut log);
        }
        fail(&mut b, now, Reason::Refused, None, &mut log);
        b.record(&T, M, now, Outcome::Answered, 0.0, &mut log);
        assert!(b.admit(&T, M, now, true, None, &mut log).is_ok());
        assert_eq!(
            log,
            [
                "mint https://mint.example backing off 7s (429)",
                "mint https://mint.example backing off 400s (refused)",
                "mint https://mint.example answered; backoff cleared",
            ]
        );
    }

    #[test]
    fn claim_tried_at_last_safe_moment_despite_backoff() {
        let mut b = Book::default();
        let now = Instant::now();
        let mut log = vec![];
        fail(&mut b, now, Reason::RateLimited, Some(900), &mut log);
        let deadline = 10_000;
        let at = |unix| Urgency {
            deadline,
            now_unix: unix,
            margin: 120,
        };
        // Far from the deadline: deferred; the backoff ends long before the last safe moment.
        assert!(
            b.admit(&T, M, now, true, Some(at(5_000)), &mut log)
                .is_err()
        );
        assert_eq!(log.len(), 1, "no deadline warning yet");
        // The 900 s backoff would run past 9_880: warned once, still deferred until then.
        assert!(
            b.admit(&T, M, now, true, Some(at(9_000)), &mut log)
                .is_err()
        );
        assert!(
            b.admit(&T, M, now, true, Some(at(9_879)), &mut log)
                .is_err()
        );
        assert_eq!(log.len(), 2);
        assert!(
            log[1].contains("would pass a claim's safe window"),
            "{log:?}"
        );
        // At the last safe moment the claim goes out despite the backoff, logged once.
        assert!(b.admit(&T, M, now, true, Some(at(9_880)), &mut log).is_ok());
        assert!(b.admit(&T, M, now, true, Some(at(9_990)), &mut log).is_ok());
        assert!(
            b.admit(&T, M, now, true, Some(at(10_119)), &mut log)
                .is_ok()
        );
        assert_eq!(log.len(), 3);
        assert!(log[2].contains("last safe moment, trying now"), "{log:?}");
        // Past deadline + margin the bypass ends: normal backoff again (no endless hammering).
        assert!(
            b.admit(&T, M, now, true, Some(at(10_120)), &mut log)
                .is_err()
        );
        // The backoff itself is untouched for every other request.
        assert!(b.admit(&T, M, now, true, None, &mut log).is_err());
    }

    /// The live incident: one item polling NUT-07 on every 3 s tick. Over a simulated hour the
    /// paced item makes at most one request per 6 s (+ the initial burst).
    #[test]
    fn no_request_storm_over_a_simulated_hour() {
        let mut b = Book::default();
        let start = Instant::now();
        let mut log = vec![];
        let mut sent = 0u64;
        let mut per_minute = vec![0u64; 60];
        for tick in 0..1200u64 {
            let now = start + Duration::from_secs(3 * tick);
            if b.wait(&T, &[M], now).is_zero() {
                // One watch pass: restore + NUT-07 + info, as a maker in `second_locked` does.
                for _ in 0..3 {
                    b.admit(&T, M, now, true, None, &mut log).unwrap();
                    sent += 1;
                    per_minute[(3 * tick / 60) as usize] += 1;
                }
            }
        }
        let unpaced = 1200 * 3;
        assert!(
            sent <= 3600 / 6 + 10,
            "{sent} requests/hour (unpaced: {unpaced})"
        );
        assert!(
            sent >= 3600 / 6 - 10,
            "pacing must not stall polling: {sent}"
        );
        assert!(
            per_minute[1..].iter().all(|n| *n <= 12),
            "steady state <= ~10/min: {per_minute:?}"
        );
        assert!(per_minute[0] <= 18, "burst bounded: {}", per_minute[0]);
    }

    #[test]
    fn pacing_is_per_mint() {
        let mut b = Book::default();
        let now = Instant::now();
        let mut log = vec![];
        for _ in 0..8 {
            b.admit(&T, M, now, true, None, &mut log).unwrap();
        }
        assert!(!b.wait(&T, &[M], now).is_zero());
        assert!(b.wait(&T, &["https://other.example"], now).is_zero());
        assert!(!b.wait(&T, &["https://other.example", M], now).is_zero());
        assert!(b.wait(&T, &[M], now + Duration::from_secs(48)).is_zero());
    }

    #[test]
    fn keys_fold_case_and_trailing_slash() {
        assert_eq!(key("HTTPS://Mint.Example/"), key(M));
    }
}
