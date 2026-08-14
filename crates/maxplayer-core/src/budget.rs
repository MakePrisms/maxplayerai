//! MCP spend authority — budget caps before any pay reaches the payment state machine.
//!
//! Caps bind from `~/.maxplayer` config only. Tool args that try to set/override
//! `per_job` / `total` are ignored by callers; this gate never reads them.
//!
//! Spent is durable as an **append-only ledger** under `~/.maxplayer/spent.jsonl`: one
//! JSON record per spend attempt, appended (never rewritten) before the pay effect
//! (write-before-effect). The spent total is **folded over the records at read time**
//! — a fresh fold happens before every cap check. Concurrent buyer processes each
//! append their own single-line records (an `O_APPEND` write ≤ `PIPE_BUF` is atomic
//! on POSIX), so no process ever clobbers another's spend history — fixing the
//! last-writer-wins regression of the old whole-file `spent.toml` rewrite (#22).
//! Crash after append / before effect shrinks remaining allowance — fail-closed vs
//! restart-resets-allowance.
//!
//! The refresh→check→append→in-memory-update critical section is serialized ACROSS
//! processes by an advisory exclusive lock (`flock` via [`std::fs::File::lock`]) held over
//! `spent.lock` next to the ledger. Two buyers sharing one `~/.maxplayer` therefore cannot both
//! fold-then-append in a tight interleave and each pass a check their combined spend would
//! exceed — the TOCTOU is closed, so the ledger cap is a real cross-process guard (not merely an
//! accounting record backstopped by the wallet balance). The lock is released BEFORE the wallet
//! effect runs; attempt-id dedupe stays inside the locked section.
//!
//! When keyed by `attempt_id`, spent is **idempotent**: a reconciled retry of the
//! same attempt does not re-count (allowance invariant, distinct from the payment
//! journal), and the fold counts a given `attempt_id` at most once even if it appears
//! in more than one record. The durable append still happens before `run()`'s mint
//! effect on first authorize of that attempt.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::home::{MaxplayerConfig, MaxplayerHome};

/// Append-only spend ledger (one JSON record per line). Source of truth for spent.
const LEDGER_FILE: &str = "spent.jsonl";
/// Legacy whole-file total (pre-#22). Read once as an opening base, never rewritten.
const LEGACY_SPENT_FILE: &str = "spent.toml";
/// Advisory lock file guarding the refresh→check→append→update critical section across
/// processes (created on first durable reserve; never carries spend data).
const LOCK_FILE: &str = "spent.lock";

/// Fail-closed refusal — never a silent clamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetRefuse {
    /// Amount exceeds the per-job cap — the sole spend gate (issue #378 removed the rolling total cap).
    PerJob { amount: u64, per_job_cap: u64 },
    /// Durable spent persist failed — effect must not run.
    Persist(String),
}

impl std::fmt::Display for BudgetRefuse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PerJob {
                amount,
                per_job_cap,
            } => write!(
                formatter,
                "budget refused: amount {amount} exceeds per-job cap {per_job_cap}"
            ),
            Self::Persist(detail) => write!(formatter, "budget spent persist failed: {detail}"),
        }
    }
}

impl std::error::Error for BudgetRefuse {}

/// Legacy pre-#22 whole-file spent total. Read once as an opening base and folded
/// under the ledger records; never written again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LegacySpentFile {
    spent_sats: u64,
    /// Attempt ids already counted toward spent_sats (idempotent retries).
    #[serde(default)]
    attempt_ids: Vec<String>,
}

/// One appended spend record. Additive-only: new fields get serde defaults so an old
/// reader ignores unknown fields and a new reader parses an old line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LedgerRecord {
    /// Sats counted toward spent by this record.
    amount_sats: u64,
    /// Sats credited BACK (subtracted from spent) by this record — a reconciliation entry.
    /// A record is either a spend (`amount_sats > 0`, `credit_sats == 0`) or a credit
    /// (`amount_sats == 0`, `credit_sats > 0`); the fold applies add-then-saturating-sub, so a
    /// pure credit lowers spent and a pure spend raises it. Used by the cross-mint hop to return
    /// the unused Lightning fee reserve once the melt reconciles the reserve against the actual
    /// fee (MakePrisms/maxplayerai#186). A pre-#186 line has no field and defaults to 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    credit_sats: u64,
    /// Present on the real pay path; folds idempotently (counted at most once). A credit record
    /// carries its OWN reconcile key here (namespaced away from the spend's attempt id) so the
    /// credit dedupes independently of the spend it reconciles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attempt_id: Option<String>,
    /// Unix seconds at append — diagnostic only, never feeds the cap check.
    #[serde(default)]
    recorded_at: u64,
}

/// serde skip predicate: a zero credit is the common (spend) case, kept off disk so spend lines
/// stay byte-identical to pre-#186 records.
fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Spent state derived by folding the legacy base and the ledger records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FoldedSpent {
    spent: u64,
    counted_attempts: BTreeSet<String>,
}

/// Allowance gate with a durable append-only spent ledger under the packaged home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetGate {
    per_job_cap: u64,
    /// Cache of the last fold; refreshed from disk before every durable cap check and
    /// used directly as the store for the non-durable (in-memory) gate.
    spent: u64,
    /// Attempt ids already counted (cache of the last fold; in-memory store when
    /// `ledger_path` is `None`).
    counted_attempts: BTreeSet<String>,
    /// When set, spent is folded/appended here (append-before-effect). `None` = in-memory.
    ledger_path: Option<PathBuf>,
    /// Legacy pre-#22 total folded in as an opening base. `None` when no legacy file.
    legacy_base: Option<FoldedSpent>,
}

impl BudgetGate {
    /// In-memory gate (tests / callers that do not need durability).
    pub fn new(per_job_cap: u64) -> Self {
        Self {
            per_job_cap,
            spent: 0,
            counted_attempts: BTreeSet::new(),
            ledger_path: None,
            legacy_base: None,
        }
    }

    /// Per-job cap from config; spent starts at 0 and is not durable.
    pub fn from_config(config: &MaxplayerConfig) -> Self {
        Self::new(config.per_job_budget_sats)
    }

    /// Per-job cap from home config; spent folded from the append-only ledger at
    /// `~/.maxplayer/spent.jsonl` (created on first append). A legacy `spent.toml`, if
    /// present, is folded in as an opening base so no pre-#22 spend history is lost;
    /// it is left in place and never rewritten.
    pub fn from_home(home: &MaxplayerHome) -> Result<Self, BudgetRefuse> {
        let ledger_path = home.root.join(LEDGER_FILE);
        let legacy_base = load_legacy_base(&home.root.join(LEGACY_SPENT_FILE))?;
        let folded = fold_ledger(&ledger_path, legacy_base.as_ref())?;
        Ok(Self {
            per_job_cap: home.config.per_job_budget_sats,
            spent: folded.spent,
            counted_attempts: folded.counted_attempts,
            ledger_path: Some(ledger_path),
            legacy_base,
        })
    }

    pub fn per_job_cap(&self) -> u64 {
        self.per_job_cap
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    /// Path to the append-only spend ledger (`spent.jsonl`), when durable.
    pub fn spent_path(&self) -> Option<&Path> {
        self.ledger_path.as_deref()
    }

    /// True when this attempt_id was already counted toward spent.
    pub fn has_counted_attempt(&self, attempt_id: &str) -> bool {
        self.counted_attempts.contains(attempt_id)
    }

    /// Check only — does not mutate. The per-job cap is the sole spend gate (issue #378 removed the
    /// rolling total cap); it is stateless, so it does not read `spent`.
    pub fn check(&self, amount: u64) -> Result<(), BudgetRefuse> {
        if amount > self.per_job_cap {
            return Err(BudgetRefuse::PerJob {
                amount,
                per_job_cap: self.per_job_cap,
            });
        }
        Ok(())
    }

    /// Re-fold the ledger from disk (durable) into the in-memory cache, so a cap
    /// check sees spends appended by other buyer processes since this gate loaded.
    /// No-op for the in-memory gate.
    fn refresh(&mut self) -> Result<(), BudgetRefuse> {
        let Some(path) = self.ledger_path.as_ref() else {
            return Ok(());
        };
        let folded = fold_ledger(path, self.legacy_base.as_ref())?;
        self.spent = folded.spent;
        self.counted_attempts = folded.counted_attempts;
        Ok(())
    }

    /// Fail-closed check then durable append (append-before any external effect).
    pub fn authorize_and_commit(&mut self, amount: u64) -> Result<(), BudgetRefuse> {
        self.reserve(amount, None)
    }

    /// Authorize, **append the spend**, then run `effect`. Refuse leaves the ledger
    /// untouched and never calls `effect`. Append failure never calls `effect`.
    ///
    /// Always counts `amount` (no attempt key). Prefer
    /// [`Self::authorize_then_attempt`] on the real pay path so reconciled retries
    /// do not double-count spent. The cross-process lock is released before `effect` runs.
    pub fn authorize_then<T>(
        &mut self,
        amount: u64,
        effect: impl FnOnce() -> T,
    ) -> Result<T, BudgetRefuse> {
        self.reserve(amount, None)?;
        Ok(effect())
    }

    /// Authorize keyed by `attempt_id`: first sighting counts `amount` (durable
    /// append-before-effect); a retry of the same id skips re-count and still runs
    /// `effect` (reconcile / closed return). "Already counted" is judged
    /// against a fresh fold under the lock, so a spend appended by another process is respected.
    /// The cross-process lock is released before `effect` runs.
    pub fn authorize_then_attempt<T>(
        &mut self,
        attempt_id: &str,
        amount: u64,
        effect: impl FnOnce() -> T,
    ) -> Result<T, BudgetRefuse> {
        self.reserve(amount, Some(attempt_id))?;
        Ok(effect())
    }

    /// Credit an over-reserved amount back to spent, keyed by `reconcile_id` (at-most-once).
    ///
    /// The cross-mint hop charges the cap a worst-case Lightning fee reserve BEFORE the melt
    /// (fail-closed: it must pass the cap before any money moves). Once the melt settles, the fee
    /// actually paid is known and the unused reserve came back to the wallet as change — so leaving
    /// the full reserve counted understates the remaining allowance. This returns that difference
    /// (MakePrisms/maxplayerai#186).
    ///
    /// Discipline mirrors [`Self::reserve`]: the same cross-process advisory lock guards
    /// refresh→dedupe→append→update, and the credit is idempotent — a retry of the same
    /// `reconcile_id` (judged against a fresh fold under the lock) is a no-op, so a reconciliation
    /// can never be applied twice. `reconcile_id` MUST be distinct from the spend's attempt id (the
    /// caller namespaces it) so the credit dedupes independently of the spend it reconciles.
    ///
    /// Fail direction is safe: this only ever LOWERS spent, and the durable append happens before
    /// the in-memory update, so a crash mid-credit leaves spent at the higher (over-counted) value —
    /// never below real outlay. A zero credit is a no-op.
    pub fn credit_reserve(
        &mut self,
        reconcile_id: &str,
        credit_sats: u64,
    ) -> Result<(), BudgetRefuse> {
        if credit_sats == 0 {
            return Ok(());
        }
        let _lock = self.acquire_lock()?;
        self.refresh()?;
        if self.counted_attempts.contains(reconcile_id) {
            // Already reconciled and persisted — do not credit twice.
            return Ok(());
        }
        // Durable append-before in-memory update, exactly as a spend commits.
        self.append_credit(reconcile_id, credit_sats)?;
        self.spent = self.spent.saturating_sub(credit_sats);
        self.counted_attempts.insert(reconcile_id.to_owned());
        Ok(())
    }

    /// Reserve `amount` against the cap: hold the cross-process advisory lock over the whole
    /// refresh→check→append→in-memory-update section so two buyer processes sharing one home
    /// cannot both pass the cap check and both spend (TOCTOU closed). When `attempt_id` is set,
    /// an id already counted (judged against the fresh fold under the lock) is a no-op — no
    /// append, no double-count. The lock drops on return, BEFORE any caller effect (wallet send).
    fn reserve(&mut self, amount: u64, attempt_id: Option<&str>) -> Result<(), BudgetRefuse> {
        let _lock = self.acquire_lock()?;
        self.refresh()?;
        if let Some(id) = attempt_id {
            if self.counted_attempts.contains(id) {
                // Already counted and persisted — do not re-add.
                return Ok(());
            }
        }
        self.check(amount)?;
        // Durable append-before mint/effect — crash-retry cannot exceed cap.
        self.append_spend(amount, attempt_id)?;
        self.spent = self.spent.saturating_add(amount);
        if let Some(id) = attempt_id {
            self.counted_attempts.insert(id.to_owned());
        }
        Ok(())
    }

    /// Acquire the exclusive advisory lock guarding the durable critical section. `None`
    /// (no-op) for the in-memory gate — a single process with a `Mutex` needs no file lock.
    /// The returned handle holds the `flock` until it drops (end of [`Self::reserve`]).
    fn acquire_lock(&self) -> Result<Option<File>, BudgetRefuse> {
        let Some(ledger) = self.ledger_path.as_ref() else {
            return Ok(None);
        };
        let lock_path = ledger.with_file_name(LOCK_FILE);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        // Blocks until any other buyer process holding the lock releases it.
        file.lock()
            .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        Ok(Some(file))
    }

    /// Append one spend record to the ledger (durable). No-op for the in-memory gate.
    fn append_spend(&self, amount: u64, attempt_id: Option<&str>) -> Result<(), BudgetRefuse> {
        let Some(path) = self.ledger_path.as_ref() else {
            return Ok(());
        };
        append_record(
            path,
            &LedgerRecord {
                amount_sats: amount,
                credit_sats: 0,
                attempt_id: attempt_id.map(str::to_owned),
                recorded_at: now_unix(),
            },
        )
    }

    /// Append one reconciliation credit record (durable). No-op for the in-memory gate.
    fn append_credit(&self, reconcile_id: &str, credit_sats: u64) -> Result<(), BudgetRefuse> {
        let Some(path) = self.ledger_path.as_ref() else {
            return Ok(());
        };
        append_record(
            path,
            &LedgerRecord {
                amount_sats: 0,
                credit_sats,
                attempt_id: Some(reconcile_id.to_owned()),
                recorded_at: now_unix(),
            },
        )
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a legacy pre-#22 `spent.toml`, if present, as the opening base. Absent = `None`.
/// A malformed legacy file fails closed (never silently ignored — that would drop spend).
fn load_legacy_base(path: &Path) -> Result<Option<FoldedSpent>, BudgetRefuse> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    let legacy: LegacySpentFile =
        toml::from_str(&raw).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    Ok(Some(FoldedSpent {
        spent: legacy.spent_sats,
        counted_attempts: legacy.attempt_ids.into_iter().collect(),
    }))
}

/// Fold the append-only ledger over the optional legacy base into a spent total and the
/// set of counted attempt ids. A record carrying an `attempt_id` counts at most once even
/// if the id repeats (idempotent retries / cross-process double-append). A malformed line
/// fails closed — undercounting spent by skipping a record would weaken the cap.
fn fold_ledger(path: &Path, base: Option<&FoldedSpent>) -> Result<FoldedSpent, BudgetRefuse> {
    let mut folded = base.cloned().unwrap_or_default();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(folded),
        Err(error) => return Err(BudgetRefuse::Persist(error.to_string())),
    };
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: LedgerRecord = serde_json::from_str(trimmed)
            .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        // A spend raises spent; a credit (reconciliation) lowers it. `saturating_sub` floors spent
        // at 0 so a credit can never drive the total negative even if records are read out of order.
        // Bind the amounts before matching on `attempt_id` (which moves the String out of `record`).
        let amount_sats = record.amount_sats;
        let credit_sats = record.credit_sats;
        let apply =
            |spent: u64| spent.saturating_add(amount_sats).saturating_sub(credit_sats);
        match record.attempt_id {
            Some(id) => {
                if folded.counted_attempts.insert(id) {
                    folded.spent = apply(folded.spent);
                }
            }
            None => folded.spent = apply(folded.spent),
        }
    }
    Ok(folded)
}

/// Durable single-line append of one spend record. One `write_all` of a line that stays
/// well under `PIPE_BUF`, so the `O_APPEND` write is atomic on POSIX — concurrent buyers
/// never interleave partial records. `sync_all` makes it durable before the pay effect.
fn append_record(path: &Path, record: &LedgerRecord) -> Result<(), BudgetRefuse> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    }
    let mut line =
        serde_json::to_string(record).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    line.push('\n');
    // Whether the ledger's directory entry is already durable: on the FIRST spend `spent.jsonl`
    // does not exist yet, so after creating+syncing it we must also fsync the parent dir — else a
    // power-loss can drop the ledger entry while ecash has already left, restoring the full budget
    // on restart (overspend past the cap). Subsequent appends only need the file `sync_all`.
    let ledger_existed = path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    file.write_all(line.as_bytes())
        .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    file.sync_all()
        .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    if !ledger_existed {
        if let Some(parent) = path.parent() {
            crate::durable::sync_dir(parent)
                .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        }
    }
    Ok(())
}

/// Fold the durable spent total at `path` (ledger + optional legacy sibling). Test helper.
#[cfg(test)]
fn load_spent(path: &Path) -> Result<u64, BudgetRefuse> {
    let legacy = load_legacy_base(&path.with_file_name(LEGACY_SPENT_FILE))?;
    Ok(fold_ledger(path, legacy.as_ref())?.spent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-budget-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn exceed_per_job_refuses_with_distinct_error() {
        let mut gate = BudgetGate::new(21);
        let err = gate.authorize_and_commit(22).expect_err("refuse");
        assert!(matches!(
            err,
            BudgetRefuse::PerJob {
                amount: 22,
                per_job_cap: 21
            }
        ));
        assert_eq!(gate.spent(), 0);
        assert!(err.to_string().contains("per-job"));
    }

    #[test]
    fn boundary_per_job_pass_then_plus_one_refuse() {
        let mut gate = BudgetGate::new(21);
        gate.authorize_and_commit(21).expect("boundary pass");
        assert_eq!(gate.spent(), 21);
        let err = gate.authorize_and_commit(22).expect_err("plus one");
        assert!(matches!(err, BudgetRefuse::PerJob { .. }));
        assert_eq!(gate.spent(), 21);
    }

    #[test]
    fn per_job_over_cap_refuses_distinctly() {
        // Issue #378 removed the rolling total cap and its `BudgetRefuse::Total` branch; the per-job
        // cap is the sole spend gate. An amount over it refuses with the distinct `PerJob` error and
        // spends nothing. (The per-job cap is stateless — a gate may commit many in-cap spends.)
        let mut gate = BudgetGate::new(30);
        let job_err = gate.authorize_and_commit(31).expect_err("per-job");
        assert!(matches!(job_err, BudgetRefuse::PerJob { .. }));
        assert_eq!(gate.spent(), 0);
    }

    #[test]
    fn default_config_binds_the_shipped_30k_per_job_gate() {
        // Ties the SHIPPED default (30_000, #378) to the per-job gate. Reddens if the default reverts
        // OR the per-job check is removed. (from_config_binds_cap_not_tool_args uses an arbitrary 7.)
        assert_eq!(crate::home::DEFAULT_PER_JOB_BUDGET_SATS, 30_000, "shipped per-job default");
        assert_eq!(
            MaxplayerConfig::default().per_job_budget_sats,
            crate::home::DEFAULT_PER_JOB_BUDGET_SATS
        );
        assert_eq!(
            BudgetGate::from_config(&MaxplayerConfig::default()).per_job_cap(),
            30_000,
            "the gate binds the shipped default cap"
        );
        // At the shipped cap: one over refuses with PerJob and spends nothing; exactly at-cap passes.
        let mut gate = BudgetGate::new(crate::home::DEFAULT_PER_JOB_BUDGET_SATS);
        let err = gate.authorize_and_commit(30_001).expect_err("one over the shipped default refuses");
        assert!(matches!(err, BudgetRefuse::PerJob { .. }));
        assert_eq!(gate.spent(), 0);
        gate.authorize_and_commit(30_000).expect("exactly at the shipped default passes");
        assert_eq!(gate.spent(), 30_000);
    }

    #[test]
    fn refuse_before_effect() {
        let mut gate = BudgetGate::new(10);
        let mut fired = false;
        let err = gate
            .authorize_then(11, || {
                fired = true;
                "paid"
            })
            .expect_err("refuse");
        assert!(!fired);
        assert_eq!(gate.spent(), 0);
        assert!(matches!(err, BudgetRefuse::PerJob { .. }));

        let out = gate
            .authorize_then(10, || {
                fired = true;
                "paid"
            })
            .expect("allow");
        assert!(fired);
        assert_eq!(out, "paid");
        assert_eq!(gate.spent(), 10);
    }

    // Issue #378 removed the total cap this once proved; the serialization it rode on now protects
    // attempt-id idempotency. Eight concurrent committers of the SAME attempt id, serialized behind
    // one lock, must count the spend exactly ONCE — never eight times — while every call still
    // returns Ok (an idempotent replay succeeds).
    #[test]
    fn concurrent_same_attempt_id_counts_spent_once() {
        let gate = Arc::new(Mutex::new(BudgetGate::new(50)));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let gate = Arc::clone(&gate);
            handles.push(thread::spawn(move || {
                let mut guard = gate.lock().expect("lock");
                guard.authorize_then_attempt("shared-id", 50, || ()).is_ok()
            }));
        }
        let oks: usize = handles
            .into_iter()
            .map(|handle| usize::from(handle.join().expect("join")))
            .sum();
        let gate = gate.lock().expect("lock");
        assert_eq!(oks, 8, "every committer of a counted attempt returns Ok idempotently");
        assert_eq!(gate.spent(), 50, "the shared attempt id is counted exactly once, never 8×");
        assert!(gate.has_counted_attempt("shared-id"));
    }

    #[test]
    fn from_config_binds_cap_not_tool_args() {
        let config = MaxplayerConfig {
            per_job_budget_sats: 7,
            ..MaxplayerConfig::default()
        };
        let gate = BudgetGate::from_config(&config);
        assert_eq!(gate.per_job_cap(), 7);
        assert_ne!(gate.per_job_cap(), 999);
    }

    #[test]
    fn durable_spent_survives_reload_write_before_effect() {
        let root = temp_home("durable");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let spent_path = gate.spent_path().expect("path").to_path_buf();
        let mut effect_fired = false;
        gate.authorize_then(21, || {
            // Spent must already be durable before effect runs.
            let on_disk = load_spent(&spent_path).expect("load");
            assert_eq!(on_disk, 21);
            effect_fired = true;
            "ok"
        })
        .expect("allow");
        assert!(effect_fired);
        assert_eq!(gate.spent(), 21);

        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 21);
    }

    // #186 RED-PROVE: mirror the hop pay flow — charge the worst-case reserve, then reconcile against
    // the actual fee. The credit must lower spent to real outlay, survive reload (durable), and be
    // idempotent (a retried reconciliation credits at most once). Before #186 there was no
    // credit_reserve at all, so spent stayed pinned at the worst-case charge.
    #[test]
    fn hop_fee_reserve_credit_reconciles_spent_to_real_outlay_durably_and_at_most_once() {
        let root = temp_home("hop-reconcile");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let mut gate = BudgetGate::from_home(&home).expect("gate");

        // Charge the hop's worst-case planned cost: 100 delivered + 7 reserved fee + 2 input fee.
        gate.authorize_then_attempt("attempt-1", 109, || "paid")
            .expect("charge planned cost");
        assert_eq!(gate.spent(), 109, "worst-case reservation is counted first");

        // The melt actually paid 2 sats of the 7 reserved; credit the 5-sat difference back.
        let reconcile_id = "attempt-1:hop-fee-reconcile";
        gate.credit_reserve(reconcile_id, 5).expect("credit unused reserve");
        assert_eq!(gate.spent(), 104, "spent must reflect real outlay after reconcile");

        // Durable: a fresh gate folds the credit from disk.
        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 104, "the credit must survive reload");

        // At-most-once: replaying the same reconciliation is a no-op — never a second credit.
        gate.credit_reserve(reconcile_id, 5).expect("idempotent replay");
        assert_eq!(gate.spent(), 104, "a repeated reconcile must not credit twice");

        // The spend's own attempt id is a DISTINCT dedupe key from the reconcile id.
        assert!(gate.has_counted_attempt("attempt-1"));
        assert!(gate.has_counted_attempt(reconcile_id));
    }

    #[test]
    fn durable_refuse_leaves_spent_file_unchanged() {
        let root = temp_home("refuse-persist");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        gate.authorize_and_commit(10).expect("seed");
        let err = gate
            .authorize_then(home.config.per_job_budget_sats + 1, || "nope")
            .expect_err("refuse");
        assert!(matches!(err, BudgetRefuse::PerJob { .. }));
        assert_eq!(gate.spent(), 10);
        assert_eq!(load_spent(gate.spent_path().expect("path")).expect("load"), 10);
    }

    #[test]
    fn attempt_id_retry_does_not_double_count_spent() {
        let mut gate = BudgetGate::new(50);
        let mut fires = 0u32;
        gate.authorize_then_attempt("att-1", 21, || {
            fires += 1;
            "first"
        })
        .expect("first");
        assert_eq!(gate.spent(), 21);
        assert!(gate.has_counted_attempt("att-1"));

        let out = gate
            .authorize_then_attempt("att-1", 21, || {
                fires += 1;
                "retry"
            })
            .expect("retry");
        assert_eq!(out, "retry");
        assert_eq!(fires, 2);
        assert_eq!(gate.spent(), 21, "reconciled retry must not re-count");

        gate.authorize_then_attempt("att-2", 21, || "other")
            .expect("other attempt");
        assert_eq!(gate.spent(), 42);
    }

    // Constraint #4 (operator completion): the original award charges the attempt; the operator
    // completion routes the SAME attempt id through `authorize_then_attempt` AGAIN (passing the
    // amount again, because the token is REUSED not re-minted). The gate no-ops the second reserve —
    // the completion effect still runs, but spent does NOT move. This is the exact gate interaction
    // `complete_recovered_locked_async` relies on to avoid double-charging a reused token.
    #[test]
    fn operator_completion_reattempt_reuses_the_charge_without_double_counting() {
        let mut gate = BudgetGate::new(100);
        gate.authorize_then_attempt("attempt-x", 40, || ())
            .expect("original award charges");
        assert_eq!(gate.spent(), 40);
        assert!(gate.has_counted_attempt("attempt-x"));

        let completed = gate
            .authorize_then_attempt("attempt-x", 40, || "completed")
            .expect("completion runs its effect");
        assert_eq!(completed, "completed", "completion effect must still run");
        assert_eq!(
            gate.spent(),
            40,
            "completion reuses the already-charged attempt — no second charge"
        );
    }

    #[test]
    fn attempt_id_write_before_effect_and_survives_reload() {
        let root = temp_home("attempt-durable");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let spent_path = gate.spent_path().expect("path").to_path_buf();
        let mut effect_fired = false;
        gate.authorize_then_attempt("att-live", 7, || {
            let on_disk = fold_ledger(&spent_path, None).expect("load");
            assert_eq!(on_disk.spent, 7);
            assert!(on_disk.counted_attempts.contains("att-live"));
            effect_fired = true;
            "ok"
        })
        .expect("allow");
        assert!(effect_fired);

        // Crash-retry window: reload then retry same attempt — spent stays 7.
        let mut reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 7);
        reloaded
            .authorize_then_attempt("att-live", 7, || "retry")
            .expect("retry");
        assert_eq!(reloaded.spent(), 7);
        assert_eq!(load_spent(&spent_path).expect("disk"), 7);
    }

    // #22 regression: two independently-opened gates (simulating two buyer processes)
    // interleave spends against the same home. Because each gate appends its own record
    // and re-folds the ledger before each check, the final fold equals the SUM of ALL
    // spends — never one writer's stale view. Under the old whole-file rewrite, gate_b's
    // load-at-start snapshot clobbered gate_a's writes (last-writer-wins) and a reload
    // showed only the last writer's total.
    #[test]
    fn two_handles_interleaved_spends_fold_to_full_sum() {
        let root = temp_home("two-handle");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        // per_job high enough that all four spends fit (issue #378 removed the total cap).
        let mut home = home;
        home.config.per_job_budget_sats = 100;

        // Both handles load at the same "start" — each sees spent == 0.
        let mut gate_a = BudgetGate::from_home(&home).expect("gate a");
        let mut gate_b = BudgetGate::from_home(&home).expect("gate b");
        assert_eq!(gate_a.spent(), 0);
        assert_eq!(gate_b.spent(), 0);

        // Interleave: a, b, a, b — distinct attempt ids.
        gate_a.authorize_then_attempt("a-1", 10, || ()).expect("a-1");
        gate_b.authorize_then_attempt("b-1", 20, || ()).expect("b-1");
        gate_a.authorize_then_attempt("a-2", 30, || ()).expect("a-2");
        gate_b.authorize_then_attempt("b-2", 40, || ()).expect("b-2");

        // gate_b's last op refolds the ledger (seeing a-1/b-1/a-2) before appending b-2,
        // so its cache reflects the full shared total — not just its own two spends.
        assert_eq!(gate_b.spent(), 100, "gate_b saw a's spends via refold");

        // A fresh reload folds the FULL history — no record was clobbered.
        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 100, "10+20+30+40 = all four spends");
        for id in ["a-1", "a-2", "b-1", "b-2"] {
            assert!(reloaded.has_counted_attempt(id), "missing {id}");
        }
    }

    // A legacy pre-#22 spent.toml is folded in as an opening base: its total and attempt
    // ids survive, the file is left in place (never zeroed), and new spends append to the
    // ledger on top of the base.
    #[test]
    fn legacy_spent_toml_migrates_as_opening_base() {
        let root = temp_home("legacy");
        let _ = fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home.config.per_job_budget_sats = 100;

        // Seed a legacy whole-file total with an already-counted attempt id.
        let legacy_path = home.root.join(LEGACY_SPENT_FILE);
        write_legacy(&legacy_path, 100, &["old-1"]);

        let mut gate = BudgetGate::from_home(&home).expect("gate");
        assert_eq!(gate.spent(), 100, "legacy total folded as base");
        assert!(gate.has_counted_attempt("old-1"));

        // A retry of the legacy attempt must not re-count.
        gate.authorize_then_attempt("old-1", 50, || ())
            .expect("legacy retry");
        assert_eq!(gate.spent(), 100, "legacy attempt id is idempotent");

        // A new spend appends on top of the base.
        gate.authorize_then_attempt("new-1", 25, || ()).expect("new");
        assert_eq!(gate.spent(), 125);

        // Legacy file left in place, never zeroed.
        assert!(legacy_path.exists(), "legacy file must not be removed");
        assert_eq!(
            load_legacy_base(&legacy_path).expect("legacy").expect("some").spent,
            100,
            "legacy file must not be zeroed"
        );

        // Reload folds base + ledger.
        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 125);
    }

    // The fold counts a repeated attempt_id at most once (idempotent across duplicate
    // appends), while records without an attempt id always count.
    #[test]
    fn fold_dedups_repeated_attempt_ids_but_counts_keyless() {
        let root = temp_home("fold-dedup");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let ledger = home.root.join(LEDGER_FILE);
        append_record(
            &ledger,
            &LedgerRecord { amount_sats: 10, credit_sats: 0, attempt_id: Some("x".into()), recorded_at: 0 },
        )
        .expect("append x");
        // Duplicate append of the same attempt id — must fold once.
        append_record(
            &ledger,
            &LedgerRecord { amount_sats: 10, credit_sats: 0, attempt_id: Some("x".into()), recorded_at: 0 },
        )
        .expect("append x dup");
        // Keyless record — always counts.
        append_record(
            &ledger,
            &LedgerRecord { amount_sats: 5, credit_sats: 0, attempt_id: None, recorded_at: 0 },
        )
        .expect("append keyless");

        let folded = fold_ledger(&ledger, None).expect("fold");
        assert_eq!(folded.spent, 15, "x once (10) + keyless (5)");
        assert!(folded.counted_attempts.contains("x"));
    }

    // A malformed ledger line fails the fold closed — undercounting spent by skipping a
    // record would silently weaken the cap.
    #[test]
    fn malformed_ledger_line_fails_closed() {
        let root = temp_home("malformed");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let ledger = home.root.join(LEDGER_FILE);
        append_record(
            &ledger,
            &LedgerRecord { amount_sats: 10, credit_sats: 0, attempt_id: None, recorded_at: 0 },
        )
        .expect("append");
        {
            let mut f = OpenOptions::new().append(true).open(&ledger).expect("open");
            f.write_all(b"{not valid json\n").expect("corrupt");
        }
        let err = fold_ledger(&ledger, None).expect_err("must fail closed");
        assert!(matches!(err, BudgetRefuse::Persist(_)));
    }

    // Fix O (repurposed for #378): the cross-process lock still serializes the fold→check→append.
    // The total cap it used to enforce is gone; the property it now protects is attempt-id append
    // integrity. "Process 1" holds the advisory lock (its critical section). A second handle's
    // `authorize_then_attempt` for the SAME attempt id MUST block on the lock rather than
    // fold-then-append against a stale (empty) view. While blocked, process 1 appends that attempt's
    // 40-sat record and releases; the second handle then refolds (sees the attempt already counted)
    // and appends NOTHING — one record on disk, counted once. Reverting the lock lets the second
    // handle run before process-1's append lands: it does NOT block (its completion signal arrives,
    // failing the block assertion) and appends a duplicate — so this test goes red on revert.
    #[test]
    fn budget_lock_serializes_reserve_and_dedupes_attempt_id() {
        use std::sync::mpsc;
        use std::time::Duration;

        let root = temp_home("budget-lock");
        let _ = fs::remove_dir_all(&root);
        let mut home = home::bootstrap(&root).expect("bootstrap");
        home.config.per_job_budget_sats = 40;

        let ledger = root.join(LEDGER_FILE);
        let lock_path = ledger.with_file_name(LOCK_FILE);
        // Process 1 enters its critical section: hold the exclusive advisory lock.
        let held = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .expect("open lock");
        held.lock().expect("hold lock");

        let (tx, rx) = mpsc::channel();
        let home2 = home.clone();
        let handle = thread::spawn(move || {
            let mut gate = BudgetGate::from_home(&home2).expect("gate");
            let result = gate.authorize_then_attempt("shared", 40, || ());
            tx.send(()).expect("signal completion");
            result.map(|()| gate.spent())
        });

        // Process 2 must be blocked on the lock — no completion signal yet.
        assert!(
            rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "reserve ran while another process held the lock (TOCTOU not closed)"
        );

        // Process 1 records the SAME attempt's 40-sat spend, then releases the lock.
        append_record(
            &ledger,
            &LedgerRecord { amount_sats: 40, credit_sats: 0, attempt_id: Some("shared".into()), recorded_at: 0 },
        )
        .expect("process-1 spend");
        held.unlock().expect("release lock");

        // Process 2 now refolds (sees "shared" already counted) and dedupes: no second append.
        let spent = handle.join().expect("join").expect("second commit is idempotent");
        assert_eq!(spent, 40, "the shared attempt id is counted once, not twice");
        let records = fs::read_to_string(&ledger)
            .expect("read ledger")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        assert_eq!(
            records, 1,
            "exactly one append survives — the lock closed the duplicate-append window"
        );
        let _ = fs::remove_dir_all(&root);
    }

    fn write_legacy(path: &Path, spent: u64, attempts: &[&str]) {
        let file = LegacySpentFile {
            spent_sats: spent,
            attempt_ids: attempts.iter().map(|s| s.to_string()).collect(),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, toml::to_string_pretty(&file).expect("ser")).expect("write legacy");
    }
}
