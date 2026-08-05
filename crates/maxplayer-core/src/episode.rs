//! Layer-0 capture: `episodes.jsonl` — one append-only, schema-versioned episode per
//! job the seller **classified** (claimed or refused).
//!
//! This is a **separate stream** from `seller-journal.jsonl`. It NEVER writes to or re-owns any
//! money-safety fact: episodes carry rich per-job context and *reference* the journal by
//! `job_id`/`result_id` (they join, never duplicate). The journal stays the sole source of truth
//! for claim/receipt/release.
//!
//! **Not money-adjacent.** Episode fields (`usage`/`cost`, self-reported figures) are diagnostic
//! and MUST never feed the pay gate, the receipt bind, or any verify decision. Writing an episode
//! is best-effort at the call site: a write failure must never fail a trade.
//!
//! Same durable-append discipline as the journal (`OpenOptions::append` + `sync_all`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version for [`Episode`]. Additive-only evolution: new fields are ADDED with serde
/// defaults, never removed or repurposed; a newer reader parses an older line, and an older
/// reader ignores unknown newer fields.
pub const EPISODE_SCHEMA_VERSION: u32 = 1;

const EPISODES_FILE: &str = "episodes.jsonl";

/// Whether the seller claimed the offer or refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeKind {
    Claimed,
    Refused,
}

/// Terminal state of the job the episode records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    /// Delivered a result-kind result AND the payment redeemed (journal Receipt written).
    DeliveredPaid,
    /// Delivered a result-kind result but dropped from the awaiting-payment backlog unpaid.
    DeliveredUnpaid,
    /// Refused at classify (never claimed).
    Refused,
    /// Claimed but failed before delivery (agent/git/publish error or deadline).
    Errored,
}

/// Serde-friendly mirror of `driver::UsageMetadata` (which is not itself serde). Opportunistic:
/// every field is optional and **absent-stays-absent** (never zero-filled). Built at the call
/// site from a `UsageMetadata` so this module stays a pure data type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

impl UsageRecord {
    /// True when no field carries a value — nothing real was captured (skip serializing it whole).
    pub fn is_empty(&self) -> bool {
        *self == UsageRecord::default()
    }
}

/// One captured job. Fields are grouped by lifecycle point; everything past the envelope is
/// optional so a refused episode (no claim/delivery facts) and a claimed-but-errored episode
/// (no delivery/outcome-payment facts) are both representable in one shape. Additive-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    // ── Envelope (always present) ────────────────────────────────────────────────────────────
    pub schema_version: u32,
    pub episode_kind: EpisodeKind,
    pub captured_at: u64,
    pub seller_pubkey: String,

    // ── Offer facts (known at classify; present for any parseable offer) ──────────────────────
    pub job_id: String,
    #[serde(default)]
    pub offer_task: String,
    #[serde(default)]
    pub output_type: String,
    #[serde(default)]
    pub amount: u64,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub mint: String,
    #[serde(default)]
    pub deadline_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_target: Option<String>,
    #[serde(default)]
    pub buyer_pubkey: String,
    #[serde(default)]
    pub job_class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contribution_base_oid: Option<String>,
    #[serde(default)]
    pub configured_rate_sats: u64,

    // ── Refusal facts (episode_kind = refused) ───────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal_reason: Option<String>,

    // ── Claim facts (episode_kind = claimed) ─────────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_deadline_unix: Option<u64>,

    // ── Delivery facts (claimed jobs that reached result-kind publish) ──────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_git_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_kind: Option<String>,
    #[serde(default, skip_serializing_if = "UsageRecord::is_empty")]
    pub usage: UsageRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_ts: Option<u64>,

    // ── Outcome (terminal state) ─────────────────────────────────────────────────────────────
    pub outcome: EpisodeOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_received: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_amount: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect_ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
}

impl Episode {
    /// A minimal envelope with the required fields; callers fill the lifecycle groups they know.
    pub fn new(
        episode_kind: EpisodeKind,
        outcome: EpisodeOutcome,
        captured_at: u64,
        seller_pubkey: impl Into<String>,
        job_id: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: EPISODE_SCHEMA_VERSION,
            episode_kind,
            captured_at,
            seller_pubkey: seller_pubkey.into(),
            job_id: job_id.into(),
            offer_task: String::new(),
            output_type: String::new(),
            amount: 0,
            unit: String::new(),
            mint: String::new(),
            deadline_unix: 0,
            offer_target: None,
            buyer_pubkey: String::new(),
            job_class: String::new(),
            contribution_target: None,
            contribution_base_oid: None,
            configured_rate_sats: 0,
            refusal_reason_code: None,
            refusal_reason: None,
            claim_id: None,
            claim_ts: None,
            resolved_deadline_unix: None,
            result_id: None,
            commit_oid: None,
            fork_git_remote: None,
            fork_branch: None,
            delivery_kind: None,
            usage: UsageRecord::default(),
            wall_time_ms: None,
            harness: None,
            transcript_ref: None,
            deliver_ts: None,
            outcome,
            amount_received: None,
            expected_amount: None,
            swap_ok: None,
            collect_ts: None,
            error_reason: None,
        }
    }
}

/// Append-only episode log: a sibling of `seller-journal.jsonl` under `MOBEE_HOME`.
///
/// Read-tolerant: [`entries`](Self::entries) SKIPS a line it cannot parse (a forward-compat or
/// partially-written line must never wedge a reader — episodes are diagnostic, not fail-closed
/// money state), unlike the journal which fails closed. Writes are durable (append + fsync).
pub struct EpisodeLog {
    path: PathBuf,
}

impl EpisodeLog {
    /// Path of the episode file for a given `MOBEE_HOME` root.
    pub fn path_for(home_root: &Path) -> PathBuf {
        home_root.join(EPISODES_FILE)
    }

    /// Open (creating the file lazily on first append). The parent dir must already exist —
    /// it is `MOBEE_HOME`, always bootstrapped before a seller runs.
    pub fn open(home_root: &Path) -> Self {
        Self {
            path: Self::path_for(home_root),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Durable single-line append (one JSON object). Mirrors the journal's append discipline:
    /// `OpenOptions::append` + `sync_all`. Never writes token/key material — episodes carry ids,
    /// amounts, mint, task text, and self-reported usage only.
    pub fn append(&self, episode: &Episode) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(episode).map_err(io::Error::other)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        file.sync_all()?;
        Ok(())
    }

    /// Every parseable episode line, oldest first. Corrupt/forward-compat lines are skipped
    /// (diagnostic stream — never fail-closed). Returns an empty vec when the file is absent.
    pub fn entries(&self) -> io::Result<Vec<Episode>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(episode) = serde_json::from_str::<Episode>(trimmed) {
                out.push(episode);
            }
        }
        Ok(out)
    }

    /// The most recent `delivered_paid` episode for `job_id`, if any — the retro's seed
    /// (a fresh turn seeded with the episode + transcript_ref).
    pub fn last_delivered_paid(&self, job_id: &str) -> io::Result<Option<Episode>> {
        Ok(self
            .entries()?
            .into_iter()
            .rev()
            .find(|e| e.job_id == job_id && e.outcome == EpisodeOutcome::DeliveredPaid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "mobee-episode-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("mkdir");
        root
    }

    #[test]
    fn append_then_read_round_trips_and_is_durable_jsonl() {
        let root = temp_root("rt");
        let log = EpisodeLog::open(&root);
        let mut ep = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredPaid,
            123,
            "sellerpk",
            "job-1",
        );
        ep.result_id = Some("res-1".into());
        ep.commit_oid = Some("a".repeat(40));
        ep.amount_received = Some(21);
        ep.transcript_ref = Some("seller-jobs/job-1/seller-run.jsonl".into());
        log.append(&ep).expect("append");

        let entries = log.entries().expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], ep);
        // Exactly one line, newline-terminated.
        let raw = fs::read_to_string(log.path()).expect("read");
        assert_eq!(raw.lines().count(), 1);
        assert!(raw.ends_with('\n'));
    }

    #[test]
    fn entries_skips_corrupt_lines_and_tolerates_absent_file() {
        let root = temp_root("skip");
        let log = EpisodeLog::open(&root);
        // Absent file ⇒ empty, never an error.
        assert!(log.entries().expect("absent ok").is_empty());

        let ep = Episode::new(
            EpisodeKind::Refused,
            EpisodeOutcome::Refused,
            9,
            "pk",
            "job-x",
        );
        log.append(&ep).expect("append");
        // Inject a corrupt line — a reader must skip it, not wedge.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(log.path())
                .expect("open");
            writeln!(f, "{{not valid json").expect("write junk");
        }
        let entries = log.entries().expect("entries tolerant");
        assert_eq!(entries.len(), 1, "corrupt line skipped, good line kept");
        assert_eq!(entries[0].job_id, "job-x");
    }

    #[test]
    fn refused_episode_serializes_without_delivery_or_usage_noise() {
        // absent-stays-absent: a refused episode must not carry zero-filled delivery/usage fields.
        let ep = {
            let mut e = Episode::new(
                EpisodeKind::Refused,
                EpisodeOutcome::Refused,
                1,
                "pk",
                "job-r",
            );
            e.refusal_reason_code = Some("RateGate".into());
            e.refusal_reason = Some("rate-gate: below floor".into());
            e
        };
        let json = serde_json::to_string(&ep).expect("ser");
        assert!(json.contains("\"episode_kind\":\"refused\""));
        assert!(json.contains("\"refusal_reason_code\":\"RateGate\""));
        assert!(!json.contains("result_id"), "no delivery noise: {json}");
        assert!(!json.contains("\"usage\""), "no empty usage block: {json}");
    }

    #[test]
    fn last_delivered_paid_finds_the_newest_for_the_job() {
        let root = temp_root("last-paid");
        let log = EpisodeLog::open(&root);
        // A refused + an errored + two paid for the same job (retro re-see is possible).
        log.append(&Episode::new(
            EpisodeKind::Refused,
            EpisodeOutcome::Refused,
            1,
            "pk",
            "other",
        ))
        .unwrap();
        let mut paid_old = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::DeliveredPaid,
            2,
            "pk",
            "job-p",
        );
        paid_old.result_id = Some("old".into());
        log.append(&paid_old).unwrap();
        let mut paid_new = paid_old.clone();
        paid_new.captured_at = 3;
        paid_new.result_id = Some("new".into());
        log.append(&paid_new).unwrap();

        let found = log.last_delivered_paid("job-p").expect("read").expect("some");
        assert_eq!(found.result_id.as_deref(), Some("new"));
        assert!(log.last_delivered_paid("nope").expect("read").is_none());
    }

    #[test]
    fn schema_version_is_one_and_unknown_fields_are_tolerated() {
        // An older reader must ignore an unknown (future) field — additive-only evolution.
        let root = temp_root("ver");
        let log = EpisodeLog::open(&root);
        let ep = Episode::new(
            EpisodeKind::Claimed,
            EpisodeOutcome::Errored,
            1,
            "pk",
            "job-v",
        );
        assert_eq!(ep.schema_version, EPISODE_SCHEMA_VERSION);
        log.append(&ep).unwrap();
        {
            let mut f = OpenOptions::new().append(true).open(log.path()).unwrap();
            writeln!(
                f,
                r#"{{"schema_version":2,"episode_kind":"claimed","captured_at":5,"seller_pubkey":"pk","job_id":"job-future","outcome":"errored","some_v2_field":"ignored"}}"#
            )
            .unwrap();
        }
        let entries = log.entries().expect("entries");
        assert_eq!(entries.len(), 2, "reader parses a newer line, ignoring extras");
        assert_eq!(entries[1].job_id, "job-future");
    }
}
