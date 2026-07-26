//! The buyer's durable application state: `$MOBEE_HOME/buyer.sqlite`.
//!
//! Opened only by the daemon (guaranteed single-owner by the home lock). This is
//! the state home the later phases build on — the reservation ledger, payment
//! attempts, and lifecycle tables all land here. Step 1 ships the minimal shell:
//! a `buyer_meta` schema-version row and a `jobs` stub table, in WAL mode with
//! foreign keys and `synchronous=FULL` so the money-adjacent state that follows
//! inherits crash-safe defaults from day one.
//!
//! `rusqlite`'s [`Connection`] is `Send` but not `Sync`; the store keeps it behind
//! a mutex and callers reach it from the async runtime via `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::reservations::{
    available_breakdown, compute_available, Converted, Dispositions, JobDisposition,
    ReconcileReport, Released, Reserved, ReservationState, ReserveRefused,
};

/// Current on-disk schema version.
///
/// - v1 — the daemon shell: `buyer_meta` + `jobs` stub (#131).
/// - v2 — the reservation ledger: the `reservations` table (#123). Upgrade is forward-only and
///   additive (a new `CREATE TABLE IF NOT EXISTS` + a monotone version bump); a v1 DB opened by a
///   v2 binary gains the table and the version moves to 2 with no data migration.
/// - v3 — the pending-award intents: the `pending_awards` table (#126/#127 background auto-award).
///   One row per posted job the daemon still owes an award; re-armed on restart. Same additive
///   forward-only upgrade.
pub const SCHEMA_VERSION: i64 = 3;

/// A cloneable handle to the daemon-owned SQLite state.
#[derive(Clone)]
pub struct BuyerStore {
    conn: Arc<Mutex<Connection>>,
}

/// Store open / query failure.
#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "buyer store error: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self(value.to_string())
    }
}

/// A point-in-time view of the store for `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub schema_version: i64,
    pub started_at_unix: i64,
    pub jobs: i64,
}

impl BuyerStore {
    /// Open (creating if absent) the state DB at `path` with WAL + crash-safe pragmas
    /// and ensure the schema is present.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path.as_ref())?;
        // WAL for concurrent reads alongside the single writer; FULL sync + FK
        // enforcement because this DB will hold money-adjacent ledger state. A
        // bounded busy timeout avoids an immediate SQLITE_BUSY under contention.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS buyer_meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- Lifecycle table stub. Later phases add reservation/attempt columns
             -- and the state machine; step 1 only proves the DB is the daemon's.
             CREATE TABLE IF NOT EXISTS jobs (
                 job_id          TEXT PRIMARY KEY,
                 status          TEXT NOT NULL,
                 created_at_unix INTEGER NOT NULL
             );
             -- v2: the reservation ledger. One row per job (job_id UNIQUE via PRIMARY KEY);
             -- `state` is the reservation lifecycle; `reserved` is the ONLY state counted toward
             -- the in-flight `reserved` term. The CHECK freezes the state domain at the DB.
             CREATE TABLE IF NOT EXISTS reservations (
                 job_id          TEXT PRIMARY KEY,
                 amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                 state           TEXT NOT NULL CHECK (state IN ('reserved','spent','released')),
                 created_at_unix INTEGER NOT NULL,
                 updated_at_unix INTEGER NOT NULL
             );
             -- v3: pending auto-award intents. One row per posted job the daemon still owes an
             -- award. `pending` = awaiting a payable claim; `awarded` = a 3405 was published;
             -- `parked` = could not award (reason surfaced). Re-armed on restart so a job posted
             -- before a crash still gets its award with zero manual commands.
             CREATE TABLE IF NOT EXISTS pending_awards (
                 job_id          TEXT PRIMARY KEY,
                 max_sats        INTEGER NOT NULL CHECK (max_sats >= 0),
                 harness         TEXT,
                 model           TEXT,
                 state           TEXT NOT NULL CHECK (state IN ('pending','awarded','parked')),
                 reason          TEXT,
                 created_at_unix INTEGER NOT NULL,
                 updated_at_unix INTEGER NOT NULL
             );",
        )?;
        // Forward-only, monotone schema-version bump. A fresh DB is stamped at SCHEMA_VERSION; a
        // pre-existing lower version is upgraded to it; a (hypothetical) higher version is left
        // untouched (never downgraded). Idempotent on repeated opens.
        conn.execute(
            "INSERT INTO buyer_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE CAST(buyer_meta.value AS INTEGER) < CAST(excluded.value AS INTEGER)",
            [SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Record (idempotently overwrite) the daemon's most recent start time.
    pub fn record_start(&self, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO buyer_meta (key, value) VALUES ('started_at_unix', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [now_unix.to_string()],
        )?;
        Ok(())
    }

    /// Read the current health view for `status`.
    pub fn health(&self) -> Result<HealthSnapshot, StoreError> {
        let conn = self.lock()?;
        let schema_version = read_meta_i64(&conn, "schema_version")?.unwrap_or(0);
        let started_at_unix = read_meta_i64(&conn, "started_at_unix")?.unwrap_or(0);
        let jobs = conn.query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get::<_, i64>(0))?;
        Ok(HealthSnapshot {
            schema_version,
            started_at_unix,
            jobs,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError("state DB mutex poisoned".into()))
    }

    // ---- Reservation ledger (#123) ------------------------------------------------------------
    //
    // Every mutation opens a `BEGIN IMMEDIATE` transaction so the available-check and the write are
    // ONE write-locked step. Two concurrent awards therefore serialize: the second blocks on the
    // write lock, then reads the first's committed reservation and refuses if the two together
    // would exceed available. Read-only views (`available`, `reserved_in_flight`, …) run outside a
    // transaction under the connection mutex.

    /// Reserve `amount` sats for `job_id`, refusing atomically if it would exceed
    /// `available = min(balance − reserved, total_cap − spent − reserved)` (see the module docs
    /// for the two-ceiling model). On refusal ZERO is written. Re-reserving the same amount for a
    /// still-`Reserved` job is an idempotent no-op; a previously-`Released` row is re-reserved
    /// (subject to the check); a `Spent` row is refused.
    ///
    /// `balance` (live wallet ecash), `total_cap` (budget policy cap), and `spent` (budget ledger
    /// total) are snapshots the caller supplies — the store does not open the wallet or the budget
    /// ledger. The transaction serializes only the `reserved` accumulation, which is the sole
    /// quantity concurrent awards race on.
    pub fn reserve(
        &self,
        job_id: &str,
        amount: u64,
        balance: u64,
        total_cap: u64,
        spent: u64,
        now_unix: i64,
    ) -> Result<Reserved, ReserveRefused> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| ReserveRefused::Store("state DB mutex poisoned".into()))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| ReserveRefused::Store(error.to_string()))?;

        // Existing row for this job? An idempotent re-award / a refused conflict short-circuits here
        // BEFORE the available-check so it never (double-)counts itself.
        if let Some((state, existing)) =
            read_state(&tx, job_id).map_err(|error| ReserveRefused::Store(error.to_string()))?
        {
            match state {
                ReservationState::Reserved => {
                    if existing != amount {
                        return Err(ReserveRefused::AmountMismatch {
                            job_id: job_id.to_owned(),
                            existing,
                            requested: amount,
                        });
                    }
                    // Same amount already reserved — idempotent replay, no new commitment.
                    tx.commit()
                        .map_err(|error| ReserveRefused::Store(error.to_string()))?;
                    return Ok(Reserved::Idempotent);
                }
                ReservationState::Spent => {
                    return Err(ReserveRefused::AlreadySpent {
                        job_id: job_id.to_owned(),
                    });
                }
                // Released: fall through and re-reserve, subject to the available-check. (A released
                // row is not counted in `reserved`, so the check below correctly excludes it.)
                ReservationState::Released => {}
            }
        }

        // The available-check + the reserve write are ONE transaction. `reserved` sums only
        // `Reserved`-state rows and therefore excludes this job (fresh, or currently released).
        let reserved =
            sum_reserved(&tx).map_err(|error| ReserveRefused::Store(error.to_string()))?;
        let breakdown = available_breakdown(balance, total_cap, reserved, spent);
        if amount > breakdown.available {
            // Refuse with ZERO written — the transaction rolls back on drop, so no released→reserved
            // flip and no INSERT leak.
            return Err(ReserveRefused::InsufficientAvailable {
                requested: amount,
                available: breakdown.available,
                bound: breakdown.bound,
            });
        }

        tx.execute(
            "INSERT INTO reservations (job_id, amount_sats, state, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, 'reserved', ?3, ?3)
             ON CONFLICT(job_id) DO UPDATE SET
                 amount_sats = excluded.amount_sats,
                 state = 'reserved',
                 updated_at_unix = excluded.updated_at_unix",
            params![job_id, amount as i64, now_unix],
        )
        .map_err(|error| ReserveRefused::Store(error.to_string()))?;
        tx.commit()
            .map_err(|error| ReserveRefused::Store(error.to_string()))?;
        Ok(Reserved::New {
            available_before: breakdown.available,
        })
    }

    /// Release `job_id`'s reservation so its funds become available again. Idempotent: only a
    /// `Reserved` row is freed; `Released`/`Spent`/absent are no-ops (never frees twice, never frees
    /// a paid reservation).
    pub fn release(&self, job_id: &str, now_unix: i64) -> Result<Released, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = match read_state(&tx, job_id)? {
            None => Released::NoReservation,
            Some((ReservationState::Released, _)) => Released::AlreadyReleased,
            Some((ReservationState::Spent, _)) => Released::WasSpent,
            Some((ReservationState::Reserved, amount)) => {
                tx.execute(
                    "UPDATE reservations SET state = 'released', updated_at_unix = ?2
                     WHERE job_id = ?1 AND state = 'reserved'",
                    params![job_id, now_unix],
                )?;
                Released::Freed { amount }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Convert `job_id`'s reservation `reserved → spent` on a successful collect. Exactly-once: only
    /// the first `Reserved → Spent` transition converts; a replayed collect sees `Spent` and does
    /// nothing (no double-label). A collect with no prior reservation inserts a `Spent` row so the
    /// job is recorded. This NEVER touches the budget ledger — that crate is the spend authority;
    /// this only moves the amount out of the `reserved` term.
    ///
    /// # Ordering obligation (the #126 wiring)
    ///
    /// This flip moves `amount` out of `reserved`. For BOTH ceilings to stay correct across the
    /// flip, the two effects that take `amount` up elsewhere MUST have already landed before this
    /// call:
    ///
    /// - the budget ledger's `spent`-append ([`crate::budget`]) — else the budget ceiling
    ///   `total_cap − spent − reserved` is transiently over-stated by `amount` (reserved dropped but
    ///   spent has not yet risen);
    /// - the wallet melt (the reduction the live `wallet_balance` reports) — else the wallet ceiling
    ///   `wallet_balance − reserved` is transiently over-stated by `amount` (reserved dropped but the
    ///   balance has not yet fallen).
    ///
    /// Sequenced correctly (append-spent + melt, THEN convert) the amount is never in two terms at
    /// once and never in neither: each ceiling sees a single, once-only reduction with no transient
    /// over-statement and no gap. The daemon that wires collect (#126) owns this ordering.
    pub fn convert_to_spent(
        &self,
        job_id: &str,
        amount: u64,
        now_unix: i64,
    ) -> Result<Converted, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = match read_state(&tx, job_id)? {
            Some((ReservationState::Spent, _)) => Converted::AlreadySpent,
            Some((ReservationState::Reserved, _)) => {
                tx.execute(
                    "UPDATE reservations SET state = 'spent', updated_at_unix = ?2
                     WHERE job_id = ?1",
                    params![job_id, now_unix],
                )?;
                Converted::FromReserved
            }
            Some((ReservationState::Released, _)) => {
                tx.execute(
                    "UPDATE reservations SET state = 'spent', updated_at_unix = ?2
                     WHERE job_id = ?1",
                    params![job_id, now_unix],
                )?;
                Converted::FromReleased
            }
            None => {
                tx.execute(
                    "INSERT INTO reservations
                         (job_id, amount_sats, state, created_at_unix, updated_at_unix)
                     VALUES (?1, ?2, 'spent', ?3, ?3)",
                    params![job_id, amount as i64, now_unix],
                )?;
                Converted::InsertedSpent
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Reconcile the ledger against caller-derived per-job dispositions (relay + disk truth). For
    /// each job: `Payable` keeps the reservation, `Dead` releases it, `Paid` ensures it is `Spent`.
    /// The whole batch runs in ONE transaction (a consistent snapshot) and is idempotent — a second
    /// run with the same dispositions changes nothing. Jobs with no reservation row are skipped.
    pub fn reconcile(
        &self,
        dispositions: &Dispositions,
        now_unix: i64,
    ) -> Result<ReconcileReport, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut report = ReconcileReport::default();
        for (job_id, disposition) in dispositions {
            let Some((state, _amount)) = read_state(&tx, job_id)? else {
                continue; // nothing reserved for this job — nothing to reconcile.
            };
            match (disposition, state) {
                (JobDisposition::Dead, ReservationState::Reserved) => {
                    tx.execute(
                        "UPDATE reservations SET state = 'released', updated_at_unix = ?2
                         WHERE job_id = ?1 AND state = 'reserved'",
                        params![job_id, now_unix],
                    )?;
                    report.released.push(job_id.clone());
                }
                (JobDisposition::Paid, ReservationState::Reserved)
                | (JobDisposition::Paid, ReservationState::Released) => {
                    tx.execute(
                        "UPDATE reservations SET state = 'spent', updated_at_unix = ?2
                         WHERE job_id = ?1",
                        params![job_id, now_unix],
                    )?;
                    report.converted.push(job_id.clone());
                }
                // Payable, or already-terminal states for Dead/Paid: leave as-is.
                _ => report.kept.push(job_id.clone()),
            }
        }
        tx.commit()?;
        Ok(report)
    }

    /// Sum of reservations still `Reserved` (the in-flight `reserved` term). Excludes `Spent` and
    /// `Released` rows.
    pub fn reserved_in_flight(&self) -> Result<u64, StoreError> {
        let conn = self.lock()?;
        let reserved: i64 = conn.query_row(
            "SELECT COALESCE(SUM(amount_sats), 0) FROM reservations WHERE state = 'reserved'",
            [],
            |row| row.get(0),
        )?;
        Ok(reserved.max(0) as u64)
    }

    /// `available = min(balance − reserved, total_cap − spent − reserved)`, saturating at 0.
    /// `balance` (live wallet ecash), `total_cap` (budget policy cap), and `spent` (budget ledger
    /// total) are caller snapshots. See the module docs for the two-ceiling model.
    pub fn available(&self, balance: u64, total_cap: u64, spent: u64) -> Result<u64, StoreError> {
        Ok(compute_available(
            balance,
            total_cap,
            self.reserved_in_flight()?,
            spent,
        ))
    }

    /// Job ids of every still-`Reserved` row — the set the daemon must resolve to dispositions at
    /// reconcile-on-restart.
    pub fn reserved_job_ids(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT job_id FROM reservations WHERE state = 'reserved' ORDER BY job_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        Ok(ids)
    }

    /// The `(state, amount)` of a job's reservation, if any. Inspection / tests.
    pub fn reservation(&self, job_id: &str) -> Result<Option<(ReservationState, u64)>, StoreError> {
        let conn = self.lock()?;
        read_reservation(&conn, job_id)
    }

    // ---- Pending auto-award intents (#126/#127) ------------------------------------------------

    /// Record (or reset to `pending`) a job's auto-award intent — its `max_sats` ceiling and optional
    /// harness/model preferences. Re-posting the same job resets it to `pending` and clears any prior
    /// parked reason so the daemon re-drives it.
    pub fn put_pending_award(
        &self,
        job_id: &str,
        max_sats: u64,
        harness: Option<&str>,
        model: Option<&str>,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO pending_awards
                 (job_id, max_sats, harness, model, state, reason, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, ?3, ?4, 'pending', NULL, ?5, ?5)
             ON CONFLICT(job_id) DO UPDATE SET
                 max_sats = excluded.max_sats,
                 harness = excluded.harness,
                 model = excluded.model,
                 state = 'pending',
                 reason = NULL,
                 updated_at_unix = excluded.updated_at_unix",
            params![job_id, max_sats as i64, harness, model, now_unix],
        )?;
        Ok(())
    }

    /// Every still-`pending` auto-award intent — the set the daemon re-arms on restart.
    pub fn list_pending_awards(&self) -> Result<Vec<PendingAward>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, max_sats, harness, model FROM pending_awards
             WHERE state = 'pending' ORDER BY created_at_unix",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingAward {
                job_id: row.get::<_, String>(0)?,
                max_sats: row.get::<_, i64>(1)?.max(0) as u64,
                harness: row.get::<_, Option<String>>(2)?,
                model: row.get::<_, Option<String>>(3)?,
            })
        })?;
        let mut intents = Vec::new();
        for row in rows {
            intents.push(row?);
        }
        Ok(intents)
    }

    /// Mark a job's auto-award intent `awarded` (a 3405 published, or already present on the relay).
    /// No-op if the job has no intent row.
    pub fn mark_award_awarded(&self, job_id: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE pending_awards SET state = 'awarded', reason = NULL, updated_at_unix = ?2
             WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        Ok(())
    }

    /// Mark a job's auto-award intent `parked` with a surfaced `reason` (e.g. the reservation was
    /// refused because funds shrank) — never a silent drop. No-op if the job has no intent row.
    pub fn mark_award_parked(&self, job_id: &str, reason: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE pending_awards SET state = 'parked', reason = ?2, updated_at_unix = ?3
             WHERE job_id = ?1",
            params![job_id, reason, now_unix],
        )?;
        Ok(())
    }

    /// The `(state, reason)` of a job's auto-award intent, if any. Inspection / tests.
    pub fn pending_award_state(&self, job_id: &str) -> Result<Option<(String, Option<String>)>, StoreError> {
        let conn = self.lock()?;
        Ok(conn
            .query_row(
                "SELECT state, reason FROM pending_awards WHERE job_id = ?1",
                [job_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?)
    }

    /// Every `parked` auto-award intent as `(job_id, reason)` — surfaced in `status` so a buyer sees
    /// jobs whose award could not be placed rather than silently losing them.
    pub fn parked_awards(&self) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, COALESCE(reason, '') FROM pending_awards
             WHERE state = 'parked' ORDER BY updated_at_unix",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut parked = Vec::new();
        for row in rows {
            parked.push(row?);
        }
        Ok(parked)
    }
}

/// A still-pending auto-award intent: the job the daemon owes an award, its spend ceiling, and the
/// (not-yet-a-filter) harness/model preferences captured at post time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAward {
    pub job_id: String,
    pub max_sats: u64,
    pub harness: Option<String>,
    pub model: Option<String>,
}

/// Read a job's `(state, amount)` from a live transaction. `None` when no row exists.
fn read_state(
    tx: &rusqlite::Transaction<'_>,
    job_id: &str,
) -> Result<Option<(ReservationState, u64)>, StoreError> {
    read_reservation(tx, job_id)
}

/// Read a job's `(state, amount)` from any [`Connection`]-like handle (a transaction derefs to
/// one). A row whose `state` is not a known label fails closed rather than being misread.
fn read_reservation(
    conn: &Connection,
    job_id: &str,
) -> Result<Option<(ReservationState, u64)>, StoreError> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT state, amount_sats FROM reservations WHERE job_id = ?1",
            [job_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((state, amount)) => {
            let state = ReservationState::parse(&state)
                .ok_or_else(|| StoreError(format!("unknown reservation state {state:?}")))?;
            Ok(Some((state, amount.max(0) as u64)))
        }
    }
}

/// Sum of `Reserved`-state amounts within a transaction (the in-flight `reserved` term).
fn sum_reserved(tx: &rusqlite::Transaction<'_>) -> Result<u64, StoreError> {
    let reserved: i64 = tx.query_row(
        "SELECT COALESCE(SUM(amount_sats), 0) FROM reservations WHERE state = 'reserved'",
        [],
        |row| row.get(0),
    )?;
    Ok(reserved.max(0) as u64)
}

fn read_meta_i64(conn: &Connection, key: &str) -> Result<Option<i64>, StoreError> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM buyer_meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok();
    match value {
        Some(text) => text
            .parse::<i64>()
            .map(Some)
            .map_err(|error| StoreError(format!("buyer_meta.{key} not an integer: {error}"))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("mobee-buyer-store-{label}-{}-{id}.sqlite", std::process::id()))
    }

    #[test]
    fn open_is_wal_and_carries_schema_and_start() {
        let path = temp_db("wal");
        let _ = std::fs::remove_file(&path);
        let store = BuyerStore::open(&path).expect("open");
        store.record_start(1234).expect("record start");

        let health = store.health().expect("health");
        assert_eq!(health.schema_version, SCHEMA_VERSION);
        assert_eq!(health.started_at_unix, 1234);
        assert_eq!(health.jobs, 0);

        // WAL mode leaves a -wal sidecar once written.
        let conn = Connection::open(&path).expect("reopen");
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");

        let _ = std::fs::remove_file(&path);
    }

    // ---- Reservation ledger (#123) ------------------------------------------------------------

    use super::super::reservations::{
        Ceiling, Converted, JobDisposition, Released, Reserved, ReserveRefused,
    };
    use std::collections::BTreeMap;

    /// A budget cap so large only the wallet ceiling can bind — lets a test isolate the wallet
    /// ceiling (`balance − reserved`) from the budget ceiling.
    const NO_BUDGET: u64 = u64::MAX;

    fn fresh_store(label: &str) -> (BuyerStore, std::path::PathBuf) {
        let path = temp_db(label);
        let _ = std::fs::remove_file(&path);
        let store = BuyerStore::open(&path).expect("open");
        (store, path)
    }

    // A v1 database (buyer_meta + jobs only, schema_version = 1) is upgraded forward on open: the
    // reservations AND pending_awards tables appear and schema_version moves to the current version.
    // No data migration, idempotent.
    #[test]
    fn open_migrates_v1_db_forward() {
        let path = temp_db("migrate");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute_batch(
                "CREATE TABLE buyer_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE jobs (job_id TEXT PRIMARY KEY, status TEXT NOT NULL, created_at_unix INTEGER NOT NULL);
                 INSERT INTO buyer_meta (key, value) VALUES ('schema_version', '1');",
            )
            .expect("seed v1");
        }
        let store = BuyerStore::open(&path).expect("open upgrades");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
        // The reservations table is now usable.
        assert_eq!(store.reserved_in_flight().expect("reserved"), 0);
        store
            .reserve(&"a".repeat(64), 10, 100, NO_BUDGET, 0, 1)
            .expect("reserve on upgraded db");
        // The pending_awards table is now usable too.
        store
            .put_pending_award(&"a".repeat(64), 10, None, None, 1)
            .expect("pending award on upgraded db");
        assert_eq!(store.list_pending_awards().expect("list").len(), 1);
        // Re-open is idempotent (still current version).
        let store2 = BuyerStore::open(&path).expect("reopen");
        assert_eq!(store2.health().expect("health").schema_version, SCHEMA_VERSION);
        let _ = std::fs::remove_file(&path);
    }

    // Pending auto-award intent lifecycle: put ⇒ listed as pending; mark_parked ⇒ off the pending
    // list, state=parked with the surfaced reason (invariant B: park, never silent drop);
    // mark_awarded ⇒ awarded; re-put ⇒ back to pending with the reason cleared.
    #[test]
    fn pending_award_lifecycle_parks_with_reason_and_rearms() {
        let (store, path) = fresh_store("pending-award");
        let job = "a".repeat(64);

        store.put_pending_award(&job, 21, Some("claude"), Some("opus"), 1).expect("put");
        let pending = store.list_pending_awards().expect("list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].job_id, job);
        assert_eq!(pending[0].max_sats, 21);
        assert_eq!(pending[0].harness.as_deref(), Some("claude"));

        // Park with a reason — off the pending list, visible as parked.
        store.mark_award_parked(&job, "reservation refused: funds shrank", 2).expect("park");
        assert!(store.list_pending_awards().expect("list").is_empty(), "parked is not pending");
        assert_eq!(
            store.pending_award_state(&job).expect("state"),
            Some(("parked".to_owned(), Some("reservation refused: funds shrank".to_owned())))
        );
        assert_eq!(
            store.parked_awards().expect("parked"),
            vec![(job.clone(), "reservation refused: funds shrank".to_owned())]
        );

        // Awarding clears the parked reason.
        store.mark_award_awarded(&job, 3).expect("awarded");
        assert_eq!(store.pending_award_state(&job).expect("state"), Some(("awarded".to_owned(), None)));
        assert!(store.parked_awards().expect("parked").is_empty());

        // Re-posting the same job re-arms it (back to pending, reason cleared).
        store.put_pending_award(&job, 30, None, None, 4).expect("re-put");
        assert_eq!(store.pending_award_state(&job).expect("state"), Some(("pending".to_owned(), None)));
        assert_eq!(store.list_pending_awards().expect("list")[0].max_sats, 30);
        let _ = std::fs::remove_file(&path);
    }

    // available = min(balance − reserved, total_cap − spent − reserved), saturating at 0.
    #[test]
    fn available_is_min_of_wallet_and_budget_ceilings() {
        let (store, path) = fresh_store("available");
        // No reservations, no budget constraint: available is the whole wallet balance.
        assert_eq!(store.available(100, NO_BUDGET, 0).expect("avail"), 100);
        store.reserve(&"a".repeat(64), 30, 100, NO_BUDGET, 0, 1).expect("reserve");
        assert_eq!(store.reserved_in_flight().expect("r"), 30);

        // Wallet ceiling binds: wallet balance 100 − reserved 30 = 70; the huge budget cap does not
        // bite, and spent is NOT subtracted from the wallet ceiling (the live balance already
        // netted completed spends). So spent=20 does not drag this below 70.
        assert_eq!(store.available(100, NO_BUDGET, 20).expect("avail"), 70);

        // Budget ceiling binds: total_cap 60 − spent 20 − reserved 30 = 10, which is below the
        // wallet ceiling 70. available = min(70, 10) = 10.
        assert_eq!(store.available(100, 60, 20).expect("avail"), 10);

        // Wallet ceiling saturates at 0: reserved alone exceeds a tiny live balance (never
        // underflows). balance 10 − reserved 30 → 0.
        assert_eq!(store.available(10, NO_BUDGET, 0).expect("avail"), 0);
        // Budget ceiling saturates at 0: total_cap 40 − spent 20 − reserved 30 → 0.
        assert_eq!(store.available(1000, 40, 20).expect("avail"), 0);
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH 1 — an award that would exceed available is REFUSED, with ZERO reserve written and
    // available unchanged. Red-on-revert: removing the `if amount > available` refuse in
    // `reserve` lets this reservation through, so the refuse assertion + the zero-write assertion
    // both fail.
    #[test]
    fn tooth1_over_available_award_refused_zero_reserve_written() {
        let (store, path) = fresh_store("tooth1");
        let job_a = "a".repeat(64);
        let job_b = "b".repeat(64);
        // balance 100, spent 0, no budget constraint. Reserve 80 → available becomes 20.
        store.reserve(&job_a, 80, 100, NO_BUDGET, 0, 1).expect("first reserve fits");
        assert_eq!(store.reserved_in_flight().expect("r"), 80);
        assert_eq!(store.available(100, NO_BUDGET, 0).expect("avail"), 20);

        // A 40-sat award would push reserved to 120 > balance 100 → refuse, bound by the wallet.
        let refused = store
            .reserve(&job_b, 40, 100, NO_BUDGET, 0, 2)
            .expect_err("over-available award must refuse");
        assert!(
            matches!(refused, ReserveRefused::InsufficientAvailable { requested: 40, available: 20, bound: Ceiling::Wallet }),
            "unexpected refusal: {refused:?}"
        );
        // ZERO written: no row for job_b, reserved + available unchanged.
        assert!(store.reservation(&job_b).expect("read").is_none(), "refused award must write NO row");
        assert_eq!(store.reserved_in_flight().expect("r"), 80, "reserved must be unchanged");
        assert_eq!(store.available(100, NO_BUDGET, 0).expect("avail"), 20, "available must be unchanged");
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH 2 — concurrent awards are serialized by BEGIN IMMEDIATE. Two independent connections
    // (two `BuyerStore` handles on the same file, so the in-process Mutex is NOT the serializer)
    // each reserve 60 against balance 100: each fits alone (avail 100) but not together (120 > 100).
    // BEGIN IMMEDIATE makes the loser block on the write lock, re-read the winner's committed 60,
    // and refuse. Exactly one succeeds; total reserved never exceeds available.
    //
    // Red-on-revert: changing `TransactionBehavior::Immediate` to `Deferred` lets both read a stale
    // reserved=0 and both commit (or the loser errors), so "exactly one clean refuse, total == 60"
    // fails.
    #[test]
    fn tooth2_concurrent_awards_serialized_exactly_one_wins() {
        use std::sync::{Arc, Barrier};

        let path = temp_db("tooth2");
        let _ = std::fs::remove_file(&path);
        // Materialize the schema once, then hand each thread its OWN connection to the same file.
        BuyerStore::open(&path).expect("create");

        let job_a = "a".repeat(64);
        let job_b = "b".repeat(64);
        let barrier = Arc::new(Barrier::new(2));

        let run = |job: String, path: std::path::PathBuf, barrier: Arc<Barrier>| {
            std::thread::spawn(move || {
                let store = BuyerStore::open(&path).expect("open");
                barrier.wait();
                store.reserve(&job, 60, 100, NO_BUDGET, 0, 1)
            })
        };
        let h_a = run(job_a.clone(), path.clone(), barrier.clone());
        let h_b = run(job_b.clone(), path.clone(), barrier.clone());
        let r_a = h_a.join().expect("join a");
        let r_b = h_b.join().expect("join b");

        let oks = [&r_a, &r_b].iter().filter(|r| r.is_ok()).count();
        let refused = [&r_a, &r_b]
            .iter()
            .filter(|r| matches!(r, Err(ReserveRefused::InsufficientAvailable { .. })))
            .count();
        assert_eq!(oks, 1, "exactly one award may win (a={r_a:?}, b={r_b:?})");
        assert_eq!(refused, 1, "the other must be a clean insufficient-available refuse (a={r_a:?}, b={r_b:?})");

        let store = BuyerStore::open(&path).expect("reopen");
        assert_eq!(
            store.reserved_in_flight().expect("r"),
            60,
            "total reserved must never exceed available — only the winner's 60 landed"
        );
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH 3 — reserve→spent is exactly-once. A replayed/idempotent collect does not double-spend
    // and leaves no dangling reserve. Red-on-revert: making `convert_to_spent` leave the row
    // `reserved` (skip the transition) leaves reserved at 60, failing the "reserved == 0" assert;
    // making a replay re-convert would return FromReserved instead of AlreadySpent.
    #[test]
    fn tooth3_reserve_to_spent_is_exactly_once() {
        let (store, path) = fresh_store("tooth3");
        let job = "a".repeat(64);
        store.reserve(&job, 60, 100, NO_BUDGET, 0, 1).expect("reserve");
        assert_eq!(store.reserved_in_flight().expect("r"), 60);

        // First collect converts reserve → spent.
        assert_eq!(
            store.convert_to_spent(&job, 60, 2).expect("convert"),
            Converted::FromReserved
        );
        assert_eq!(store.reserved_in_flight().expect("r"), 0, "spent leaves the reserved term");
        assert_eq!(
            store.reservation(&job).expect("read"),
            Some((ReservationState::Spent, 60))
        );

        // Replayed collect is a no-op — never a second spend, never a dangling reserve.
        assert_eq!(
            store.convert_to_spent(&job, 60, 3).expect("replay"),
            Converted::AlreadySpent
        );
        assert_eq!(store.reserved_in_flight().expect("r"), 0);

        // Re-reserving a spent job is refused (already paid) — no phantom re-commitment.
        let refused = store.reserve(&job, 60, 100, NO_BUDGET, 0, 4).expect_err("spent job re-reserve refused");
        assert!(matches!(refused, ReserveRefused::AlreadySpent { .. }), "got {refused:?}");
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH 4 (gudnuf's ANTI-LOCKUP tooth) — a reservation for a job that is no longer payable is
    // RELEASED, its funds become available again, and an award that was previously refused for
    // "insufficient funds" now SUCCEEDS. Proves funds are reclaimed, not stranded against a dead job.
    // Red-on-revert: if `release` did not free the reserved row (or counted `released` toward
    // `reserved`), the second award would still be refused, failing the "now succeeds" assert.
    #[test]
    fn tooth4_release_of_dead_job_reclaims_funds_for_a_new_award() {
        let (store, path) = fresh_store("tooth4");
        let dead = "a".repeat(64);
        let fresh = "b".repeat(64);
        // Whole balance reserved against `dead`.
        store.reserve(&dead, 100, 100, NO_BUDGET, 0, 1).expect("reserve dead");
        assert_eq!(store.available(100, NO_BUDGET, 0).expect("avail"), 0);

        // A new payable job is refused — the classic lock-up symptom.
        let refused = store
            .reserve(&fresh, 100, 100, NO_BUDGET, 0, 2)
            .expect_err("no funds while dead job holds them");
        assert!(matches!(refused, ReserveRefused::InsufficientAvailable { available: 0, .. }), "got {refused:?}");

        // The dead job is released (offer expired / declined / pay-window lapsed / …).
        assert_eq!(store.release(&dead, 3).expect("release"), Released::Freed { amount: 100 });
        assert_eq!(store.reserved_in_flight().expect("r"), 0);
        assert_eq!(store.available(100, NO_BUDGET, 0).expect("avail"), 100, "funds reclaimed, not stuck");

        // The previously-refused award now succeeds against the reclaimed funds.
        assert!(matches!(
            store.reserve(&fresh, 100, 100, NO_BUDGET, 0, 4).expect("now fits"),
            Reserved::New { .. }
        ));
        assert_eq!(store.reserved_in_flight().expect("r"), 100);

        // Release is idempotent: releasing the already-released dead job frees nothing more.
        assert_eq!(store.release(&dead, 5).expect("re-release"), Released::AlreadyReleased);
        assert_eq!(store.reserved_in_flight().expect("r"), 100, "no double-free");
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH 5 — reconcile-on-restart reclaims a stale reservation. A reserved-but-now-dead job
    // survives a simulated daemon restart (re-open the durable DB); reconcile with a `Dead`
    // disposition releases it and available is restored. Idempotent: a second reconcile changes
    // nothing. Red-on-revert: if reconcile did not release `Dead` jobs, reserved would stay 100 and
    // available would not be restored.
    #[test]
    fn tooth5_reconcile_on_restart_reclaims_stale_reservation() {
        let path = temp_db("tooth5");
        let _ = std::fs::remove_file(&path);
        let dead = "a".repeat(64);
        let live = "b".repeat(64);
        {
            let store = BuyerStore::open(&path).expect("open");
            store.reserve(&dead, 60, 100, NO_BUDGET, 0, 1).expect("reserve dead");
            store.reserve(&live, 20, 100, NO_BUDGET, 0, 1).expect("reserve live");
        } // daemon "crashes" — in-memory tracking is lost, the DB persists.

        // Restart: the reservations are still on disk.
        let store = BuyerStore::open(&path).expect("restart open");
        assert_eq!(store.reserved_in_flight().expect("r"), 80);
        assert_eq!(store.reserved_job_ids().expect("ids").len(), 2);

        // The daemon resolves relay/disk truth into dispositions: `dead` is no longer payable,
        // `live` still is.
        let mut dispositions: BTreeMap<String, JobDisposition> = BTreeMap::new();
        dispositions.insert(dead.clone(), JobDisposition::Dead);
        dispositions.insert(live.clone(), JobDisposition::Payable);

        let report = store.reconcile(&dispositions, 10).expect("reconcile");
        assert_eq!(report.released, vec![dead.clone()]);
        assert_eq!(report.kept, vec![live.clone()]);
        assert_eq!(store.reserved_in_flight().expect("r"), 20, "only the live reservation remains");
        assert_eq!(store.available(100, NO_BUDGET, 0).expect("avail"), 80, "dead funds restored");

        // Idempotent — a second reconcile with the same truth changes nothing.
        let again = store.reconcile(&dispositions, 11).expect("reconcile again");
        assert!(again.released.is_empty(), "no second release");
        assert_eq!(store.reserved_in_flight().expect("r"), 20);
        let _ = std::fs::remove_file(&path);
    }

    // reconcile also converts a `Paid` job to spent — an already-paid job must not dangle as
    // `reserved` after a restart that lost the in-memory convert.
    #[test]
    fn reconcile_paid_job_converts_dangling_reserve_to_spent() {
        let (store, path) = fresh_store("reconcile-paid");
        let paid = "a".repeat(64);
        store.reserve(&paid, 40, 100, NO_BUDGET, 0, 1).expect("reserve");
        let mut dispositions: BTreeMap<String, JobDisposition> = BTreeMap::new();
        dispositions.insert(paid.clone(), JobDisposition::Paid);
        let report = store.reconcile(&dispositions, 2).expect("reconcile");
        assert_eq!(report.converted, vec![paid.clone()]);
        assert_eq!(store.reserved_in_flight().expect("r"), 0);
        assert_eq!(store.reservation(&paid).expect("read"), Some((ReservationState::Spent, 40)));
        let _ = std::fs::remove_file(&path);
    }

    // Idempotent re-award of a still-reserved job with the SAME amount is a no-op; a DIFFERENT
    // amount is refused (a job's offer amount is fixed).
    #[test]
    fn re_award_same_amount_idempotent_different_amount_refused() {
        let (store, path) = fresh_store("re-award");
        let job = "a".repeat(64);
        store.reserve(&job, 50, 100, NO_BUDGET, 0, 1).expect("reserve");
        assert_eq!(
            store.reserve(&job, 50, 100, NO_BUDGET, 0, 2).expect("idempotent"),
            Reserved::Idempotent
        );
        assert_eq!(store.reserved_in_flight().expect("r"), 50, "no double-count");
        let refused = store.reserve(&job, 70, 100, NO_BUDGET, 0, 3).expect_err("amount mismatch refused");
        assert!(matches!(refused, ReserveRefused::AmountMismatch { existing: 50, requested: 70, .. }), "got {refused:?}");
        let _ = std::fs::remove_file(&path);
    }

    // A released reservation can be re-reserved (the job came back / was re-awarded), subject to the
    // available check against the OTHER reservations.
    #[test]
    fn released_row_can_be_re_reserved() {
        let (store, path) = fresh_store("re-reserve");
        let job = "a".repeat(64);
        store.reserve(&job, 30, 100, NO_BUDGET, 0, 1).expect("reserve");
        assert_eq!(store.release(&job, 2).expect("release"), Released::Freed { amount: 30 });
        assert_eq!(store.reserved_in_flight().expect("r"), 0);
        assert!(matches!(
            store.reserve(&job, 30, 100, NO_BUDGET, 0, 3).expect("re-reserve"),
            Reserved::New { .. }
        ));
        assert_eq!(store.reserved_in_flight().expect("r"), 30);
        let _ = std::fs::remove_file(&path);
    }

    // release never frees a SPENT reservation (that would fabricate a phantom credit), and
    // releasing a job with no reservation is a clean no-op.
    #[test]
    fn release_never_frees_spent_and_noop_when_absent() {
        let (store, path) = fresh_store("release-guards");
        let job = "a".repeat(64);
        let absent = "b".repeat(64);
        store.reserve(&job, 40, 100, NO_BUDGET, 0, 1).expect("reserve");
        store.convert_to_spent(&job, 40, 2).expect("convert");
        assert_eq!(store.release(&job, 3).expect("release spent"), Released::WasSpent);
        assert_eq!(
            store.reservation(&job).expect("read"),
            Some((ReservationState::Spent, 40)),
            "a spent row must stay spent"
        );
        assert_eq!(store.release(&absent, 4).expect("release absent"), Released::NoReservation);
        let _ = std::fs::remove_file(&path);
    }

    // A collect with no prior reservation (never awarded through the ledger) records a spent row so
    // the job is not invisible; a replay is idempotent.
    #[test]
    fn convert_without_prior_reservation_inserts_spent_row() {
        let (store, path) = fresh_store("convert-noprior");
        let job = "a".repeat(64);
        assert_eq!(
            store.convert_to_spent(&job, 25, 1).expect("insert spent"),
            Converted::InsertedSpent
        );
        assert_eq!(store.reservation(&job).expect("read"), Some((ReservationState::Spent, 25)));
        assert_eq!(store.reserved_in_flight().expect("r"), 0);
        assert_eq!(
            store.convert_to_spent(&job, 25, 2).expect("replay"),
            Converted::AlreadySpent
        );
        let _ = std::fs::remove_file(&path);
    }

    // REGRESSION (gudnuf's double-count bug) — the OLD formula `available = balance − reserved −
    // spent` subtracted cumulative `spent` from the LIVE wallet balance, double-counting every
    // completed payment (the melt already reduced the balance) and progressively refusing awards the
    // buyer can actually afford. Here the wallet holds 40 ecash after 60 sat of completed spends
    // under a 1000 budget cap; a 40-sat award is affordable. The corrected two-ceiling formula
    // ALLOWS it: wallet ceiling = 40 − 0 = 40, budget ceiling = 1000 − 60 − 0 = 940, available = 40.
    //
    // Red-on-revert: revert `compute_available` to `balance − reserved − spent` and it returns
    // 40 − 0 − 60 = 0 (saturated), so the award is wrongly refused and this test's `expect` fails.
    #[test]
    fn regression_completed_spend_not_double_counted_against_live_wallet() {
        let (store, path) = fresh_store("regression-double-count");
        let job = "a".repeat(64);
        // wallet_balance 40 (post-melt), total_cap 1000, spent 60, reserved 0. Award 40.
        let breakdown = store
            .reserve(&job, 40, 40, 1000, 60, 1)
            .expect("affordable award must be allowed under the two-ceiling formula");
        assert!(matches!(breakdown, Reserved::New { available_before: 40 }), "got {breakdown:?}");
        assert_eq!(store.reserved_in_flight().expect("r"), 40, "the 40-sat award landed");
        assert_eq!(store.available(40, 1000, 60).expect("avail"), 0, "wallet ceiling now 40 − 40 = 0");
        let _ = std::fs::remove_file(&path);
    }

    // The budget ceiling still BITES: with plenty of ecash on hand (wallet 1000) but the budget cap
    // nearly exhausted (total_cap 100, spent 90 → budget headroom 10), an award over the budget
    // headroom is refused even though the wallet could physically cover it — and the refusal names
    // the budget ceiling. An award within the headroom succeeds.
    //
    // Red-on-revert: drop the budget ceiling from `available_breakdown` (min only the wallet
    // ceiling) and available becomes 1000, so the 40 is wrongly allowed and the refuse assert fails.
    #[test]
    fn budget_ceiling_bites_even_when_wallet_could_cover() {
        let (store, path) = fresh_store("budget-ceiling");
        let over = "a".repeat(64);
        let fits = "b".repeat(64);
        // wallet 1000, total_cap 100, spent 90, reserved 0 → budget ceiling 10, wallet ceiling 1000.
        assert_eq!(store.available(1000, 100, 90).expect("avail"), 10, "budget headroom binds");

        let refused = store
            .reserve(&over, 40, 1000, 100, 90, 1)
            .expect_err("over-budget award must refuse even though the wallet could cover it");
        assert!(
            matches!(refused, ReserveRefused::InsufficientAvailable { requested: 40, available: 10, bound: Ceiling::Budget }),
            "must refuse bound by the budget ceiling: {refused:?}"
        );
        assert!(store.reservation(&over).expect("read").is_none(), "refused award writes NO row");
        assert!(refused.to_string().contains("budget"), "message names the budget ceiling: {refused}");

        // An award within the budget headroom succeeds.
        assert!(matches!(
            store.reserve(&fits, 10, 1000, 100, 90, 2).expect("within budget headroom"),
            Reserved::New { available_before: 10 }
        ));
        assert_eq!(store.reserved_in_flight().expect("r"), 10);
        let _ = std::fs::remove_file(&path);
    }
}
