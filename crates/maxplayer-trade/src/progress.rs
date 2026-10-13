//! Swap progress on stderr: mint attempts that matter to a swap, and its state changes.
//!
//! Field report 2026-10-12 §4b: a `nostr://` claim took ~405 s behind ~10 untimestamped timeout
//! lines and a deadline printed as raw unix seconds. Each line here is one JSON object with the
//! UTC time, the swap, the mint and operation, the attempt number since the mint last answered,
//! the outcome (`answered`, `refused`, `timeout`, `no_answer`, `backing_off`, `error`, or
//! `started` for a retry being sent), elapsed time, the next retry, and the claim deadline and
//! earliest refund eligibility as UTC plus time remaining. The claim deadline is judged on the
//! MINT clock exactly as `backoff` judges it, and the line says which clock it used.
//!
//! DISPLAY ONLY. Nothing here is read by scheduling, budgets, retry or money decisions: every
//! value is derived from the swap record and the backoff book after the fact. A line never
//! carries a token, proof, secret or preimage: only the outcome class, a NUT code, and the
//! outermost error message passed through [`crate::redact_reason`] (transport errors withhold
//! mint bodies already; inner causes are never printed).
//!
//! Volume: a healthy swap prints its state changes only. Attempt lines appear once a mint stops
//! answering (or answers slower than [`SLOW_SECONDS`]); a change of outcome is printed at once,
//! a repeat of the same outcome at most once per [`REPEAT_SECONDS`] per swap and mint, with the
//! number of lines held back in `suppressed`.
use crate::{clock, coordinator::Swap};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// Repeat the same outcome for one swap and mint at most this often.
pub const REPEAT_SECONDS: u64 = 60;
/// An answer slower than this is reported (rate-limited) even when the mint never failed.
pub const SLOW_SECONDS: u64 = 5;

/// What a swap's progress lines show besides the attempt itself. Derived from the swap record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ctx {
    pub swap_id: String,
    pub role: String,
    /// (unix deadline on the MINT clock, claim mint): the claim send deadline.
    pub claim: Option<(u64, String)>,
    /// Earliest unix time (this host's clock) at which our own lock may be refunded.
    pub refund_from: Option<u64>,
}
impl Ctx {
    pub fn of(s: &Swap) -> Self {
        let q = s.quote.as_ref();
        let claim = crate::coordinator::claim_deadline(s)
            .map(|(deadline, mint)| (deadline, mint.to_owned()))
            .or_else(|| {
                // The taker's check of the maker's lock gates its claim: same deadline.
                let q = q?;
                (s.role == "taker" && s.state == "first_locked").then(|| {
                    (
                        q.short.saturating_sub(q.cutoff),
                        q.lot.give.asset.mint_url.clone(),
                    )
                })
            });
        // Mirrors the coordinator's refund gates (`time > long + margin` taker, `short + margin`
        // maker) while our own lock may still be outstanding; decides nothing.
        let refund_from = q.and_then(|q| match (s.role.as_str(), s.state.as_str()) {
            ("taker", "first_locked" | "second_validated") => {
                Some(q.long.saturating_add(q.margin).saturating_add(1))
            }
            ("maker", "second_locked" | "claiming" | "lock_unforwardable") => {
                Some(q.short.saturating_add(q.margin).saturating_add(1))
            }
            _ => None,
        });
        Ctx {
            swap_id: s.id.clone(),
            role: s.role.clone(),
            claim,
            refund_from,
        }
    }
}

/// The outcome of one mint call, as shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// The mint answered.
    Answered,
    /// The mint answered with a NUT error (code only).
    Refused(Option<u64>),
    /// No reply within the bound.
    Timeout,
    /// No usable answer: 429/502/503/504, a refused/reset connection, or no reply.
    NoAnswer,
    /// Not sent: the mint is backing off.
    BackingOff,
    /// Any other failure (invalid reply, local check); ambiguous, never a refusal.
    Error,
    /// A CDK wallet call that may have been served from its cache: proves nothing either way.
    Unknown,
}
impl Seen {
    fn label(self) -> &'static str {
        match self {
            Seen::Answered => "answered",
            Seen::Refused(_) => "refused",
            Seen::Timeout => "timeout",
            Seen::NoAnswer => "no_answer",
            Seen::BackingOff => "backing_off",
            Seen::Error => "error",
            Seen::Unknown => "unknown",
        }
    }
}

/// Classify a transport-level failure by type, never by its text.
pub fn classify(e: &anyhow::Error) -> Seen {
    if crate::backoff::deferred(e) {
        Seen::BackingOff
    } else if let Some(r) = crate::mint::MintRefusal::of(e) {
        Seen::Refused(Some(r.code))
    } else if crate::backoff::unanswered(e) {
        if timed_out(e) {
            Seen::Timeout
        } else {
            Seen::NoAnswer
        }
    } else {
        Seen::Error
    }
}
fn timed_out(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<crate::wallet::Bound>(),
            Some(crate::wallet::Bound::Elapsed)
        ) || matches!(c.downcast_ref::<cdk::Error>(), Some(cdk::Error::Timeout))
            || c.downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_timeout)
            || c.downcast_ref::<crate::transport::ObservedMintError>()
                .is_some_and(|o| o.0.reply_ms.is_none() && o.0.ignored_reply_ms.is_none())
    })
}
/// Outcome and redacted reason of a transport call (`transport::post`/`get`).
pub fn of_result<T>(r: &anyhow::Result<T>) -> (Seen, Option<String>) {
    match r {
        Ok(_) => (Seen::Answered, None),
        // The OUTERMOST message only: inner causes (a JSON parse error quoting the body, a CDK
        // error with mint detail) are never printed.
        Err(e) => (classify(e), Some(crate::redact_reason(&e.to_string()))),
    }
}
/// Outcome of a CDK wallet call bounded by `wallet::bounded_for` (no error text is printed).
pub fn of_wallet<T>(
    r: &std::result::Result<std::result::Result<T, cdk::Error>, crate::wallet::Bound>,
) -> (Seen, Option<String>) {
    let seen = match r {
        Ok(Ok(_)) => Seen::Unknown,
        Err(crate::wallet::Bound::Elapsed) | Ok(Err(cdk::Error::Timeout)) => Seen::Timeout,
        Err(crate::wallet::Bound::Deferred(_)) => Seen::BackingOff,
        Ok(Err(e)) if crate::backoff::cdk_unanswered(e) => Seen::NoAnswer,
        Ok(Err(_)) => Seen::Error,
    };
    (seen, None)
}

tokio::task_local! {
    static CTX: Ctx;
    static OP: String;
}

/// Run `f` with progress for this swap (`None`: plainly). Task-local like `backoff::with_claim`:
/// it does not cross `tokio::spawn`. Boxed: the wrapped coordinator futures are large.
pub async fn scoped<F: std::future::Future>(ctx: Option<Ctx>, f: F) -> F::Output {
    let f = Box::pin(f);
    match ctx {
        Some(ctx) => CTX.scope(ctx, f).await,
        None => f.await,
    }
}

/// Report one mint call `op` to `mint`: [`sending`] (from `backoff::admit`) when a request
/// actually goes out, the outcome when it returns. Outside a swap scope it only runs `f`.
pub async fn call<R>(
    mint: &str,
    op: &str,
    f: impl std::future::Future<Output = R>,
    seen: impl FnOnce(&R) -> (Seen, Option<String>),
) -> R {
    let Ok(ctx) = CTX.try_with(Clone::clone) else {
        return f.await;
    };
    let started = Instant::now();
    let r = OP.scope(op.to_owned(), Box::pin(f)).await;
    let (outcome, reason) = seen(&r);
    end(&ctx, mint, op, started.elapsed(), outcome, reason);
    r
}

#[derive(Default)]
struct Track {
    /// Requests sent since the mint last answered (the current one included).
    sent: u32,
    /// Consecutive calls without an answer.
    unanswered: u32,
    stalled_since: Option<Instant>,
    last: Option<&'static str>,
    last_emit: Option<u64>,
    suppressed: u32,
}
impl Track {
    /// Whether to print now (display clock `now`); `force` for a change of outcome.
    fn allow(&mut self, now: u64, force: bool) -> Option<u32> {
        if force
            || self
                .last_emit
                .is_none_or(|t| now >= t.saturating_add(REPEAT_SECONDS))
        {
            self.last_emit = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed += 1;
            None
        }
    }
}
type Key = (String, String, String);
static TRACKS: LazyLock<Mutex<HashMap<Key, Track>>> = LazyLock::new(Default::default);
static STATES: LazyLock<Mutex<HashMap<(String, String), String>>> = LazyLock::new(Default::default);
static FEED: LazyLock<tokio::sync::broadcast::Sender<(String, String)>> =
    LazyLock::new(|| tokio::sync::broadcast::channel(256).0);
#[cfg(any(test, feature = "lab"))]
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn key(ctx: &Ctx, mint: &str) -> Key {
    (
        ctx.swap_id.clone(),
        ctx.role.clone(),
        mint.trim_end_matches('/').to_ascii_lowercase(),
    )
}
fn emit(swap_id: &str, line: serde_json::Value) {
    let line = line.to_string();
    eprintln!("{line}");
    #[cfg(any(test, feature = "lab"))]
    lock(&LOG).push(line.clone());
    let _ = FEED.send((swap_id.to_owned(), line));
}

/// Progress lines as they are printed, keyed by swap id, for a serve client following one swap.
pub fn subscribe() -> tokio::sync::broadcast::Receiver<(String, String)> {
    FEED.subscribe()
}

/// Hook in `backoff::admit`: a request to `mint` is being sent now (not deferred). `charge`
/// false: a CDK wallet call that may be answered from cache, not counted as an attempt.
pub fn sending(mint: &str, charge: bool) {
    if !charge {
        return;
    }
    let Ok(ctx) = CTX.try_with(Clone::clone) else {
        return;
    };
    let op = OP
        .try_with(Clone::clone)
        .unwrap_or_else(|_| "request".into());
    let now = clock::now();
    // Read the backoff book before our own lock (never both at once).
    let fields = deadline_fields(&ctx, now);
    let line = {
        let mut tracks = lock(&TRACKS);
        let t = tracks.entry(key(&ctx, mint)).or_default();
        t.sent = t.sent.saturating_add(1);
        if t.unanswered == 0 {
            return;
        }
        let Some(suppressed) = t.allow(now, false) else {
            return;
        };
        let mut line = attempt_line(&ctx, mint, &op, "started", t, suppressed, now, fields);
        line["retry"] = true.into();
        line
    };
    emit(&ctx.swap_id, line);
}

fn end(ctx: &Ctx, mint: &str, op: &str, elapsed: Duration, seen: Seen, reason: Option<String>) {
    if seen == Seen::Unknown {
        return;
    }
    let now = clock::now();
    let failed = matches!(seen, Seen::Timeout | Seen::NoAnswer | Seen::BackingOff);
    // Read the backoff book before our own lock (never both at once).
    let next = failed.then(|| crate::backoff::next_retry(mint));
    let fields = deadline_fields(ctx, now);
    let line = {
        let mut tracks = lock(&TRACKS);
        let t = tracks.entry(key(ctx, mint)).or_default();
        let label = seen.label();
        let allowed = match seen {
            Seen::Answered => {
                let recovered = t.unanswered > 0;
                if recovered || elapsed >= Duration::from_secs(SLOW_SECONDS) {
                    t.allow(now, recovered)
                } else {
                    None
                }
            }
            Seen::Timeout | Seen::NoAnswer => {
                t.unanswered = t.unanswered.saturating_add(1);
                t.stalled_since.get_or_insert_with(Instant::now);
                let change = t.last != Some(label);
                t.allow(now, change)
            }
            Seen::BackingOff => t.allow(now, false),
            Seen::Refused(_) | Seen::Error | Seen::Unknown => {
                let change = t.last != Some(label);
                t.allow(now, change)
            }
        };
        if seen != Seen::BackingOff {
            t.last = Some(label);
        }
        let line = allowed.map(|suppressed| {
            let mut v = attempt_line(ctx, mint, op, label, t, suppressed, now, fields);
            v["elapsed"] = format!("{:.1}s", elapsed.as_secs_f64()).into();
            match next {
                Some(Some(wait)) => {
                    let at = now.saturating_add(wait.as_secs_f64().ceil() as u64);
                    v["next_retry"] = format!(
                        "not before {} (in {})",
                        clock::iso(at),
                        clock::span(at - now)
                    )
                    .into();
                }
                Some(None) => v["next_retry"] = "next recovery tick".into(),
                None => {}
            }
            if let Seen::Refused(Some(code)) = seen {
                v["nut_code"] = code.into();
            }
            if let Some(reason) = &reason {
                v["reason"] = reason.clone().into();
            }
            v
        });
        if matches!(seen, Seen::Answered | Seen::Refused(_)) {
            // The mint answered: the next failure starts a new streak at attempt 1.
            t.sent = 0;
            t.unanswered = 0;
            t.stalled_since = None;
        }
        line
    };
    if let Some(line) = line {
        emit(&ctx.swap_id, line);
    }
}

#[allow(clippy::too_many_arguments)]
fn attempt_line(
    ctx: &Ctx,
    mint: &str,
    op: &str,
    outcome: &str,
    t: &Track,
    suppressed: u32,
    now: u64,
    fields: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "at": clock::iso(now),
        "progress": "mint_attempt",
        "swap_id": ctx.swap_id,
        "role": ctx.role,
        "mint": mint,
        "op": op,
        "attempt": t.sent,
        "outcome": outcome,
    });
    if t.unanswered > 0 {
        v["unanswered_in_row"] = t.unanswered.into();
    }
    if let Some(since) = t.stalled_since {
        v["stalled_for"] = clock::span(since.elapsed().as_secs()).into();
    }
    if suppressed > 0 {
        v["suppressed"] = suppressed.into();
    }
    if let Some(o) = v.as_object_mut() {
        o.extend(fields);
    }
    v
}

/// Claim deadline (mint clock, as `backoff` judges it) and earliest refund eligibility (this
/// host's clock), each as UTC plus time remaining.
fn deadline_fields(ctx: &Ctx, now: u64) -> serde_json::Map<String, serde_json::Value> {
    let mut v = serde_json::Map::new();
    if let Some((deadline, mint)) = &ctx.claim {
        let (lead, observed) = crate::backoff::clock_lead(mint);
        v.insert("claim_deadline".into(), clock::iso(*deadline).into());
        v.insert(
            "claim_deadline_in".into(),
            clock::remaining(*deadline, now.saturating_add(lead)).into(),
        );
        v.insert(
            "claim_deadline_clock".into(),
            match observed {
                Some(skew) => format!("mint clock, observed skew {skew:+}s"),
                None => format!("mint clock, not observed yet: assumed {lead}s ahead"),
            }
            .into(),
        );
    }
    if let Some(at) = ctx.refund_from {
        v.insert("refund_earliest".into(), clock::iso(at).into());
        v.insert(
            "refund_earliest_in".into(),
            clock::remaining(at, now).into(),
        );
        v.insert(
            "refund_basis".into(),
            "this host's clock; the mint enforces locktime; eligibility, not a guaranteed refund"
                .into(),
        );
    }
    v
}

/// Hook in `coordinator::save`, AFTER the journal write: print a state change of `s`.
pub fn state(s: &Swap) {
    state_change(
        &s.id,
        &s.role,
        &s.state,
        crate::coordinator::terminal(s),
        Ctx::of(s),
    );
}
fn state_change(id: &str, role: &str, state: &str, terminal: bool, ctx: Ctx) {
    let k = (id.to_owned(), role.to_owned());
    {
        let mut states = lock(&STATES);
        if states.get(&k).map(String::as_str) == Some(state) {
            return;
        }
        if terminal {
            states.remove(&k);
        } else {
            if states.len() >= 4096 {
                states.clear();
            }
            states.insert(k, state.to_owned());
        }
    }
    if terminal {
        lock(&TRACKS).retain(|(swap, r, _), _| (swap.as_str(), r.as_str()) != (id, role));
    }
    let now = clock::now();
    let mut v = serde_json::json!({
        "at": clock::iso(now),
        "progress": "swap_state",
        "swap_id": id,
        "role": role,
        "state": state,
    });
    if let Some(o) = v.as_object_mut() {
        o.extend(deadline_fields(&ctx, now));
    }
    emit(id, v);
}

/// Lines printed since the last [`lab_reset`].
#[cfg(any(test, feature = "lab"))]
pub fn lab_log() -> Vec<String> {
    lock(&LOG).clone()
}
/// Clear the recorded lines and all per-swap tracking.
#[cfg(any(test, feature = "lab"))]
pub fn lab_reset() {
    lock(&LOG).clear();
    lock(&TRACKS).clear();
    lock(&STATES).clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    static SERIAL: Mutex<()> = Mutex::new(());
    const MINT: &str = "https://unobserved.mint.example";
    const T: u64 = 1_791_837_000; // 2026-10-12T20:30:00Z

    struct Frozen(#[allow(dead_code)] MutexGuard<'static, ()>);
    impl Frozen {
        fn at(t: u64) -> Self {
            let g = lock(&SERIAL);
            lab_reset();
            clock::lab_freeze(Some(t));
            Frozen(g)
        }
    }
    impl Drop for Frozen {
        fn drop(&mut self) {
            clock::lab_freeze(None);
        }
    }
    fn ctx(id: &str) -> Ctx {
        Ctx {
            swap_id: id.into(),
            role: "taker".into(),
            claim: Some((T + 600, MINT.into())),
            refund_from: Some(T + 3_000),
        }
    }
    fn lines(id: &str) -> Vec<serde_json::Value> {
        lab_log()
            .iter()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|v| v["swap_id"] == id)
            .collect()
    }
    /// One call as transport makes it: `backoff::admit` (the `sending` hook, only when the
    /// request is not deferred), then the outcome.
    async fn attempt(op: &str, r: anyhow::Result<()>) {
        let sent = !matches!(&r, Err(e) if crate::backoff::deferred(e));
        let _ = call(
            MINT,
            op,
            async move {
                if sent {
                    sending(MINT, true);
                }
                r
            },
            of_result,
        )
        .await;
    }
    fn timeout() -> anyhow::Result<()> {
        Err(anyhow::Error::new(crate::wallet::Bound::Elapsed).context("checkstate unanswered"))
    }
    fn deferred() -> anyhow::Result<()> {
        Err(anyhow::Error::new(crate::backoff::Deferred {
            mint: MINT.into(),
            reason: crate::backoff::Reason::Timeout,
            remaining: Duration::from_secs(9),
        }))
    }
    fn outline(l: &[serde_json::Value]) -> Vec<(&str, &str, u64, Option<u64>)> {
        l.iter()
            .map(|v| {
                (
                    v["at"].as_str().unwrap(),
                    v["outcome"].as_str().unwrap(),
                    v["attempt"].as_u64().unwrap(),
                    v["suppressed"].as_u64(),
                )
            })
            .collect()
    }

    /// timeout -> answer -> timeout -> complete, on a frozen display clock: every line carries
    /// the UTC time, attempt number and outcome; the claim deadline is UTC on the MINT clock (an
    /// unobserved mint counts 120 s ahead, as `backoff` judges it) and the refund eligibility is
    /// UTC on this host's clock, both with time remaining; completion is printed only by the
    /// journaled state change.
    #[tokio::test]
    async fn timeout_answer_timeout_complete_on_a_frozen_clock() {
        let _f = Frozen::at(T);
        let id = "sw-sequence";
        scoped(Some(ctx(id)), async {
            attempt("checkstate", timeout()).await;
            clock::lab_freeze(Some(T + 10));
            attempt("info", timeout()).await;
            clock::lab_freeze(Some(T + 70));
            attempt("info", Ok(())).await;
            attempt("swap", timeout()).await;
        })
        .await;
        let l = lines(id);
        assert_eq!(
            outline(&l),
            [
                ("2026-10-12T20:30:00Z", "timeout", 1, None),
                // T+10: the retry and its timeout are held back (same outcome within 60 s);
                // T+70: the next retry is shown with what was held back, then its answer.
                ("2026-10-12T20:31:10Z", "started", 3, Some(2)),
                ("2026-10-12T20:31:10Z", "answered", 3, None),
                // A new streak after the answer.
                ("2026-10-12T20:31:10Z", "timeout", 1, None),
            ],
            "{l:#?}"
        );
        let first = &l[0];
        assert_eq!(first["op"], "checkstate");
        assert_eq!(first["mint"], MINT);
        assert_eq!(first["unanswered_in_row"], 1);
        assert_eq!(first["claim_deadline"], "2026-10-12T20:40:00Z");
        // Mint clock: 600 s to the deadline minus the assumed 120 s lead.
        assert_eq!(first["claim_deadline_in"], "8m00s");
        assert_eq!(
            first["claim_deadline_clock"],
            "mint clock, not observed yet: assumed 120s ahead"
        );
        assert_eq!(first["refund_earliest"], "2026-10-12T21:20:00Z");
        assert_eq!(first["refund_earliest_in"], "50m00s");
        assert_eq!(first["next_retry"], "next recovery tick");
        assert!(first["elapsed"].as_str().unwrap().ends_with('s'));
        assert_eq!(l[1]["op"], "info");
        assert_eq!(l[1]["retry"], true);
        assert_eq!(l[1]["unanswered_in_row"], 2);
        let answered = &l[2];
        assert_eq!(answered["unanswered_in_row"], 2, "{answered}");
        assert_eq!(answered["claim_deadline_in"], "6m50s");
        assert!(answered.get("next_retry").is_none());
        assert_eq!(l[3]["op"], "swap");
        assert!(
            l.iter().all(|v| v["progress"] == "mint_attempt"),
            "SAFETY: no completion before the journal says so: {l:#?}"
        );
        // Completion comes from the journaled state, once, without deadlines.
        let done = Ctx {
            claim: None,
            refund_from: None,
            ..ctx(id)
        };
        state_change(id, "taker", "complete", true, done.clone());
        state_change(id, "taker", "second_validated", false, ctx(id));
        state_change(id, "taker", "second_validated", false, ctx(id));
        let states: Vec<_> = lines(id)
            .into_iter()
            .filter(|v| v["progress"] == "swap_state")
            .collect();
        assert_eq!(states.len(), 2, "a repeated state prints once: {states:#?}");
        assert_eq!(states[0]["state"], "complete");
        assert_eq!(states[0]["at"], "2026-10-12T20:31:10Z");
        assert!(states[0].get("claim_deadline").is_none());
        assert_eq!(states[1]["claim_deadline_in"], "6m50s");
        assert!(lock(&TRACKS).keys().all(|(swap, _, _)| swap != id));
    }

    /// A long stall prints one line per outcome change and otherwise one per REPEAT_SECONDS per
    /// swap and mint, counting what it held back; a backing-off mint does too; an answer after
    /// the stall is printed at once.
    #[tokio::test]
    async fn stall_is_rate_limited() {
        let _f = Frozen::at(T);
        let id = "sw-stall";
        scoped(Some(ctx(id)), async {
            for i in 0..30u64 {
                clock::lab_freeze(Some(T + i * 4));
                attempt("info", timeout()).await;
            }
            clock::lab_freeze(Some(T + 200));
            for _ in 0..20 {
                attempt("info", deferred()).await;
            }
            attempt("info", Ok(())).await;
        })
        .await;
        let l = lines(id);
        assert_eq!(
            outline(&l),
            [
                ("2026-10-12T20:30:00Z", "timeout", 1, None),
                ("2026-10-12T20:31:00Z", "started", 16, Some(28)),
                ("2026-10-12T20:33:20Z", "backing_off", 30, Some(29)),
                ("2026-10-12T20:33:20Z", "answered", 31, Some(20)),
            ],
            "{l:#?}"
        );
        assert_eq!(
            l[2]["unanswered_in_row"], 30,
            "a deferral is not an attempt"
        );
        assert_eq!(l[3]["unanswered_in_row"], 30);
    }

    /// No token, proof secret or preimage reaches a progress line: only the outcome class, a
    /// NUT code and the redacted outermost message.
    #[tokio::test]
    async fn redacted_mint_errors() {
        let _f = Frozen::at(T);
        let id = "sw-redact";
        let token = "cashuBo2FteCJodHRwczovL21pbnQuZXhhbXBsZS5jb20";
        let secret = "9f".repeat(32);
        scoped(Some(ctx(id)), async {
            attempt(
                "swap",
                Err(anyhow::Error::new(crate::mint::MintRefusal::new(
                    11001,
                    &format!("spent {token} {secret}"),
                ))
                .context(format!("mint {MINT} swap: HTTP 400 {token}"))),
            )
            .await;
            attempt(
                "swap",
                Err(anyhow::Error::new(crate::backoff::NoAnswer(format!(
                    "mint {MINT} swap: no reply {token} {secret}"
                )))),
            )
            .await;
            attempt(
                "swap",
                Err(anyhow::anyhow!("inner body {token} {secret}").context("invalid response")),
            )
            .await;
        })
        .await;
        let l = lines(id);
        let text = lab_log().join("\n");
        assert_eq!(l.len(), 3, "{text}");
        assert_eq!(l[0]["outcome"], "refused");
        assert_eq!(l[0]["nut_code"], 11001);
        assert_eq!(l[1]["outcome"], "no_answer");
        assert_eq!(l[2]["outcome"], "error");
        assert_eq!(
            l[2]["reason"], "invalid response",
            "inner causes never printed"
        );
        assert!(text.contains("<redacted token>"), "{text}");
        for needle in [token, &secret[..32], "inner body"] {
            assert!(!text.contains(needle), "SAFETY: {needle} leaked: {text}");
        }
    }

    /// Calls outside a swap scope print nothing; a cached (unknown) CDK wallet result neither
    /// prints nor ends a streak.
    #[tokio::test]
    async fn no_scope_no_lines_and_cached_wallet_calls_are_silent() {
        let _f = Frozen::at(T);
        attempt("info", timeout()).await;
        assert!(lab_log().is_empty());
        let id = "sw-cached";
        scoped(Some(ctx(id)), async {
            attempt("info", timeout()).await;
            let ok: std::result::Result<std::result::Result<(), cdk::Error>, crate::wallet::Bound> =
                Ok(Ok(()));
            call(MINT, "wallet", async { ok }, of_wallet).await.ok();
            attempt("info", Ok(())).await;
        })
        .await;
        let l = lines(id);
        assert_eq!(outline(&l).len(), 2, "{l:#?}");
        assert_eq!(l[1]["outcome"], "answered");
        assert_eq!(
            l[1]["unanswered_in_row"], 1,
            "the cached call did not end the streak"
        );
    }
}
