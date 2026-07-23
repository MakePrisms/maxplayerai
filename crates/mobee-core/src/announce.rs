//! Seller lifecycle **announce** sink: one structured JSON event per lifecycle transition,
//! piped to a pluggable external command (`[seller_announce] command`).
//!
//! This is the daemon-native, first-class sibling of the log-tailing sidecar
//! (`scripts/mobee-buzz-sidecar.sh`): instead of grepping stderr, the daemon emits a typed,
//! schema-versioned JSON event and spawns the operator's sink command with that JSON on stdin.
//! Buzz is the first-class target (`sinks/buzz-announce.sh`), but the contract is generic — any
//! command that reads one JSON event from stdin works (Discord/Slack webhooks ship too).
//!
//! **Never money-adjacent, never blocking, always fail-soft.** An announce event carries only
//! ids/amounts/reasons already public on the relay or in the seller log — never a token, key, or
//! NIP-17 plaintext. Emission NEVER blocks the seller event loop: each event is dispatched on its
//! OWN detached OS thread that spawns the sink, writes the JSON, and bounded-waits (killing a
//! hung sink at the bound). A sink that is slow, hung, missing, or failing can never delay, stall,
//! or crash the daemon (an inline await in the event loop would deafen the daemon — announce
//! must never do that).

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Schema version for [`AnnounceEvent`]. Additive-only, exactly like the episode schema: new
/// fields are ADDED with `skip_serializing_if`, never removed or repurposed. A newer sink parses
/// an older event; an older sink ignores unknown newer fields.
pub const ANNOUNCE_SCHEMA_VERSION: u32 = 1;

/// One seller lifecycle event, serialized one-line-per-event to the sink's stdin.
///
/// The envelope (`v`, `event`, `ts`, `seller_pubkey`) is always present; everything else is
/// per-transition and omitted when absent (`skip_serializing_if`), so one shape represents every
/// lifecycle point. Built via the constructors below — never by hand — so the `event` label and
/// the fields that go with it stay in lockstep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnnounceEvent {
    /// Schema version ([`ANNOUNCE_SCHEMA_VERSION`]).
    pub v: u32,
    /// Lifecycle transition label: `online` · `claimed` · `delivered` · `collected` · `refused` ·
    /// `reconcile_released` · `job_failed`.
    pub event: &'static str,
    /// Unix seconds when the event was emitted.
    pub ts: u64,
    /// The seller's public key (hex).
    pub seller_pubkey: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buyer_pubkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_received: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nip42: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liveness: Option<String>,
}

impl AnnounceEvent {
    /// Envelope with every optional field cleared. Constructors fill in what their transition owns.
    fn base(event: &'static str, ts: u64, seller_pubkey: &str) -> Self {
        Self {
            v: ANNOUNCE_SCHEMA_VERSION,
            event,
            ts,
            seller_pubkey: seller_pubkey.to_owned(),
            job_id: None,
            buyer_pubkey: None,
            result_id: None,
            amount: None,
            amount_received: None,
            expected: None,
            deadline_unix: None,
            commit: None,
            mint: None,
            relay: None,
            nip42: None,
            reason_code: None,
            reason: None,
            liveness: None,
        }
    }

    /// Daemon came online (subscribed + past the NIP-42 auth wait; reacting to live offers).
    pub fn online(ts: u64, seller_pubkey: &str, relay: &str, mint: &str, nip42: &str) -> Self {
        let mut event = Self::base("online", ts, seller_pubkey);
        event.relay = Some(relay.to_owned());
        event.mint = Some(mint.to_owned());
        event.nip42 = Some(nip42.to_owned());
        event
    }

    /// Offer claimed (feedback-kind published + claim journaled). The daemon's first-ever claim signal
    /// — before this feature a claim was journaled but emitted no observable event at all.
    pub fn claimed(
        ts: u64,
        seller_pubkey: &str,
        job_id: &str,
        buyer_pubkey: &str,
        amount: u64,
        deadline_unix: u64,
    ) -> Self {
        let mut event = Self::base("claimed", ts, seller_pubkey);
        event.job_id = Some(job_id.to_owned());
        event.buyer_pubkey = Some(buyer_pubkey.to_owned());
        event.amount = Some(amount);
        event.deadline_unix = Some(deadline_unix);
        event
    }

    /// Result delivered (result-kind published).
    pub fn delivered(
        ts: u64,
        seller_pubkey: &str,
        job_id: &str,
        result_id: &str,
        commit: &str,
        amount: u64,
    ) -> Self {
        let mut event = Self::base("delivered", ts, seller_pubkey);
        event.job_id = Some(job_id.to_owned());
        event.result_id = Some(result_id.to_owned());
        event.commit = Some(commit.to_owned());
        event.amount = Some(amount);
        event
    }

    /// Payment collected — a kind-1059 wrap redeemed at the mint and the receipt journaled.
    pub fn collected(
        ts: u64,
        seller_pubkey: &str,
        job_id: &str,
        result_id: &str,
        amount_received: u64,
        expected: u64,
        mint: &str,
    ) -> Self {
        let mut event = Self::base("collected", ts, seller_pubkey);
        event.job_id = Some(job_id.to_owned());
        event.result_id = Some(result_id.to_owned());
        event.amount_received = Some(amount_received);
        event.expected = Some(expected);
        event.mint = Some(mint.to_owned());
        event
    }

    /// Offer refused at classify, with the machine-readable reason code (`OfferSkip::code`) + the
    /// human reason. `amount` is the offer amount when the offer parsed, else `None`.
    pub fn refused(
        ts: u64,
        seller_pubkey: &str,
        job_id: &str,
        reason_code: &str,
        reason: &str,
        amount: Option<u64>,
    ) -> Self {
        let mut event = Self::base("refused", ts, seller_pubkey);
        event.job_id = Some(job_id.to_owned());
        event.reason_code = Some(reason_code.to_owned());
        event.reason = Some(reason.to_owned());
        event.amount = amount;
        event
    }

    /// A restart-orphaned claim released during startup reconcile.
    pub fn reconcile_released(
        ts: u64,
        seller_pubkey: &str,
        job_id: &str,
        reason_code: &str,
        liveness: &str,
        reason: &str,
    ) -> Self {
        let mut event = Self::base("reconcile_released", ts, seller_pubkey);
        event.job_id = Some(job_id.to_owned());
        event.reason_code = Some(reason_code.to_owned());
        event.liveness = Some(liveness.to_owned());
        event.reason = Some(reason.to_owned());
        event
    }

    /// A parked claim released without executing — the buyer awarded another claim, or the offer
    /// deadline passed with no award. The seller committed no compute; the award gate did its job.
    pub fn released(ts: u64, seller_pubkey: &str, job_id: &str, reason_code: &str, reason: &str) -> Self {
        let mut event = Self::base("released", ts, seller_pubkey);
        event.job_id = Some(job_id.to_owned());
        event.reason_code = Some(reason_code.to_owned());
        event.reason = Some(reason.to_owned());
        event
    }

    /// A claimed job that failed before/at delivery (agent/git/publish error or deadline).
    pub fn job_failed(ts: u64, seller_pubkey: &str, job_id: &str, reason_code: &str, reason: &str) -> Self {
        let mut event = Self::base("job_failed", ts, seller_pubkey);
        event.job_id = Some(job_id.to_owned());
        event.reason_code = Some(reason_code.to_owned());
        event.reason = Some(reason.to_owned());
        event
    }

    /// Compact one-line JSON for the sink's stdin. Serialization of this fixed shape cannot fail;
    /// the `unwrap_or_default` is a belt-and-braces guard that never produces a partial line.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Dispatch one lifecycle event to the configured sink command, NEVER blocking the caller.
///
/// `command` is the operator's `[seller_announce] command` argv. **Empty ⇒ feature OFF: no
/// process is spawned and this is a zero-cost no-op** (absent-config zero-behavior-change). Non-
/// empty ⇒ the whole spawn+write+bounded-wait runs on its OWN detached OS thread and this call
/// returns immediately, so a slow or hung sink can never stall the seller event loop. Any error
/// (spawn failure, write failure, or the bounded-wait timeout) is logged ONCE and swallowed.
pub fn dispatch(command: &[String], timeout: Duration, event: &AnnounceEvent) {
    if command.is_empty() {
        return; // feature off — no sink configured.
    }
    let argv = command.to_vec();
    let json = event.to_json();
    let label = event.event;
    // Detached: spawn+write+wait all happen off the event loop. Never joined.
    std::thread::spawn(move || {
        if let Err(error) = run_sink(&argv, timeout, &json) {
            eprintln!("seller announce: sink for '{label}' failed (non-fatal, ignored): {error}");
        }
    });
}

/// Spawn the sink command, write `json` (+newline) to its stdin, then bounded-wait for it to
/// exit — killing it at `timeout`. Runs on the detached thread from [`dispatch`]; returning here
/// affects only that thread, never the event loop. Exposed `pub(crate)` so the bound can be
/// exercised directly by a timed test.
pub(crate) fn run_sink(command: &[String], timeout: Duration, json: &str) -> io::Result<()> {
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Write the event, then close stdin (EOF) so a sink reading to EOF completes. The JSON is
    // small (well under the pipe buffer), so this write does not block; even if a sink ignores
    // stdin entirely, the bounded-wait below still reaps it. Best-effort — a write error just
    // means the sink closed stdin early; the exit status still decides success below.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(json.as_bytes());
        let _ = stdin.write_all(b"\n");
        // `stdin` drops here → pipe closes → EOF for the sink.
    }
    // Bounded wait: poll for exit until the timeout, then kill + reap a hung sink.
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_status) => return Ok(()),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "sink exceeded {}ms bound; killed (event loop was never blocked)",
                            timeout.as_millis()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn unique_tmp(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("mobee-announce-test-{name}-{nanos}"));
        path
    }

    fn parse(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("announce event must be valid JSON")
    }

    #[test]
    fn envelope_is_versioned_and_complete() {
        let v = parse(&AnnounceEvent::online(1234, "sk", "wss://r", "https://m", "authenticated").to_json());
        assert_eq!(v["v"], ANNOUNCE_SCHEMA_VERSION);
        assert_eq!(v["event"], "online");
        assert_eq!(v["ts"], 1234);
        assert_eq!(v["seller_pubkey"], "sk");
        assert_eq!(v["relay"], "wss://r");
        assert_eq!(v["mint"], "https://m");
        assert_eq!(v["nip42"], "authenticated");
    }

    #[test]
    fn claimed_carries_the_claim_facts() {
        let v = parse(&AnnounceEvent::claimed(10, "sk", "job1", "buyer1", 42, 999).to_json());
        assert_eq!(v["event"], "claimed");
        assert_eq!(v["job_id"], "job1");
        assert_eq!(v["buyer_pubkey"], "buyer1");
        assert_eq!(v["amount"], 42);
        assert_eq!(v["deadline_unix"], 999);
        // Fields owned by other transitions are absent (not null-filled).
        assert!(v.get("result_id").is_none());
        assert!(v.get("amount_received").is_none());
    }

    #[test]
    fn delivered_carries_the_delivery_facts() {
        let v = parse(&AnnounceEvent::delivered(11, "sk", "job1", "res1", "abc123", 42).to_json());
        assert_eq!(v["event"], "delivered");
        assert_eq!(v["result_id"], "res1");
        assert_eq!(v["commit"], "abc123");
        assert_eq!(v["amount"], 42);
    }

    #[test]
    fn collected_carries_the_payment_facts() {
        let v = parse(&AnnounceEvent::collected(12, "sk", "job1", "res1", 40, 42, "https://m").to_json());
        assert_eq!(v["event"], "collected");
        assert_eq!(v["amount_received"], 40);
        assert_eq!(v["expected"], 42);
        assert_eq!(v["mint"], "https://m");
    }

    #[test]
    fn refused_carries_the_reason_code() {
        let v = parse(&AnnounceEvent::refused(13, "sk", "job1", "below_rate", "rate-gate: below floor", Some(3)).to_json());
        assert_eq!(v["event"], "refused");
        assert_eq!(v["reason_code"], "below_rate");
        assert_eq!(v["reason"], "rate-gate: below floor");
        assert_eq!(v["amount"], 3);
        // amount omitted entirely when None (unparseable offer).
        let none = parse(&AnnounceEvent::refused(13, "sk", "job1", "unparseable", "bad", None).to_json());
        assert!(none.get("amount").is_none());
    }

    #[test]
    fn reconcile_and_failed_carry_reasons() {
        let rel = parse(&AnnounceEvent::reconcile_released(14, "sk", "job1", "delivery_failed", "Expired", "claim expired").to_json());
        assert_eq!(rel["event"], "reconcile_released");
        assert_eq!(rel["reason_code"], "delivery_failed");
        assert_eq!(rel["liveness"], "Expired");
        assert_eq!(rel["reason"], "claim expired");

        let failed = parse(&AnnounceEvent::job_failed(15, "sk", "job1", "execution_failed", "agent timeout").to_json());
        assert_eq!(failed["event"], "job_failed");
        assert_eq!(failed["reason_code"], "execution_failed");
        assert_eq!(failed["reason"], "agent timeout");
    }

    #[test]
    fn empty_command_is_a_noop_no_spawn() {
        // Absent config ⇒ feature OFF. An empty argv must be a pure no-op: dispatch returns
        // without spawning anything (dropping the empty-guard would index `command[0]` and
        // panic, so this also red-on-reverts the guard). Zero behavior change when unconfigured.
        dispatch(&[], Duration::from_secs(1), &AnnounceEvent::job_failed(1, "sk", "j", "execution_failed", "x"));
        std::thread::sleep(Duration::from_millis(50));
        // Reaching here with no panic is the assertion.
    }

    #[test]
    fn capturing_sink_receives_exactly_one_json_per_event() {
        // A tiny capturing sink (`tee -a <file>`) receives the JSON on stdin. Dispatch three
        // distinct lifecycle events and prove each lands as exactly one well-formed JSON line
        // with the right event label + fields.
        let out = unique_tmp("capture");
        let command = vec!["tee".to_owned(), "-a".to_owned(), out.display().to_string()];
        let timeout = Duration::from_secs(5);
        dispatch(&command, timeout, &AnnounceEvent::claimed(1, "sk", "j1", "b1", 5, 100));
        dispatch(&command, timeout, &AnnounceEvent::delivered(2, "sk", "j1", "r1", "oid", 5));
        dispatch(&command, timeout, &AnnounceEvent::refused(3, "sk", "j2", "below_rate", "too cheap", Some(1)));

        // Wait (bounded) for all three detached sinks to land their line.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut lines: Vec<String> = Vec::new();
        while Instant::now() < deadline {
            if let Ok(body) = std::fs::read_to_string(&out) {
                lines = body.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
                if lines.len() == 3 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&out);
        assert_eq!(lines.len(), 3, "expected exactly 3 captured events, got {lines:?}");
        let mut events: Vec<String> = lines
            .iter()
            .map(|l| parse(l)["event"].as_str().unwrap().to_owned())
            .collect();
        events.sort();
        assert_eq!(events, vec!["claimed", "delivered", "refused"]);
    }

    #[test]
    fn dispatch_returns_immediately_even_for_a_hung_sink() {
        // The core guarantee: a hung sink (sleep 300s) must NOT delay the caller — dispatch only
        // spawns a detached thread and returns. The event loop calls dispatch synchronously, so
        // this proves the loop is never blocked past a trivial bound.
        let command = vec!["sleep".to_owned(), "300".to_owned()];
        let started = Instant::now();
        dispatch(&command, Duration::from_millis(200), &AnnounceEvent::job_failed(1, "sk", "j", "execution_failed", "x"));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "dispatch blocked the caller for {elapsed:?} — must return immediately"
        );
    }

    #[test]
    fn run_sink_kills_a_hung_sink_at_the_bound() {
        // The bounded-wait itself: run_sink against `sleep 300` returns a TimedOut error within
        // roughly the bound (never after 300s), proving the hung child is killed at the bound.
        let command = vec!["sleep".to_owned(), "300".to_owned()];
        let started = Instant::now();
        let result = run_sink(&command, Duration::from_millis(300), "{}");
        let elapsed = started.elapsed();
        assert!(result.is_err(), "a hung sink must surface the bound as an error");
        assert!(
            elapsed < Duration::from_secs(3),
            "run_sink honored no bound: took {elapsed:?} (child not killed)"
        );
    }

    #[test]
    fn missing_command_is_fail_soft() {
        // A nonexistent sink binary must not panic and must not block — dispatch swallows the
        // spawn error on its detached thread.
        dispatch(
            &["definitely-not-a-real-binary-xyz".to_owned()],
            Duration::from_secs(1),
            &AnnounceEvent::online(1, "sk", "r", "m", "n"),
        );
        std::thread::sleep(Duration::from_millis(100));
        // Reaching here without a panic is the assertion.
    }
}
