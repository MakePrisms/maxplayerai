//! Seller **brain/episode telemetry** stream: every [`Episode`](crate::episode::Episode) captured
//! to `episodes.jsonl` (the on-disk source of truth) is ALSO emitted, live, as one JSON telemetry
//! event so an operator can watch what is going on inside a mobee's brain as it happens.
//!
//! Two independent, best-effort delivery paths, both driven by [`emit`]:
//!   * a pluggable **sink command** (`[telemetry] command`) spawned with the event JSON on stdin —
//!     the daemon-native, no-shell contract identical to [`crate::announce`]; and
//!   * an optional append-only **mirror file** (`[telemetry] mirror_file`), a durable JSONL tap.
//!
//! Relationship to the other channels (no duplication):
//!   * [`crate::announce`] carries seller LIFECYCLE transitions (online/claimed/delivered/
//!     collected/refused/…) — the money/publish signals. Telemetry does NOT re-send those; it is
//!     the episode/BRAIN channel (per-job reasoning + economics: usage, wall-time, outcome, harness).
//!   * `episodes.jsonl` is the fail-tolerant on-disk log; telemetry is the live wire over the top.
//!
//! **Never money-adjacent, never blocking, always fail-soft.** A telemetry event wraps only an
//! `Episode`, whose fields are ids/amounts/mint/task-text/self-reported-usage — NEVER a token, key,
//! or proof secret (see `episode.rs` module doc). Emission is best-effort off the hot path: the
//! sink runs the whole spawn+write+bounded-wait on its OWN detached thread (via
//! [`crate::announce::run_sink`], the proven bounded runner) and a mirror write that fails is
//! logged once and swallowed. A slow/hung/missing/failing sink or an unwritable mirror can never
//! block, stall, or crash the seller loop — and, by construction, never blocks or loses the
//! `episodes.jsonl` append, which the caller always performs FIRST.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::episode::Episode;
use crate::home::TelemetryConfig;

/// Schema version for [`TelemetryEvent`]. Additive-only, exactly like the episode/announce schemas:
/// new envelope fields are ADDED with `skip_serializing_if`, never removed or repurposed.
pub const TELEMETRY_SCHEMA_VERSION: u32 = 1;

/// One brain-telemetry event: a `type:"episode"` envelope wrapping a full [`Episode`], with the
/// seller pubkey + job id hoisted to the top level so a sink can route without parsing the nested
/// episode. Borrows the episode (and its pubkey/job_id) — serialize on the caller thread with
/// [`to_json`](Self::to_json) before handing the owned line to the detached sink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TelemetryEvent<'a> {
    /// Schema version ([`TELEMETRY_SCHEMA_VERSION`]).
    pub v: u32,
    /// Envelope discriminator. Currently always `"episode"` (the only brain-stream shape); a future
    /// per-turn/progress shape would add a new label, never repurpose this one.
    #[serde(rename = "type")]
    pub event_type: &'static str,
    /// Unix seconds when the event was emitted.
    pub ts: u64,
    /// The seller's public key (hex) — same value as `episode.seller_pubkey`, hoisted for routing.
    pub seller_pubkey: &'a str,
    /// The job id — same value as `episode.job_id`, hoisted for routing.
    pub job_id: &'a str,
    /// The wrapped episode, serialized inline under `"episode"`.
    pub episode: &'a Episode,
}

impl<'a> TelemetryEvent<'a> {
    /// Wrap an episode as a `type:"episode"` telemetry event.
    pub fn episode(ts: u64, episode: &'a Episode) -> Self {
        Self {
            v: TELEMETRY_SCHEMA_VERSION,
            event_type: "episode",
            ts,
            seller_pubkey: &episode.seller_pubkey,
            job_id: &episode.job_id,
            episode,
        }
    }

    /// Compact one-line JSON for the sink's stdin / the mirror file. This fixed shape cannot fail
    /// to serialize; the `unwrap_or_default` is a belt-and-braces guard against a partial line.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Emit one telemetry event to whatever the operator configured — best-effort, never blocking,
/// never affecting the caller's already-completed `episodes.jsonl` append.
///
/// No-op when the channel is disabled (`enabled = false`). Otherwise the event is serialized once,
/// appended to `mirror_file` (if set; a write error is logged and swallowed), then dispatched to
/// `command` (if set) on a detached thread that returns immediately. With neither a command nor a
/// mirror configured, `enabled` alone emits nowhere — the channel is armed but unpointed.
pub fn emit(config: &TelemetryConfig, event: &TelemetryEvent<'_>) {
    if !config.enabled {
        return;
    }
    let json = event.to_json();
    if let Some(path) = &config.mirror_file {
        if let Err(error) = append_mirror(path, &json) {
            eprintln!("telemetry: mirror_file append failed (non-fatal, ignored): {error}");
        }
    }
    dispatch(&config.command, Duration::from_millis(config.timeout_ms), json);
}

/// Dispatch `json` to the sink `command`, NEVER blocking the caller. Empty `command` ⇒ no process
/// is spawned (zero-cost no-op). Non-empty ⇒ the whole spawn+write+bounded-wait runs on its OWN
/// detached thread — reusing [`crate::announce::run_sink`], the proven bounded runner that kills a
/// hung sink at `timeout` — and this call returns immediately. Any error is logged once and
/// swallowed. Takes an owned `json` so the detached thread needs nothing from the caller.
pub fn dispatch(command: &[String], timeout: Duration, json: String) {
    if command.is_empty() {
        return; // no sink configured — nothing to spawn.
    }
    let argv = command.to_vec();
    std::thread::spawn(move || {
        if let Err(error) = crate::announce::run_sink(&argv, timeout, &json) {
            eprintln!("telemetry: sink failed (non-fatal, ignored): {error}");
        }
    });
}

/// Durable single-line append of `json` to the mirror file. Same append discipline as the episode
/// log (`OpenOptions::append` + `sync_all`); creates the parent dir and the file lazily. Errors are
/// returned to [`emit`], which logs-and-continues — a mirror is a diagnostic tap, never fail-closed.
pub fn append_mirror(path: &Path, json: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{json}")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::episode::{Episode, EpisodeKind, EpisodeLog, EpisodeOutcome, UsageRecord};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "mobee-telemetry-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        root
    }

    /// A fully-populated delivered-paid episode: every lifecycle group set with public facts. Used
    /// by the never-echo test to prove the envelope adds no secret material of its own.
    fn populated_episode() -> Episode {
        let mut ep = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredPaid,
            123,
            "seller_public_pubkey_hex",
            "job-abc",
        );
        ep.offer_task = "refactor the widget module".to_owned();
        ep.buyer_pubkey = "buyer_public_pubkey_hex".to_owned();
        ep.amount = 21;
        ep.claim_id = Some("claim-1".to_owned());
        ep.result_id = Some("result-1".to_owned());
        ep.commit_oid = Some("a".repeat(40));
        ep.harness = Some("claude".to_owned());
        ep.usage = UsageRecord {
            model: Some("claude-opus".to_owned()),
            input_tokens: Some(1000),
            output_tokens: Some(500),
            ..UsageRecord::default()
        };
        ep.amount_received = Some(21);
        ep.expected_amount = Some(21);
        ep
    }

    fn parse(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("telemetry event must be valid JSON")
    }

    #[test]
    fn envelope_wraps_the_episode_with_type_and_routing_fields() {
        let ep = populated_episode();
        let v = parse(&TelemetryEvent::episode(999, &ep).to_json());
        assert_eq!(v["v"], TELEMETRY_SCHEMA_VERSION);
        assert_eq!(v["type"], "episode");
        assert_eq!(v["ts"], 999);
        assert_eq!(v["seller_pubkey"], "seller_public_pubkey_hex");
        assert_eq!(v["job_id"], "job-abc");
        // The full episode is nested, not flattened away.
        assert_eq!(v["episode"]["outcome"], "delivered_paid");
        assert_eq!(v["episode"]["amount_received"], 21);
    }

    #[test]
    fn sink_receives_episode_envelope() {
        // A capturing sink (`tee -a <file>`) receives the JSON on stdin. Prove the emitted line is
        // exactly one well-formed `type:"episode"` envelope carrying the episode + routing fields.
        let root = temp_root("sink");
        let out = root.join("captured.jsonl");
        let config = TelemetryConfig {
            enabled: true,
            command: vec!["tee".to_owned(), "-a".to_owned(), out.display().to_string()],
            timeout_ms: 5000,
            mirror_file: None,
        };
        let ep = populated_episode();
        emit(&config, &TelemetryEvent::episode(7, &ep));

        // Bounded wait for the detached sink to land its line.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut lines: Vec<String> = Vec::new();
        while Instant::now() < deadline {
            if let Ok(body) = fs::read_to_string(&out) {
                lines = body.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
                if !lines.is_empty() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(lines.len(), 1, "expected exactly one captured event, got {lines:?}");
        let v = parse(&lines[0]);
        assert_eq!(v["type"], "episode");
        assert_eq!(v["seller_pubkey"], "seller_public_pubkey_hex");
        assert_eq!(v["job_id"], "job-abc");
        assert_eq!(v["episode"]["job_id"], "job-abc");
    }

    #[test]
    fn sink_failure_does_not_block_or_lose_episodes_jsonl_append() {
        // Ordering + fail-soft guarantee: the caller appends the episode (source of truth) FIRST,
        // THEN emits. A hung sink (`sleep 300`) must neither block emit nor corrupt/lose the
        // episodes.jsonl line. The mirror still lands (independent of the sink).
        let root = temp_root("failsoft");
        let log = EpisodeLog::open(&root);
        let ep = populated_episode();
        // 1) Source of truth: durable episode append happens first and completes.
        log.append(&ep).expect("episode append is the source of truth");

        // 2) Emit into a hung sink + a mirror, with a short bound.
        let mirror = root.join("telemetry.jsonl");
        let config = TelemetryConfig {
            enabled: true,
            command: vec!["sleep".to_owned(), "300".to_owned()],
            timeout_ms: 200,
            mirror_file: Some(mirror.clone()),
        };
        let started = Instant::now();
        emit(&config, &TelemetryEvent::episode(1, &ep));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(150),
            "emit blocked the caller for {elapsed:?} — a hung sink must never block the seller loop"
        );

        // 3) The episodes.jsonl append is intact — exactly the one line, unaffected by the sink.
        let entries = log.entries().expect("episode entries");
        assert_eq!(entries.len(), 1, "the source-of-truth append must survive a sink failure");
        assert_eq!(entries[0], ep);

        // 4) The mirror landed despite the hung sink (bounded wait).
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut mirrored = false;
        while Instant::now() < deadline {
            if let Ok(body) = fs::read_to_string(&mirror) {
                if body.lines().filter(|l| !l.is_empty()).count() == 1 {
                    mirrored = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(mirrored, "mirror_file must land the event even when the sink hangs");
    }

    #[test]
    fn never_echoes_secret_key_in_rendered_event() {
        // Safety: the telemetry envelope wraps only an Episode (ids/amounts/task-text/usage) plus
        // the PUBLIC seller pubkey + job id — it can carry no key/token/proof-secret by construction.
        // Prove it: a fully-populated episode renders with none of the secret sentinels, and the
        // top-level envelope keys are EXACTLY the allowed set (so a future field can't smuggle one).
        const SECRET_KEY_HEX: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        const CASHU_TOKEN: &str = "cashuAeyJ0b2tlbiI6";
        const PROOF_SECRET: &str = "proof_secret_0xC0FFEE";

        let ep = populated_episode();
        let json = TelemetryEvent::episode(1, &ep).to_json();
        assert!(!json.contains(SECRET_KEY_HEX), "telemetry leaked a secret key: {json}");
        assert!(!json.contains(CASHU_TOKEN), "telemetry leaked a cashu token: {json}");
        assert!(!json.contains(PROOF_SECRET), "telemetry leaked a proof secret: {json}");
        // The public seller pubkey is present (routing) — that is public, not a secret.
        assert!(json.contains("seller_public_pubkey_hex"));

        // Lock the envelope surface: exactly these top-level keys, nothing else could hold a secret.
        let v = parse(&json);
        let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["episode", "job_id", "seller_pubkey", "ts", "type", "v"],
            "unexpected top-level telemetry key — audit it for secret material"
        );
    }

    #[test]
    fn disabled_channel_emits_nothing() {
        // enabled=false ⇒ no sink spawn, no mirror write, even with both configured.
        let root = temp_root("disabled");
        let mirror = root.join("telemetry.jsonl");
        let config = TelemetryConfig {
            enabled: false,
            command: vec!["tee".to_owned(), "-a".to_owned(), root.join("sink.jsonl").display().to_string()],
            timeout_ms: 2000,
            mirror_file: Some(mirror.clone()),
        };
        let ep = populated_episode();
        emit(&config, &TelemetryEvent::episode(1, &ep));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!mirror.exists(), "disabled telemetry must not write the mirror");
        assert!(!root.join("sink.jsonl").exists(), "disabled telemetry must not spawn the sink");
    }

    #[test]
    fn mirror_only_works_without_a_sink_command() {
        // A mirror with an empty command: the event is durably appended, dispatch is a pure no-op.
        let root = temp_root("mirroronly");
        let mirror = root.join("telemetry.jsonl");
        let config = TelemetryConfig {
            enabled: true,
            command: Vec::new(),
            timeout_ms: 2000,
            mirror_file: Some(mirror.clone()),
        };
        let ep = populated_episode();
        emit(&config, &TelemetryEvent::episode(1, &ep));
        let body = fs::read_to_string(&mirror).expect("mirror written");
        let lines: Vec<&str> = body.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(parse(lines[0])["type"], "episode");
    }
}
