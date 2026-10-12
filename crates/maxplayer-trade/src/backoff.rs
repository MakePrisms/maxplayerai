//! Per-mint backoff and request pacing, shared by every item in one process.
//!
//! Why: on 2026-10-11 (binary from `bc41510`) one `serve` sent about 14 NUT-07 checkstates a
//! minute to a mint that was answering HTTP 429, and four serve/take processes kept retrying two
//! mints that had blocked this host. Every mint request this crate sends now passes [`admit`]
//! first and reports its outcome back (HTTP status, connection error, timeout).
//!
//! **Backoff** (per mint URL, per process, shared by all items):
//! - trigger: HTTP 429, 502, 503 or 504 (`Retry-After` is honoured, capped at 15 min), a refused
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
//! its whole sequence (restore, NUT-07, swap are never split by pacing). Pacing bounds ONLY the
//! recovery loop (the `serve` tick and `recover` items): there, at most one request per 6 s per
//! mint per process (10/min) long run, after a burst of at most 5 requests plus one item's
//! sequence. Message-driven advances (one per inbound trade message), CDK wallet calls and other
//! processes on the same host are NOT paced; they are bounded only by backoff, which stops them
//! as soon as the mint answers 429/503 or stops answering. `serve` skips a paced item until a
//! later tick; `recover` (one bounded pass) waits for it. Items take turns: the one served
//! longest ago goes first.
//!
//! **Claims are never idle:** a swap whose claim is possible (maker: preimage known, until the
//! taker's refund locktime `long`; taker: in `second_validated`, until its claim send deadline
//! `short - cutoff`) runs its advance inside [`with_claim`], keyed by that deadline AND the claim
//! mint. Requests to the claim mint (and only to it: refund and own-mint traffic keep normal
//! backoff) ignore a longer backoff or `Retry-After`: they are retried in rounds, one round per
//! 30 s ([`Timing::claim_retry`]), from the moment the claim is possible. In the final stretch
//! ([`margin`]: 10 min before the deadline) a round starts every 10 s. Each round sends until the
//! first failure, so a mint that keeps refusing sees at most one failed request per round. The
//! deadline is judged on the MINT clock (last observed `info.time` skew; an unobserved mint is
//! assumed 2 min ahead). An item with a possible claim is never paced. Past `deadline + margin`
//! normal backoff applies again (still retried, never dropped). Backoff never reorders, skips,
//! releases or refuses anything: it only delays a request, exactly as an outage would.
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
    /// Final stretch before a claim deadline (mint clock); `None` = [`CLAIM_MARGIN_SECONDS`]
    /// (at least two abandonment graces for the mint).
    pub urgent_margin: Option<Duration>,
    /// While a claim to a backing-off mint is possible: one attempt round per this interval.
    pub claim_retry: Duration,
    /// The same inside the final stretch.
    pub final_retry: Duration,
    /// Mint clock lead assumed for a mint whose `info.time` has not been observed yet.
    pub unknown_skew: Duration,
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
        claim_retry: Duration::from_secs(30),
        final_retry: Duration::from_secs(10),
        unknown_skew: Duration::from_secs(120),
        loopback: false,
    };
}

/// Final stretch before a claim deadline, in seconds of mint time. 10 min: one claim attempt
/// over HTTP is bounded by about 20 s per request, so the stretch holds ~60 rounds at
/// [`Timing::final_retry`] (and a serve tick or item budget misalignment costs one round, not
/// the claim). A clock skew under 10 min past the observed one still leaves the claim tried.
pub const CLAIM_MARGIN_SECONDS: u64 = 600;

/// Why a mint is being backed off. The label is what the log shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    RateLimited,
    Unavailable,
    /// HTTP 502 or 504 (a gateway in front of the mint).
    Gateway,
    Refused,
    Timeout,
}
impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::RateLimited => "429",
            Reason::Unavailable => "503",
            Reason::Gateway => "502/504",
            Reason::Refused => "refused",
            Reason::Timeout => "timeout",
        }
    }
}

/// The outcome of one request, as far as mint health goes.
#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    /// The mint answered (any status other than 429/502/503/504, NUT errors included).
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

/// A mint request that got no usable answer: no reply within its bound, or HTTP 429/502/503/504.
/// Ambiguous like [`Deferred`]; the text is the whole message.
#[derive(Debug)]
pub struct NoAnswer(pub String);
impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for NoAnswer {}

/// Whether `e` failed only because a mint did not answer: not sent while it backs off
/// ([`Deferred`]), no reply within a CDK wallet or transport bound, a refused/reset connection, or
/// 429/502/503/504 (exactly the failures that start a backoff). The same request may succeed
/// later. A mint's own answer (a NUT error included) and every local validation failure are NOT
/// unanswered: they are refusals.
pub fn unanswered(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.is::<Deferred>()
            || c.is::<NoAnswer>()
            || c.is::<crate::wallet::Bound>()
            || c.downcast_ref::<cdk::Error>().is_some_and(cdk_unanswered)
            || c.downcast_ref::<reqwest::Error>()
                .is_some_and(|e| classify_reqwest(e).is_some())
    })
}

/// A claim deadline, as seen by one request to the claim mint. `now_unix` is mint time.
#[derive(Clone, Copy, Debug)]
struct Urgency {
    deadline: u64,
    now_unix: u64,
    margin: u64,
}
impl Urgency {
    /// Claim rounds run until `deadline + margin`. After it, normal backoff applies again
    /// (still retried, never dropped), so a stuck claim cannot hammer forever.
    fn over(&self) -> bool {
        self.now_unix >= self.deadline.saturating_add(self.margin)
    }
    /// `[deadline - margin, deadline + margin)`: rounds at the shorter final cadence.
    fn final_stretch(&self) -> bool {
        !self.over() && self.now_unix.saturating_add(self.margin) >= self.deadline
    }
}

/// One claim retry round against a backing-off mint: requests pass until the first failure.
#[derive(Clone, Copy, Debug)]
struct Round {
    start: Instant,
    open: bool,
}

#[derive(Default)]
struct MintState {
    failures: u32,
    until: Option<Instant>,
    reason: Option<Reason>,
    debt: Option<Instant>,
    /// (deadline, stage) of the last claim warning: 1 = claim rounds, 2 = final stretch.
    warned: Option<(u64, u8)>,
    /// The current claim retry round while backing off.
    round: Option<Round>,
    /// Last observed `info.time - local unix time`, seconds.
    skew: Option<i64>,
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
                let deferred = |remaining| Deferred {
                    mint: mint.to_owned(),
                    reason,
                    remaining,
                };
                let Some(u) = urgent.filter(|u| !u.over()) else {
                    return Err(deferred(remaining));
                };
                let (every, stage) = if u.final_stretch() {
                    (t.final_retry, 2)
                } else {
                    (t.claim_retry, 1)
                };
                match m.round {
                    Some(r) if now < r.start + every => {
                        if !r.open {
                            return Err(deferred((r.start + every - now).min(remaining)));
                        }
                    }
                    _ => {
                        m.round = Some(Round {
                            start: now,
                            open: true,
                        });
                        if m.warned < Some((u.deadline, stage)) {
                            m.warned = Some((u.deadline, stage));
                            log.push(if stage == 1 {
                                format!(
                                    "mint {mint} backing off ({}, {}s left) but a claim to it is possible: retrying it every {}s until its deadline (unix {}, mint clock)",
                                    reason.label(),
                                    remaining.as_secs(),
                                    every.as_secs_f64(),
                                    u.deadline
                                )
                            } else {
                                format!(
                                    "mint {mint} backing off ({}) and a claim to it is due before unix {} (mint clock): final stretch, retrying it every {}s",
                                    reason.label(),
                                    u.deadline,
                                    every.as_secs_f64()
                                )
                            });
                        }
                    }
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
                // `round` and `warned` survive an answer: a mint that answers restore/NUT-07
                // but rate-limits the swap POST must not get a fresh claim round (and a fresh
                // warning) on every serve tick. A stale round is only consulted while backing
                // off with a possible claim, and an expired one is replaced (review r2, M-A).
            }
            Outcome::Failed(reason, retry_after) => {
                // A backoff that expired more than `cap` ago is history: start again at `base`
                // instead of jumping straight to the cap on one blip hours later.
                if m.until.is_some_and(|u| now >= u + t.cap) {
                    m.failures = 0;
                    m.reason = None;
                    m.warned = None;
                }
                // The failure ends the current claim round; the next one starts on cadence.
                if let Some(r) = &mut m.round {
                    r.open = false;
                }
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

    fn observe(&mut self, mint: &str, local: u64, mint_time: u64) {
        let skew = i64::try_from(mint_time).unwrap_or(i64::MAX) - i64::try_from(local).unwrap_or(0);
        self.mints.entry(key(mint)).or_default().skew = Some(skew);
    }

    /// The mint's clock: local time plus its last observed lead (a mint behind us counts as
    /// on time; an unobserved one as [`Timing::unknown_skew`] ahead).
    fn mint_now(&self, t: &Timing, mint: &str, local: u64) -> u64 {
        let lead = match self.mints.get(&key(mint)).and_then(|m| m.skew) {
            Some(skew) => u64::try_from(skew.max(0)).unwrap_or(0),
            None => t.unknown_skew.as_secs(),
        };
        local.saturating_add(lead)
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
    /// (deadline, mint key) of every claim (or claim-critical read) the current task may have to
    /// land. Nested [`with_claim`] scopes add to it.
    static CLAIM: Vec<(u64, String)>;
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

/// Final stretch before a claim deadline to `mint`, in seconds of mint time.
pub fn margin(mint: &str) -> u64 {
    timing()
        .urgent_margin
        .map(|m| m.as_secs())
        .unwrap_or_else(|| {
            CLAIM_MARGIN_SECONDS.max(2 * crate::transport::abandon_grace_seconds(mint))
        })
}

/// Run `f` as an advance whose claim to `mint` must land before `deadline` (unix seconds, mint
/// clock). Only requests to `mint` get claim rounds; every other mint keeps normal backoff.
///
/// The scope is a tokio task-local: it does NOT cross `tokio::spawn` (or a `JoinSet`). Nothing
/// on the claim path spawns today; a request moved into a spawned task would silently lose its
/// claim rounds and fall back to normal backoff.
///
/// Scopes nest: an inner scope adds its (deadline, mint) to the outer ones (a maker watching its
/// own `nostr://` mint for the claim witness inside the advance that may claim on the other mint).
/// A mint named by several scopes uses the latest deadline.
pub async fn with_claim<F: std::future::Future>(deadline: u64, mint: &str, f: F) -> F::Output {
    let mut scope = CLAIM.try_with(Clone::clone).unwrap_or_default();
    scope.push((deadline, key(mint)));
    CLAIM.scope(scope, f).await
}

/// Record a mint's `info.time` (its clock), observed now.
pub fn observe_time(mint: &str, mint_time: u64) {
    book().observe(mint, crate::coordinator::now(), mint_time);
}

/// `mint`'s clock as far as claim deadlines go (see [`Book::mint_now`]).
pub fn mint_now(mint: &str) -> u64 {
    book().mint_now(&timing(), mint, crate::coordinator::now())
}

fn urgency(mint: &str) -> Option<Urgency> {
    let mint_key = key(mint);
    let deadline = CLAIM
        .try_with(|scope| {
            scope
                .iter()
                .filter(|(_, claim_mint)| *claim_mint == mint_key)
                .map(|(deadline, _)| *deadline)
                .max()
        })
        .ok()
        .flatten()?;
    Some(Urgency {
        deadline,
        now_unix: mint_now(mint),
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
    // Before taking the book: `urgency` reads it too (the mutex is not reentrant).
    let urgent = urgency(mint);
    let result = book().admit(&timing(), mint, Instant::now(), charge, urgent, &mut log);
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
            // A CDN or proxy in front of an overloaded mint (review r2, N-3).
            502 | 504 => Outcome::Failed(Reason::Gateway, retry_after),
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

/// CDK flattens a transport failure to `HttpError(None, reqwest_error.to_string())`; the text
/// match is pinned by `cdk_connection_refused_is_recognised`.
fn classify_cdk(e: &cdk::Error) -> Option<Reason> {
    Some(match e {
        cdk::Error::HttpError(Some(429), _) => Reason::RateLimited,
        cdk::Error::HttpError(Some(503), _) => Reason::Unavailable,
        cdk::Error::HttpError(Some(502 | 504), _) => Reason::Gateway,
        cdk::Error::Timeout => Reason::Timeout,
        cdk::Error::HttpError(None, msg) if msg.contains("error sending request") => {
            Reason::Refused
        }
        _ => return None,
    })
}

/// Whether a CDK (wallet or `nostr://` connector) error means the mint did not answer: a backoff
/// trigger, or any transport failure without an HTTP status (no relay reachable, pool closed).
pub fn cdk_unanswered(e: &cdk::Error) -> bool {
    classify_cdk(e).is_some() || matches!(e, cdk::Error::HttpError(None, _))
}

/// Report a CDK (wallet or `nostr://` connector) failure. Successes are not reported here: a CDK
/// call may be answered from its cache without reaching the mint. CDK does not expose response
/// headers, so a 429 on this path never carries a `Retry-After`: exponential backoff applies.
pub fn record_cdk(mint: &str, e: &cdk::Error) {
    if let Some(reason) = classify_cdk(e) {
        record(mint, Outcome::Failed(reason, None));
    }
}

/// `Retry-After` as delay-seconds or an HTTP-date (RFC 9110); a past date means now. The 15 min
/// cap is applied when recording.
fn parse_retry_after(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

fn retry_after(r: &reqwest::Response) -> Option<Duration> {
    parse_retry_after(
        r.headers()
            .get(reqwest::header::RETRY_AFTER)?
            .to_str()
            .ok()?,
        std::time::SystemTime::now(),
    )
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
/// this tick. `recover` waits instead of returning false. An item with a possible claim
/// (`claim`: its deadline and claim mint) is never paced until `deadline + margin`.
pub async fn ready(item: &str, mints: &[&str], claim: Option<(u64, &str)>) -> bool {
    let mints: Vec<&str> = mints.iter().copied().filter(|m| !exempt(m)).collect();
    let urgent = claim.is_some_and(|(deadline, mint)| {
        !Urgency {
            deadline,
            now_unix: mint_now(mint),
            margin: margin(mint),
        }
        .over()
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

    /// H1: a claim to a backing-off mint is retried in 30 s rounds from the moment it is
    /// possible (not only near the deadline), one failed request per round however long the
    /// backoff or `Retry-After`, and every 10 s in the final stretch.
    #[test]
    fn claim_retried_on_short_cadence_however_long_the_backoff() {
        let mut b = Book::default();
        let t0 = Instant::now();
        let mut log = vec![];
        fail(&mut b, t0, Reason::RateLimited, Some(900), &mut log);
        let deadline = 100_000;
        let at = |unix| Urgency {
            deadline,
            now_unix: unix,
            margin: 600,
        };
        // A mint that keeps answering 429 (Retry-After 900) for 15 minutes, an hour before
        // the deadline: the claim reaches it once per 30 s, never more, never idle.
        let mut sent = vec![];
        for ms in (0..900_000u64).step_by(500) {
            let now = t0 + Duration::from_millis(ms);
            let unix = deadline - 3_600 + ms / 1000;
            // Bounded: a round that a failure does not close would admit forever.
            for _ in 0..3 {
                if b.admit(&T, M, now, true, Some(at(unix)), &mut log).is_err() {
                    break;
                }
                sent.push(ms);
                fail(&mut b, now, Reason::RateLimited, Some(900), &mut log);
            }
            // Refund or watch traffic to the same mint (no claim) keeps the backoff.
            assert!(b.admit(&T, M, now, true, None, &mut log).is_err());
        }
        assert_eq!(sent.len(), 30, "{sent:?}");
        assert!(
            sent.windows(2).all(|w| w[1] - w[0] == 30_000),
            "every 30 s: {sent:?}"
        );
        assert!(
            log.iter()
                .any(|l| l.contains("a claim to it is possible: retrying it every 30s")),
            "{log:?}"
        );
        // A round sends until its first failure: the claim's several requests all pass.
        let later = t0 + Duration::from_secs(10_000);
        fail(&mut b, later, Reason::RateLimited, Some(900), &mut log);
        let later = later + Duration::from_secs(31);
        for _ in 0..5 {
            assert!(
                b.admit(&T, M, later, true, Some(at(deadline - 1_000)), &mut log)
                    .is_ok()
            );
        }
        // Final stretch (mint clock within `margin` of the deadline): every 10 s.
        fail(&mut b, later, Reason::RateLimited, Some(900), &mut log);
        let near = deadline - 300;
        // The round above started at `later` (+31 s from the failure before it).
        let ok = |b: &mut Book, s: u64, log: &mut Vec<String>| {
            b.admit(
                &T,
                M,
                later + Duration::from_secs(s),
                true,
                Some(at(near)),
                log,
            )
            .is_ok()
        };
        assert!(!ok(&mut b, 9, &mut log), "round closed by the failure");
        assert!(ok(&mut b, 10, &mut log), "final stretch: 10 s rounds");
        assert!(
            log.iter()
                .any(|l| l.contains("final stretch, retrying it every 10s")),
            "{log:?}"
        );
        // Past deadline + margin the rounds end: normal backoff (no endless hammering).
        fail(&mut b, later, Reason::RateLimited, Some(900), &mut log);
        assert!(
            b.admit(
                &T,
                M,
                later + Duration::from_secs(60),
                true,
                Some(at(deadline + 600)),
                &mut log
            )
            .is_err()
        );
    }

    #[test]
    fn production_claim_margin_is_wide() {
        assert_eq!(T.claim_retry, Duration::from_secs(30));
        assert_eq!(T.final_retry, Duration::from_secs(10));
        assert_eq!(margin("https://mint.example"), 600);
        assert!(margin("https://mint.example") >= 5 * 120);
        let npub = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
        assert!(margin(npub) >= 2 * crate::transport::NOSTR_ABANDON_GRACE_SECONDS);
    }

    #[test]
    fn mint_clock_from_observed_skew() {
        let mut b = Book::default();
        // Unobserved: assumed `unknown_skew` (2 min) ahead.
        assert_eq!(b.mint_now(&T, M, 1_000), 1_120);
        b.observe(M, 1_000, 1_300);
        assert_eq!(b.mint_now(&T, "HTTPS://mint.example/", 2_000), 2_300);
        // A mint behind us counts as on time (local is the lower bound).
        b.observe(M, 1_000, 900);
        assert_eq!(b.mint_now(&T, M, 2_000), 2_000);
    }

    /// The claim mint's clock decides the window: ahead by 15 min, the final stretch (and its
    /// end) come 15 min earlier in local time. Global state; unique mint URLs.
    #[tokio::test]
    async fn claim_window_judged_on_mint_clock() {
        let ahead = "https://ahead.claim-clock.example";
        let even = "https://even.claim-clock.example";
        let local = crate::coordinator::now();
        observe_time(ahead, local + 900);
        observe_time(even, local);
        let deadline = local + 100;
        for mint in [ahead, even] {
            record_status(mint, 429, Some(Duration::from_secs(900)));
        }
        let (a, e) = with_claim(deadline, ahead, async { (urgency(ahead), mint_now(ahead)) }).await;
        let a = a.expect("claim mint");
        assert!(e >= local + 900 && a.now_unix >= local + 900);
        assert!(a.over(), "mint time is past deadline + margin");
        assert!(
            with_claim(deadline, ahead, async { admit(ahead, true) })
                .await
                .is_err()
        );
        let e = with_claim(deadline, even, async { urgency(even) })
            .await
            .unwrap();
        assert!(e.final_stretch() && !e.over());
        assert!(
            with_claim(deadline, even, async { admit(even, true) })
                .await
                .is_ok()
        );
        // Far from the deadline on local time but inside the final stretch on the mint's.
        let early = local + 1_000;
        let ahead2 = "https://ahead2.claim-clock.example";
        observe_time(ahead2, local + 900);
        let u = with_claim(early, ahead2, async { urgency(ahead2) })
            .await
            .unwrap();
        assert!(u.final_stretch(), "{u:?}");
    }

    /// M2: only the claim mint gets claim rounds; refund and own-mint traffic in the same
    /// advance keeps its backoff. Global state; unique mint URLs.
    #[tokio::test]
    async fn claim_rounds_only_for_the_claim_mint() {
        let claim = "https://claim.claim-scope.example";
        let own = "https://own.claim-scope.example";
        for mint in [claim, own] {
            observe_time(mint, crate::coordinator::now());
            record_status(mint, 429, Some(Duration::from_secs(900)));
        }
        let deadline = crate::coordinator::now() + 3_000;
        let (c, o) = with_claim(deadline, "HTTPS://claim.claim-scope.example/", async {
            (admit(claim, true), admit(own, true))
        })
        .await;
        assert!(c.is_ok(), "claim mint: claim round");
        let o = o.expect_err("own mint keeps its backoff");
        assert!(o.remaining > Duration::from_secs(800), "{o}");
        assert!(admit(claim, true).is_err(), "outside the claim scope");
        assert!(urgency(claim).is_none());
    }

    #[test]
    fn stale_failure_count_decays_after_backoff_expires() {
        let mut b = Book::default();
        let now = Instant::now();
        let mut log = vec![];
        for _ in 0..8 {
            fail(&mut b, now, Reason::Timeout, None, &mut log);
        }
        let until = now + b.remaining(M, now).unwrap();
        assert_eq!(b.remaining(M, now).unwrap(), T.cap);
        // A blip soon after expiry still escalates…
        let soon = until + Duration::from_secs(1);
        fail(&mut b, soon, Reason::Timeout, None, &mut log);
        assert_eq!(b.remaining(M, soon).unwrap(), T.cap);
        // …but one hours later starts again at the base delay (+ jitter).
        let until = soon + T.cap;
        let hours = until + Duration::from_secs(3 * 3600);
        fail(&mut b, hours, Reason::Timeout, None, &mut log);
        assert_eq!(b.remaining(M, hours).unwrap().as_millis(), 6250);
    }

    #[test]
    fn retry_after_http_date() {
        let at = httpdate::parse_http_date("Wed, 21 Oct 2026 07:28:00 GMT").unwrap();
        let now = at - Duration::from_secs(120);
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after(" 42 ", now),
            Some(Duration::from_secs(42))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT", at + Duration::from_secs(5)),
            Some(Duration::ZERO),
            "a past date means now"
        );
        assert_eq!(parse_retry_after("soon", now), None);
        // A far date is capped at 15 min when recorded.
        let mut b = Book::default();
        let mut log = vec![];
        let i = Instant::now();
        let far = parse_retry_after(
            "Wed, 21 Oct 2026 07:28:00 GMT",
            at - Duration::from_secs(86_400),
        );
        b.record(
            &T,
            M,
            i,
            Outcome::Failed(Reason::RateLimited, far),
            0.0,
            &mut log,
        );
        assert_eq!(b.remaining(M, i).unwrap().as_secs(), 900);
    }

    /// L3: pin the CDK transport-error text `classify_cdk` matches (cdk 0.17.2 flattens the
    /// reqwest error with `to_string()`). Loopback port with nothing listening; no real mint.
    #[tokio::test]
    async fn cdk_connection_refused_is_recognised() {
        use cdk::wallet::MintConnector;
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let client = cdk::wallet::HttpClient::new(url.parse().unwrap(), None);
        let e = client.get_mint_info().await.expect_err("nothing listens");
        assert!(matches!(e, cdk::Error::HttpError(None, _)), "{e:?}");
        assert_eq!(classify_cdk(&e), Some(Reason::Refused), "{e:?}");
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

    /// M-A (review r2): a claim mint that answers restore and NUT-07 but rate-limits the swap
    /// POST (Retry-After 900). Serve ticks every 3 s for 60 s; each tick's claim attempt is
    /// restore (answered), NUT-07 (answered), swap (429). The answers must not reset the claim
    /// round: one round per 30 s, so at most 2 rounds (6 requests, 2 swaps) in 60 s, not a
    /// fresh round on every tick (60 requests, 20 swaps before the fix).
    #[test]
    fn answered_requests_do_not_reset_the_claim_round() {
        let mut b = Book::default();
        let t0 = Instant::now();
        let mut log = vec![];
        let u = Urgency {
            deadline: 100_000,
            now_unix: 100_000 - 3_600,
            margin: 600,
        };
        fail(&mut b, t0, Reason::RateLimited, Some(900), &mut log);
        let (mut sent, mut swaps) = (0, 0);
        for tick in 0..20u64 {
            let now = t0 + Duration::from_secs(1 + 3 * tick);
            for step in 0..3 {
                if b.admit(&T, M, now, true, Some(u), &mut log).is_err() {
                    break;
                }
                sent += 1;
                if step < 2 {
                    b.record(&T, M, now, Outcome::Answered, 1.0, &mut log);
                } else {
                    swaps += 1;
                    fail(&mut b, now, Reason::RateLimited, Some(900), &mut log);
                }
            }
        }
        assert!(
            sent <= 6,
            "{sent} requests ({swaps} swaps) in 60 s: {log:?}"
        );
        assert_eq!(swaps, 2, "one swap per 30 s round, never idle: {log:?}");
        assert_eq!(
            log.iter()
                .filter(|l| l.contains("a claim to it is possible"))
                .count(),
            1,
            "warned once, not per round: {log:?}"
        );
    }

    /// N-3 (review r2): 502/504 from a gateway in front of the mint back off like 503; other
    /// errors (500, NUT 400) are answers. Global state; unique mint URLs.
    #[test]
    fn gateway_errors_back_off() {
        for (status, backs_off) in [
            (502, true),
            (504, true),
            (503, true),
            (500, false),
            (400, false),
        ] {
            let mint = format!("https://status-{status}.gateway-errors.example");
            record_status(&mint, status, None);
            assert_eq!(remaining(&mint).is_some(), backs_off, "{status}");
        }
        assert_eq!(
            classify_cdk(&cdk::Error::HttpError(Some(504), String::new())),
            Some(Reason::Gateway)
        );
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

    /// Only an unanswered mint request makes a trade message retryable; a mint's own refusal and
    /// every local validation failure stay refusals.
    #[test]
    fn unanswered_is_only_no_answer() {
        use anyhow::Context;
        let deferred = anyhow::Error::new(Deferred {
            mint: M.into(),
            reason: Reason::Timeout,
            remaining: Duration::from_secs(5),
        });
        assert!(unanswered(&deferred.context("check second lock")));
        let elapsed: Result<(), crate::wallet::Bound> = Err(crate::wallet::Bound::Elapsed);
        assert!(unanswered(
            &elapsed.context("CDK wallet request not answered").unwrap_err()
        ));
        assert!(unanswered(&anyhow::Error::new(cdk::Error::Timeout)));
        assert!(unanswered(&anyhow::Error::new(cdk::Error::HttpError(
            None,
            "no relay reachable".into()
        ))));
        assert!(unanswered(&anyhow::Error::new(NoAnswer("x".into()))));
        assert!(!unanswered(&anyhow::anyhow!("wrong hash lock")));
        assert!(!unanswered(&anyhow::anyhow!("proof is not UNSPENT")));
        assert!(!unanswered(
            &anyhow::Error::new(crate::mint::MintRefusal::new(11001, "spent"))
                .context("mint m checkstate")
        ));
        assert!(!unanswered(&anyhow::Error::new(cdk::Error::HttpError(
            Some(400),
            "bad".into()
        ))));
    }

    #[test]
    fn keys_fold_case_and_trailing_slash() {
        assert_eq!(key("HTTPS://Mint.Example/"), key(M));
    }
}
