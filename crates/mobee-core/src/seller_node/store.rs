//! The seller node's durable lifecycle state: `$MOBEE_HOME/seller.sqlite`.
//!
//! Opened only by the node (single-owner, guaranteed by the home lock). This SQLite database — in
//! WAL mode, `synchronous=FULL`, foreign keys on — is the **source of truth** for the seller's
//! trade lifecycle: the offers it has seen, the claims it has parked, the awards it has been
//! selected for, the jobs it is running, its deliveries and its collected receipts. Alongside them
//! sits the **nostr event outbox**: every event the node publishes is written to the DB and
//! enqueued in the SAME transaction as the state change that produced it, then handed to an async
//! publisher that retries until the relay confirms it or it expires. A crash between "state
//! changed" and "event sent" therefore never loses the obligation to publish, and never publishes
//! twice — the outbox `dedup_key` makes re-enqueue a no-op and the stored `created_at` makes the
//! signed event's id deterministic, so a re-publish is relay-idempotent.
//!
//! Every transition here is idempotent: replaying an award, a delivery, or a receipt lands the same
//! state and never double-credits. `rusqlite`'s [`Connection`] is `Send` but not `Sync`, so the
//! store keeps it behind a mutex and callers reach it from the async runtime via `spawn_blocking`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::gateway::EventDraft;

/// Current on-disk schema version.
pub const SCHEMA_VERSION: i64 = 4;

/// A cloneable handle to the node-owned SQLite state.
#[derive(Clone)]
pub struct SellerStore {
    conn: Arc<Mutex<Connection>>,
}

/// Store open / query failure.
#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "seller store error: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self(value.to_string())
    }
}

/// An offer the relay ingester has seen and the node may claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub offer_id: String,
    pub buyer_pubkey: String,
    pub amount_sats: u64,
    pub unit: String,
    pub task: String,
    pub deadline_unix: i64,
    pub targeted: bool,
    /// The harness the offer asked for (`["param", "agent", …]`), canonicalised; `None` ⇒ no
    /// preference. Journaled with the other offer facts because execution can be a RESTART away
    /// from the claim: a resumed job reads its requested harness from here, so it dispatches to
    /// the harness the buyer asked for and not to whichever one happens to be preferred now.
    pub requested_agent: Option<String>,
}

/// The lifecycle state of a job (execution side of a claim that was awarded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Awarded,
    Executing,
    Delivered,
    Paid,
    Failed,
}

impl JobState {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "awarded" => Self::Awarded,
            "executing" => Self::Executing,
            "delivered" => Self::Delivered,
            "paid" => Self::Paid,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// Outcome of parking a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claimed {
    /// A fresh claim row + a fresh outbox enqueue landed.
    New,
    /// The claim already existed — an idempotent replay, nothing re-enqueued.
    Idempotent,
}

/// Outcome of recording an award.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Awarded {
    /// First time this award id was seen: the claim moved to `awarded` and a job row was created.
    New,
    /// This award id was already recorded — a duplicate, ignored (no second job).
    Duplicate,
    /// The award names a claim this node never parked — recorded, but no job created.
    NoClaim,
}

/// Outcome of recording a collected receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collected {
    /// First time this receipt id was seen: the job moved to `paid`.
    New,
    /// This receipt id was already recorded — deduped, not credited a second time.
    Duplicate,
}

/// A pending outbox row the publisher must send. `draft` is the FULL event to sign — kind, content,
/// and every protocol/routing tag (`["v","0"]`, `["t","mobee"]`, the `e`/`p` tags) — so what the
/// publisher signs is wire-valid by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxItem {
    pub id: i64,
    pub dedup_key: String,
    pub draft: EventDraft,
    /// The fixed authored-at second: signing with this makes the event id deterministic, so a
    /// re-publish after a crash is idempotent at the relay.
    pub created_at_unix: i64,
    pub attempts: i64,
    pub expires_at_unix: i64,
}

/// A message addressed to this node that has not been answered.
///
/// The node records the debt and nothing more — no reply is drafted, and no agent is consulted.
/// Answering belongs to the slice that brings up the mind; until then this is a ledger a human (or
/// a later surface) can read to see exactly what the node was asked and has not addressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwedResponse {
    /// The asking event's id — the ledger's identity, and what makes re-delivery idempotent.
    pub event_id: String,
    pub relay_url: String,
    pub channel_id: String,
    /// Who asked.
    pub counterparty: String,
    /// The asking event's kind, kept so a later reader can tell a channel mention from a DM
    /// without re-fetching the event.
    pub kind: u16,
    /// The author's timestamp, not ours.
    pub created_at_unix: i64,
}

/// A point-in-time view of the store for `status` / reconcile reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub schema_version: i64,
    pub started_at_unix: i64,
    pub offers: i64,
    pub open_claims: i64,
    pub jobs: i64,
    pub pending_outbox: i64,
}

impl SellerStore {
    /// Open (creating if absent) the state DB at `path` with WAL + crash-safe pragmas and ensure
    /// the schema is present.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path.as_ref())?;
        // WAL for concurrent reads alongside the single writer; FULL sync + FK enforcement because
        // this DB holds money-adjacent lifecycle state. A bounded busy timeout avoids an immediate
        // SQLITE_BUSY under contention.
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
            "CREATE TABLE IF NOT EXISTS seller_meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             -- Offers the ingester has seen. One row per offer event id.
             CREATE TABLE IF NOT EXISTS offers (
                 offer_id        TEXT PRIMARY KEY,
                 buyer_pubkey    TEXT NOT NULL,
                 amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                 unit            TEXT NOT NULL,
                 task            TEXT NOT NULL,
                 deadline_unix   INTEGER NOT NULL,
                 targeted        INTEGER NOT NULL,
                 created_at_unix INTEGER NOT NULL,
                 -- The harness the offer requested. NULL ⇒ no preference, which is also what an
                 -- offer recorded before this column existed reads as.
                 requested_agent TEXT
             );
             -- Claims the node parked. `state` is the claim's own lifecycle; `awarded` marks the
             -- one the buyer selected, `released` the ones it stepped back from.
             CREATE TABLE IF NOT EXISTS claims (
                 job_id          TEXT PRIMARY KEY,
                 offer_id        TEXT NOT NULL,
                 state           TEXT NOT NULL CHECK (state IN ('claimed','awarded','released')),
                 -- The seller creq (NUT-18 payment request) authored from the offer terms at CLAIM
                 -- time (audit N-4). It is the single source of truth for the trade's payment terms:
                 -- the delivery cosignature signs ITS hash (never a rebuild from live config, so a
                 -- config change between claim and delivery cannot break the buyer/seller cosig), and
                 -- the restart redeem-guard settles against the mints IT lists (Fix Q — original terms,
                 -- not current config).
                 creq            TEXT NOT NULL,
                 created_at_unix INTEGER NOT NULL,
                 updated_at_unix INTEGER NOT NULL
             );
             -- Awards received. `award_id` (the award event id) is UNIQUE so a re-seen award is
             -- deduped and never creates a second job.
             CREATE TABLE IF NOT EXISTS awards (
                 award_id        TEXT PRIMARY KEY,
                 job_id          TEXT NOT NULL,
                 buyer_pubkey    TEXT NOT NULL,
                 created_at_unix INTEGER NOT NULL
             );
             -- Jobs the node is executing (one per awarded claim). `agent_name` is the harness that
             -- actually ran it — the journal row naming which agent did the job, and the evidence
             -- that a harness-requesting job was served by the harness it asked for.
             CREATE TABLE IF NOT EXISTS jobs (
                 job_id          TEXT PRIMARY KEY,
                 offer_id        TEXT NOT NULL,
                 agent_name      TEXT,
                 state           TEXT NOT NULL
                     CHECK (state IN ('awarded','executing','delivered','paid','failed')),
                 created_at_unix INTEGER NOT NULL,
                 updated_at_unix INTEGER NOT NULL
             );
             -- One delivery per job (the seller-authored snapshot the daemon published).
             CREATE TABLE IF NOT EXISTS deliveries (
                 job_id          TEXT PRIMARY KEY,
                 result_ref      TEXT NOT NULL,
                 delivered_at_unix INTEGER NOT NULL
             );
             -- Collected receipts. `receipt_id` is UNIQUE — the dedup that stops a replayed
             -- payment from crediting the same job twice.
             CREATE TABLE IF NOT EXISTS receipts (
                 receipt_id      TEXT PRIMARY KEY,
                 job_id          TEXT NOT NULL,
                 amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                 received_at_unix INTEGER NOT NULL
             );
             -- Intent-to-receive breadcrumbs, written BEFORE the mint swap (payment ordering,
             -- invariant 3). A breadcrumb records ONLY that a swap was attempted for a token — it is
             -- NEVER proof the swap landed (the mint reporting already-spent + a COMPLETED receipt is
             -- the only proof of our own prior collection). `token_hash` is SHA-256 of the token
             -- string; no proof/secret material is stored.
             CREATE TABLE IF NOT EXISTS pending_receive (
                 job_id          TEXT NOT NULL,
                 token_hash      TEXT NOT NULL,
                 buyer_pubkey    TEXT NOT NULL,
                 mint            TEXT NOT NULL,
                 amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                 created_at_unix INTEGER NOT NULL,
                 PRIMARY KEY (job_id, token_hash)
             );
             -- The nostr event outbox. `dedup_key` (UNIQUE) makes an enqueue idempotent; `draft_json`
             -- is the full serialized EventDraft (kind + content + all protocol/routing tags) so the
             -- publisher signs a wire-valid event. The publisher drains `pending` rows, signs with
             -- the fixed `created_at_unix` (so the event id is deterministic and re-publish is
             -- relay-idempotent), and marks each `confirmed` or `expired`.
             CREATE TABLE IF NOT EXISTS nostr_event_outbox (
                 id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                 dedup_key          TEXT NOT NULL UNIQUE,
                 draft_json         TEXT NOT NULL,
                 created_at_unix    INTEGER NOT NULL,
                 state              TEXT NOT NULL CHECK (state IN ('pending','confirmed','expired')),
                 attempts           INTEGER NOT NULL DEFAULT 0,
                 expires_at_unix    INTEGER NOT NULL,
                 published_event_id TEXT,
                 updated_at_unix    INTEGER NOT NULL
             );
             -- One cursor PER FILTER, not per relay and not per connection. A relay carries many
             -- independent subscriptions (the global membership filter, one filter per channel) and
             -- each advances at its own rate; a single shared cursor would drag the quiet ones
             -- forward past events they never saw. `filter_id` is the filter's stable identity
             -- ('membership', or 'channel:<uuid>').
             CREATE TABLE IF NOT EXISTS participation_cursors (
                 relay_url       TEXT NOT NULL,
                 filter_id       TEXT NOT NULL,
                 since_unix      INTEGER NOT NULL,
                 updated_at_unix INTEGER NOT NULL,
                 PRIMARY KEY (relay_url, filter_id)
             );
             -- The channels this node belongs to, so a restart re-subscribes to the right set.
             -- Cursors alone cannot carry this: they make the membership filter resume from where
             -- it stopped, which means the 44100 that admitted us is exactly what we will NOT see
             -- again. Membership has to be remembered, not re-derived.
             CREATE TABLE IF NOT EXISTS participation_channels (
                 relay_url       TEXT NOT NULL,
                 channel_id      TEXT NOT NULL,
                 -- 'joined' or 'left'. Rows are never deleted: a channel we were removed from is a
                 -- fact worth keeping, and a re-add flips it back rather than inventing a new row.
                 state           TEXT NOT NULL CHECK (state IN ('joined','left')),
                 -- The relay-signed 44100/44101 that last moved this row — the provenance of our
                 -- membership claim, so it can be checked against the relay rather than believed.
                 source_event_id TEXT NOT NULL,
                 -- The AUTHOR's timestamp on that event, which is what orders membership. Delivery
                 -- order cannot: a replayed old 44100 arriving after a newer 44101 would otherwise
                 -- flip us back to joined. Kept as a high-water mark even when the state does not
                 -- change, or a newer add followed by an older remove would still be applied.
                 source_created_at_unix INTEGER NOT NULL DEFAULT 0,
                 updated_at_unix INTEGER NOT NULL,
                 PRIMARY KEY (relay_url, channel_id)
             );
             -- Messages addressed to this node that nobody has answered yet.
             --
             -- The ledger is keyed on the EVENT ID, which is what makes ingest exactly-once. Every
             -- reconnect deliberately re-asks from slightly before its cursor (clock skew), so
             -- re-delivery is normal and expected; the primary key absorbs it. State is only ever
             -- advanced by a later slice — this one records the debt and answers nothing.
             CREATE TABLE IF NOT EXISTS participation_owed (
                 event_id         TEXT PRIMARY KEY,
                 relay_url        TEXT NOT NULL,
                 channel_id       TEXT NOT NULL,
                 counterparty     TEXT NOT NULL,
                 kind             INTEGER NOT NULL,
                 -- 'owed' until something answers it. 'dropped' when we lost access to the channel
                 -- it lives in: a debt we can no longer discharge is closed, not left pending.
                 state            TEXT NOT NULL CHECK (state IN ('owed','answered','dropped')),
                 -- The author's timestamp (ordering, and what the cursor advances on) kept apart
                 -- from when WE saw it (which is the only one of the two we can vouch for).
                 created_at_unix  INTEGER NOT NULL,
                 recorded_at_unix INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_participation_owed_state
                 ON participation_owed (state, created_at_unix);",
        )?;
        Self::migrate(conn)?;
        conn.execute(
            "INSERT INTO seller_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE CAST(seller_meta.value AS INTEGER) < CAST(excluded.value AS INTEGER)",
            [SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Bring a store created by an older binary up to [`SCHEMA_VERSION`]. `CREATE TABLE IF NOT
    /// EXISTS` never alters a table that already exists, so a column added to the schema above
    /// reaches existing stores only through here.
    ///
    /// Every step is ADDITIVE and idempotent — a nullable column whose absence reads the same as
    /// its default. Nothing here rewrites or drops a row: this store holds live trade state.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        if !Self::column_exists(conn, "offers", "requested_agent")? {
            conn.execute_batch("ALTER TABLE offers ADD COLUMN requested_agent TEXT;")?;
        }
        // A store written by a v3 binary has membership rows with no author timestamp. `0` is the
        // right backfill: it makes every stored row lose to the next real membership event, so the
        // relay's own notification re-establishes order rather than a guessed timestamp doing it.
        if !Self::column_exists(conn, "participation_channels", "source_created_at_unix")? {
            conn.execute_batch(
                "ALTER TABLE participation_channels
                     ADD COLUMN source_created_at_unix INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        Ok(())
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
        let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Record (idempotently overwrite) the node's most recent start time.
    pub fn record_start(&self, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO seller_meta (key, value) VALUES ('started_at_unix', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [now_unix.to_string()],
        )?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError("state DB mutex poisoned".into()))
    }

    // ---- Offer ingest ---------------------------------------------------------------------------

    /// Record a seen offer. Idempotent: a re-seen offer id is a no-op. Returns whether a new row
    /// landed.
    pub fn record_offer(&self, offer: &Offer, now_unix: i64) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO offers
                 (offer_id, buyer_pubkey, amount_sats, unit, task, deadline_unix, targeted, created_at_unix,
                  requested_agent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                offer.offer_id,
                offer.buyer_pubkey,
                offer.amount_sats as i64,
                offer.unit,
                offer.task,
                offer.deadline_unix,
                offer.targeted as i64,
                now_unix,
                offer.requested_agent,
            ],
        )?;
        Ok(changed == 1)
    }

    /// The `(buyer_pubkey, amount_sats, unit)` of a recorded offer, if any. The award arm reads the
    /// buyer to authorize an award (the award author MUST be the offer's buyer), and the pay path
    /// reads amount/unit as the redeem terms. `None` when the node never recorded this offer.
    pub fn offer_facts(&self, offer_id: &str) -> Result<Option<(String, u64, String)>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT buyer_pubkey, amount_sats, unit FROM offers WHERE offer_id = ?1",
                [offer_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? as u64,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The full recorded [`Offer`], if any. The execute arm needs the task (agent prompt + delivery
    /// message) and the absolute deadline (the unified job timeout) on top of the buyer/amount/unit
    /// that [`Self::offer_facts`] returns. `None` when the node never recorded this offer.
    pub fn offer_row(&self, offer_id: &str) -> Result<Option<Offer>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT offer_id, buyer_pubkey, amount_sats, unit, task, deadline_unix, targeted,
                        requested_agent
                 FROM offers WHERE offer_id = ?1",
                [offer_id],
                |row| {
                    Ok(Offer {
                        offer_id: row.get(0)?,
                        buyer_pubkey: row.get(1)?,
                        amount_sats: row.get::<_, i64>(2)? as u64,
                        unit: row.get(3)?,
                        task: row.get(4)?,
                        deadline_unix: row.get(5)?,
                        targeted: row.get::<_, i64>(6)? != 0,
                        requested_agent: row.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    // ---- Claim (state change + outbox enqueue in one transaction) -------------------------------

    /// Park a claim and enqueue its claim event in ONE transaction: either both the claim row and
    /// the outbox row land, or neither does. Idempotent — a replay for a `job_id` that already has
    /// a claim row changes nothing and re-enqueues nothing.
    ///
    /// `draft` is the full claim nostr event to publish (kind + content + protocol/routing tags);
    /// `created_at_unix` is its fixed authored-at second; `expires_at_unix` bounds how long the
    /// publisher retries before giving up. `creq` is the seller creq (NUT-18 payment request)
    /// authored from the offer terms at claim time (audit N-4) — journaled here so the delivery
    /// cosignature signs its stored hash and the restart redeem-guard settles against its stored
    /// mints, never a rebuild from live config.
    #[allow(clippy::too_many_arguments)]
    pub fn claim_and_enqueue(
        &self,
        job_id: &str,
        offer_id: &str,
        creq: &str,
        draft: &EventDraft,
        created_at_unix: i64,
        expires_at_unix: i64,
        now_unix: i64,
    ) -> Result<Claimed, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if claim_state(&tx, job_id)?.is_some() {
            tx.commit()?;
            return Ok(Claimed::Idempotent);
        }
        tx.execute(
            "INSERT INTO claims (job_id, offer_id, state, creq, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, 'claimed', ?3, ?4, ?4)",
            params![job_id, offer_id, creq, now_unix],
        )?;
        enqueue_event(
            &tx,
            &format!("claim:{job_id}"),
            draft,
            created_at_unix,
            expires_at_unix,
            now_unix,
        )?;
        tx.commit()?;
        Ok(Claimed::New)
    }

    /// Release a parked claim (offer expired, another seller won, capacity reached). Idempotent:
    /// only a still-`claimed` row is released; `awarded`/`released`/absent are no-ops.
    pub fn release_claim(&self, job_id: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE claims SET state = 'released', updated_at_unix = ?2
             WHERE job_id = ?1 AND state = 'claimed'",
            params![job_id, now_unix],
        )?;
        Ok(())
    }

    // ---- Award ----------------------------------------------------------------------------------

    /// Record an award for `job_id`. The `award_id` (award event id) is deduped: the first sighting
    /// moves the claim to `awarded` and creates the job row; a re-seen award id is a
    /// [`Awarded::Duplicate`] no-op (never a second job). An award naming a claim this node never
    /// parked is recorded but creates no job ([`Awarded::NoClaim`]).
    pub fn record_award(
        &self,
        award_id: &str,
        job_id: &str,
        buyer_pubkey: &str,
        now_unix: i64,
    ) -> Result<Awarded, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let inserted = tx.execute(
            "INSERT OR IGNORE INTO awards (award_id, job_id, buyer_pubkey, created_at_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![award_id, job_id, buyer_pubkey, now_unix],
        )?;
        if inserted == 0 {
            tx.commit()?;
            return Ok(Awarded::Duplicate);
        }

        let claim = claim_state(&tx, job_id)?;
        let offer_id = match &claim {
            Some((_, offer_id)) => offer_id.clone(),
            None => {
                // Award for a claim we do not hold — record the award, create no job.
                tx.commit()?;
                return Ok(Awarded::NoClaim);
            }
        };
        tx.execute(
            "UPDATE claims SET state = 'awarded', updated_at_unix = ?2 WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO jobs (job_id, offer_id, agent_name, state, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, NULL, 'awarded', ?3, ?3)",
            params![job_id, offer_id, now_unix],
        )?;
        tx.commit()?;
        Ok(Awarded::New)
    }

    // ---- Job execution --------------------------------------------------------------------------

    /// Record which harness ran a job. Idempotent (last write wins).
    pub fn assign_agent(&self, job_id: &str, agent_name: &str) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET agent_name = ?2 WHERE job_id = ?1",
            params![job_id, agent_name],
        )?;
        Ok(())
    }

    /// Move a job to `executing`. Idempotent: only an `awarded` job advances; a job already
    /// executing/delivered/paid is left as-is.
    pub fn mark_executing(&self, job_id: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET state = 'executing', updated_at_unix = ?2
             WHERE job_id = ?1 AND state = 'awarded'",
            params![job_id, now_unix],
        )?;
        Ok(())
    }

    /// Record a delivery and enqueue its result event in ONE transaction. Idempotent — a replay for
    /// a job that already has a delivery row changes nothing and re-enqueues nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn deliver_and_enqueue(
        &self,
        job_id: &str,
        result_ref: &str,
        draft: &EventDraft,
        created_at_unix: i64,
        expires_at_unix: i64,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM deliveries WHERE job_id = ?1",
                [job_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO deliveries (job_id, result_ref, delivered_at_unix) VALUES (?1, ?2, ?3)",
            params![job_id, result_ref, now_unix],
        )?;
        tx.execute(
            "UPDATE jobs SET state = 'delivered', updated_at_unix = ?2 WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        enqueue_event(
            &tx,
            &format!("result:{job_id}"),
            draft,
            created_at_unix,
            expires_at_unix,
            now_unix,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Mark a job failed. Idempotent (last write wins) but never overwrites a terminal `paid`.
    pub fn fail_job(&self, job_id: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET state = 'failed', updated_at_unix = ?2
             WHERE job_id = ?1 AND state != 'paid'",
            params![job_id, now_unix],
        )?;
        Ok(())
    }

    /// Record a collected receipt and mark the job paid. The `receipt_id` is deduped: the first
    /// sighting credits the job (`New`); a replay is a [`Collected::Duplicate`] no-op that never
    /// marks paid a second time. This is the money-safe boundary — a job is only ever `paid` once,
    /// keyed on the unique receipt id.
    pub fn collect_receipt(
        &self,
        receipt_id: &str,
        job_id: &str,
        amount_sats: u64,
        now_unix: i64,
    ) -> Result<Collected, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO receipts (receipt_id, job_id, amount_sats, received_at_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![receipt_id, job_id, amount_sats as i64, now_unix],
        )?;
        if inserted == 0 {
            tx.commit()?;
            return Ok(Collected::Duplicate);
        }
        tx.execute(
            "UPDATE jobs SET state = 'paid', updated_at_unix = ?2 WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        tx.commit()?;
        Ok(Collected::New)
    }

    /// Write the durable intent-to-receive breadcrumb BEFORE a mint swap (payment ordering, invariant
    /// 3). Idempotent on `(job_id, token_hash)` — a replay is a no-op. A breadcrumb NEVER proves the
    /// swap landed; it exists so a crash between swap and receipt is diagnosable and the re-see is
    /// classified by the COMPLETED-receipt read, not by the breadcrumb.
    pub fn append_pending_receive(
        &self,
        job_id: &str,
        token_hash: &str,
        buyer_pubkey: &str,
        mint: &str,
        amount_sats: u64,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO pending_receive
                 (job_id, token_hash, buyer_pubkey, mint, amount_sats, created_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![job_id, token_hash, buyer_pubkey, mint, amount_sats as i64, now_unix],
        )?;
        Ok(())
    }

    /// Whether a COMPLETED receipt exists for `job_id`. This is the ONLY positive proof of our own
    /// prior collection (finding S): on an already-spent re-see, `true` ⇒ idempotent no-op, `false` ⇒
    /// refuse (never forge a receipt from a breadcrumb), and a read error fails CLOSED at the caller.
    /// The most recent collected receipt's timestamp, or `None` when nothing has ever been
    /// collected. One half of the wrap-backfill cursor.
    pub fn last_receipt_unix(&self) -> Result<Option<i64>, StoreError> {
        let conn = self.lock()?;
        let latest = conn.query_row(
            "SELECT MAX(received_at_unix) FROM receipts",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok(latest)
    }

    /// Delivery timestamp of the OLDEST job that has been delivered but never paid, or `None` when
    /// every delivery has settled. The clamp that stops the wrap-backfill cursor from stepping over
    /// an older job's still-uncollected payment.
    pub fn oldest_unsettled_delivery_unix(&self) -> Result<Option<i64>, StoreError> {
        let conn = self.lock()?;
        let oldest = conn.query_row(
            "SELECT MIN(d.delivered_at_unix) FROM deliveries d
             WHERE NOT EXISTS (SELECT 1 FROM receipts r WHERE r.job_id = d.job_id)",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok(oldest)
    }

    pub fn has_receipt(&self, job_id: &str) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let found = conn
            .query_row(
                "SELECT 1 FROM receipts WHERE job_id = ?1 LIMIT 1",
                [job_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    // ---- Outbox ---------------------------------------------------------------------------------

    /// Every still-`pending` outbox row that has not yet expired (`expires_at_unix > now`),
    /// oldest first — the batch the publisher must send.
    pub fn pending_outbox(&self, now_unix: i64) -> Result<Vec<OutboxItem>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, dedup_key, draft_json, created_at_unix, attempts, expires_at_unix
             FROM nostr_event_outbox
             WHERE state = 'pending' AND expires_at_unix > ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map([now_unix], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (id, dedup_key, draft_json, created_at_unix, attempts, expires_at_unix) = row?;
            let draft: EventDraft = serde_json::from_str(&draft_json)
                .map_err(|error| StoreError(format!("outbox draft decode: {error}")))?;
            items.push(OutboxItem {
                id,
                dedup_key,
                draft,
                created_at_unix,
                attempts,
                expires_at_unix,
            });
        }
        Ok(items)
    }

    /// Mark an outbox row confirmed by the relay, recording the published event id.
    pub fn mark_confirmed(
        &self,
        id: i64,
        published_event_id: &str,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE nostr_event_outbox
             SET state = 'confirmed', published_event_id = ?2, attempts = attempts + 1,
                 updated_at_unix = ?3
             WHERE id = ?1",
            params![id, published_event_id, now_unix],
        )?;
        Ok(())
    }

    /// Bump the attempt counter after a failed publish (the row stays `pending` to retry).
    pub fn record_attempt(&self, id: i64, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE nostr_event_outbox SET attempts = attempts + 1, updated_at_unix = ?2
             WHERE id = ?1",
            params![id, now_unix],
        )?;
        Ok(())
    }

    /// Mark an outbox row expired (retry window elapsed) so the publisher stops sending it.
    pub fn expire_outbox(&self, now_unix: i64) -> Result<usize, StoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE nostr_event_outbox SET state = 'expired', updated_at_unix = ?1
             WHERE state = 'pending' AND expires_at_unix <= ?1",
            [now_unix],
        )?;
        Ok(changed)
    }

    /// The `(state, attempts, published_event_id)` of an outbox row by dedup key. Inspection/tests.
    pub fn outbox_row(
        &self,
        dedup_key: &str,
    ) -> Result<Option<(String, i64, Option<String>)>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT state, attempts, published_event_id FROM nostr_event_outbox
                 WHERE dedup_key = ?1",
                [dedup_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    // ---- Reconcile / inspection -----------------------------------------------------------------

    /// The jobs that must resume after a restart: everything not yet terminal (`awarded`,
    /// `executing`, `delivered`), oldest first. `paid`/`failed` are done and excluded.
    pub fn resumable_jobs(&self) -> Result<Vec<(String, JobState)>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT job_id, state FROM jobs
             WHERE state IN ('awarded','executing','delivered')
             ORDER BY created_at_unix, job_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut jobs = Vec::new();
        for row in rows {
            let (job_id, state) = row?;
            let state = JobState::parse(&state)
                .ok_or_else(|| StoreError(format!("unknown job state {state:?}")))?;
            jobs.push((job_id, state));
        }
        Ok(jobs)
    }

    /// The state of a single job, if any. Inspection/tests.
    pub fn job_state(&self, job_id: &str) -> Result<Option<JobState>, StoreError> {
        let conn = self.lock()?;
        let raw: Option<String> = conn
            .query_row("SELECT state FROM jobs WHERE job_id = ?1", [job_id], |row| {
                row.get(0)
            })
            .optional()?;
        match raw {
            None => Ok(None),
            Some(state) => JobState::parse(&state)
                .map(Some)
                .ok_or_else(|| StoreError(format!("unknown job state {state:?}"))),
        }
    }

    /// The unix second the award for `job_id` was recorded, if any. This is a durable, restart-STABLE
    /// value (written once at `record_award`), so the execute path uses it as the delivery commit's
    /// authored-at — a re-created delivery after a restart is then byte-identical (invariant 2). `None`
    /// when the job was never awarded.
    pub fn job_award_time(&self, job_id: &str) -> Result<Option<i64>, StoreError> {
        let conn = self.lock()?;
        let ts: Option<i64> = conn
            .query_row(
                "SELECT created_at_unix FROM awards WHERE job_id = ?1",
                [job_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(ts)
    }

    /// The creq journaled for a job at claim time (audit N-4). The delivery path signs its hash into
    /// the receipt preimage and the restart redeem-guard reads its mints, so a config change between
    /// claim and delivery can never alter the cosigned terms or the settlement mint set. `None` when
    /// the node never parked a claim for this job.
    pub fn job_creq(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let creq: Option<String> = conn
            .query_row("SELECT creq FROM claims WHERE job_id = ?1", [job_id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(creq)
    }

    /// The assigned agent for a job, if any. Inspection/tests.
    pub fn job_agent(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let agent: Option<Option<String>> = conn
            .query_row(
                "SELECT agent_name FROM jobs WHERE job_id = ?1",
                [job_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(agent.flatten())
    }

    /// Read the current health view for `status`.
    pub fn health(&self) -> Result<HealthSnapshot, StoreError> {
        let conn = self.lock()?;
        let schema_version = read_meta_i64(&conn, "schema_version")?.unwrap_or(0);
        let started_at_unix = read_meta_i64(&conn, "started_at_unix")?.unwrap_or(0);
        let offers = count(&conn, "SELECT COUNT(*) FROM offers")?;
        let open_claims = count(&conn, "SELECT COUNT(*) FROM claims WHERE state = 'claimed'")?;
        let jobs = count(&conn, "SELECT COUNT(*) FROM jobs")?;
        let pending_outbox = count(
            &conn,
            "SELECT COUNT(*) FROM nostr_event_outbox WHERE state = 'pending'",
        )?;
        Ok(HealthSnapshot {
            schema_version,
            started_at_unix,
            offers,
            open_claims,
            jobs,
            pending_outbox,
        })
    }

    // ── participation: cursors, membership, owed responses ───────────────────────────────────────
    //
    // Read/write state for the buzz participation surface. Deliberately NOT here: the per-relay
    // access state. Admission is a grant the relay holds, not a property of ours, so it is
    // re-proven on every boot rather than restored from disk — a cached "admitted" would let the
    // node address a relay that revoked it while we were down.

    /// The stored cursor for one filter, or `None` if it has never run on this relay.
    pub fn participation_cursor(
        &self,
        relay_url: &str,
        filter_id: &str,
    ) -> Result<Option<i64>, StoreError> {
        let conn = self.lock()?;
        let since = conn
            .query_row(
                "SELECT since_unix FROM participation_cursors
                 WHERE relay_url = ?1 AND filter_id = ?2",
                params![relay_url, filter_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(since)
    }

    /// Advance a filter's cursor.
    ///
    /// Monotonic by construction: the `WHERE` clause refuses to move a cursor backwards. Out-of-order
    /// delivery is ordinary — a reconnect replays from before the cursor, and relays do not promise
    /// ordering across a resubscribe — and a cursor that could step back would re-open a window it
    /// had already closed, every time.
    pub fn advance_participation_cursor(
        &self,
        relay_url: &str,
        filter_id: &str,
        since_unix: i64,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO participation_cursors (relay_url, filter_id, since_unix, updated_at_unix)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(relay_url, filter_id) DO UPDATE SET
                 since_unix      = excluded.since_unix,
                 updated_at_unix = excluded.updated_at_unix
             WHERE excluded.since_unix > participation_cursors.since_unix",
            params![relay_url, filter_id, since_unix, now_unix],
        )?;
        Ok(())
    }

    /// Record that the relay admitted us to a channel. Idempotent on `(relay, channel)`; a re-seen
    /// add for a channel we are already in changes nothing and returns `false`.
    ///
    /// This is why membership needs no event-id dedup set of its own: the effect of the notification
    /// is the row, and writing the same row twice is writing it once.
    /// Record a join AND seed the channel's mention cursor in ONE transaction.
    ///
    /// ★ The two writes cannot be separate calls. A crash between them leaves a joined channel with no
    /// cursor, and a channel with no cursor subscribes from `now` — reopening the lost-mentions window
    /// through a crash seam. Worse, it is unrecoverable by replay: the re-delivered `44100` finds the
    /// row already joined, reports no transition, and a caller that seeds only on a fresh join would
    /// skip the seed forever. Atomic here means a restart sees both or neither.
    ///
    /// `cursor_filter_id` is the channel's mention-filter id, and the seed is monotonic — an existing
    /// cursor that is already further ahead is left alone.
    pub fn record_channel_joined(
        &self,
        relay_url: &str,
        channel_id: &str,
        cursor_filter_id: &str,
        source_event_id: &str,
        source_created_at_unix: i64,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        self.apply_membership(
            relay_url,
            channel_id,
            "joined",
            Some((cursor_filter_id, source_created_at_unix)),
            source_event_id,
            source_created_at_unix,
            now_unix,
        )
    }

    /// Record that we were removed from a channel, and close out anything still owed inside it.
    ///
    /// Both halves are one transaction because a half-applied removal is the worst of the three
    /// outcomes: the node believes it left, while the ledger still shows debts in a channel it can
    /// no longer read — permanently unanswerable, permanently pending.
    ///
    /// Returns whether we had believed ourselves a member.
    pub fn record_channel_left(
        &self,
        relay_url: &str,
        channel_id: &str,
        source_event_id: &str,
        source_created_at_unix: i64,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        self.apply_membership(
            relay_url,
            channel_id,
            "left",
            None,
            source_event_id,
            source_created_at_unix,
            now_unix,
        )
    }

    /// Apply one relay-signed membership notification, ordered by the AUTHOR's timestamp.
    ///
    /// Returns whether the membership state actually transitioned — which is what decides if there
    /// is wire work to do. A stale notification returns `false` having changed nothing, and so does
    /// a newer one that merely re-states the state we already hold.
    ///
    /// The `(created_at, event_id)` high-water mark advances even when the state does not change.
    /// Without that, `add@3` (ignored as a no-op because we are already joined) followed by
    /// `remove@2` would compare `remove@2` against `add@1` and apply — resurrecting an ordering bug
    /// through the one path that looked safe to skip.
    fn apply_membership(
        &self,
        relay_url: &str,
        channel_id: &str,
        state: &str,
        seed_cursor: Option<(&str, i64)>,
        source_event_id: &str,
        source_created_at_unix: i64,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        let mut conn = self.lock()?;
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let current: Option<(String, i64, String)> = transaction
            .query_row(
                "SELECT state, source_created_at_unix, source_event_id
                 FROM participation_channels WHERE relay_url = ?1 AND channel_id = ?2",
                params![relay_url, channel_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        // ★ Seed the mention cursor FIRST, and unconditionally — before the staleness check below can
        // return early. The crash seam this closes is precisely a joined row with no cursor, whose only
        // repair is the replayed 44100; that replay is by definition NOT newer, so a seed placed after
        // the early return would be skipped exactly when it is the one thing needed. Monotonic, so a
        // cursor already further ahead is left alone and re-running this is free.
        if let Some((filter_id, since_unix)) = seed_cursor {
            transaction.execute(
                "INSERT INTO participation_cursors
                     (relay_url, filter_id, since_unix, updated_at_unix)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(relay_url, filter_id) DO UPDATE SET
                     since_unix      = excluded.since_unix,
                     updated_at_unix = excluded.updated_at_unix
                 WHERE excluded.since_unix > participation_cursors.since_unix",
                params![relay_url, filter_id, since_unix, now_unix],
            )?;
        }

        let transitioned = match current {
            None => {
                transaction.execute(
                    "INSERT INTO participation_channels
                         (relay_url, channel_id, state, source_event_id, source_created_at_unix,
                          updated_at_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        relay_url,
                        channel_id,
                        state,
                        source_event_id,
                        source_created_at_unix,
                        now_unix
                    ],
                )?;
                // A removal for a channel we hold no row for is recorded, but we were never a
                // member of it — so there is no subscription to tear down and nothing transitioned.
                state == "joined"
            }
            Some((stored_state, stored_created_at, stored_event_id)) => {
                // ★ Ordering is (created_at, restrictiveness, event_id) — NOT (created_at, event_id).
                //
                // A `CLOSED`-derived leave has no author timestamp and carries a SYNTHETIC
                // `closed:{reason}` marker as its source id. Comparing that against a real hex event id
                // is a string sort: `'c'` sits inside the hex alphabet, so a same-second CLOSED-leave
                // versus a genuine re-add would be decided by lexicographic accident.
                //
                // Ranking `left` above `joined` at an equal timestamp settles it on meaning instead:
                // the RESTRICTIVE transition wins, so ties FAIL CLOSED. Believing we hold access we do
                // not keeps sending traffic to a relay that refused us; believing we lack access we do
                // hold costs one channel, which the next 44100 replay restores. The synthetic id then
                // only ever participates when timestamp AND state both match — two leaves in the same
                // second, where either outcome is the same row.
                let rank = |state: &str| i32::from(state == "left");
                let newer = (
                    source_created_at_unix,
                    rank(state),
                    source_event_id,
                ) > (
                    stored_created_at,
                    rank(&stored_state),
                    stored_event_id.as_str(),
                );
                if !newer {
                    transaction.commit()?;
                    return Ok(false);
                }
                transaction.execute(
                    "UPDATE participation_channels
                     SET state = ?3, source_event_id = ?4, source_created_at_unix = ?5,
                         updated_at_unix = ?6
                     WHERE relay_url = ?1 AND channel_id = ?2",
                    params![
                        relay_url,
                        channel_id,
                        state,
                        source_event_id,
                        source_created_at_unix,
                        now_unix
                    ],
                )?;
                stored_state != state
            }
        };

        if state == "left" {
            // A debt in a channel we can no longer read is a debt we cannot discharge. Closed in the
            // same transaction as the membership change so no restart can observe one without the
            // other.
            transaction.execute(
                "UPDATE participation_owed SET state = 'dropped'
                 WHERE relay_url = ?1 AND channel_id = ?2 AND state = 'owed'",
                params![relay_url, channel_id],
            )?;
        }


        transaction.commit()?;
        Ok(transitioned)
    }

    /// The channels to re-subscribe on boot for one relay.
    pub fn joined_channels(&self, relay_url: &str) -> Result<Vec<String>, StoreError> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT channel_id FROM participation_channels
             WHERE relay_url = ?1 AND state = 'joined'
             ORDER BY channel_id",
        )?;
        let rows = statement.query_map([relay_url], |row| row.get::<_, String>(0))?;
        let mut channels = Vec::new();
        for row in rows {
            channels.push(row?);
        }
        Ok(channels)
    }

    /// The event that last made us a member of this channel — the provenance of our membership
    /// claim, so it can be checked against the relay rather than believed.
    pub fn joined_channel_source(
        &self,
        relay_url: &str,
        channel_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let source = conn
            .query_row(
                "SELECT source_event_id FROM participation_channels
                 WHERE relay_url = ?1 AND channel_id = ?2 AND state = 'joined'",
                params![relay_url, channel_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(source)
    }

    /// Record a message that is owed a response. Idempotent on the event id: the same message
    /// re-delivered after a reconnect is one debt, not two. Returns whether a new debt landed.
    pub fn record_owed(&self, owed: &OwedResponse, now_unix: i64) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        // ★ Conditional on still holding the channel. A mention can arrive after the 44101 that
        // removed us — queued behind it on the socket, or replayed within the cursor skew — and an
        // unconditional insert would mint a debt in a channel we can no longer read OR answer.
        // `record_channel_left` drains the debts that exist when it runs; this is the other half,
        // for the ones that turn up afterwards.
        let changed = conn.execute(
            "INSERT OR IGNORE INTO participation_owed
                 (event_id, relay_url, channel_id, counterparty, kind, state, created_at_unix,
                  recorded_at_unix)
             SELECT ?1, ?2, ?3, ?4, ?5, 'owed', ?6, ?7
             WHERE EXISTS (
                 SELECT 1 FROM participation_channels
                 WHERE relay_url = ?2 AND channel_id = ?3 AND state = 'joined'
             )",
            params![
                owed.event_id,
                owed.relay_url,
                owed.channel_id,
                owed.counterparty,
                owed.kind as i64,
                owed.created_at_unix,
                now_unix,
            ],
        )?;
        Ok(changed == 1)
    }

    /// Outstanding debts, oldest first.
    pub fn owed_responses(&self) -> Result<Vec<OwedResponse>, StoreError> {
        let conn = self.lock()?;
        let mut statement = conn.prepare(
            "SELECT event_id, relay_url, channel_id, counterparty, kind, created_at_unix
             FROM participation_owed WHERE state = 'owed'
             ORDER BY created_at_unix, event_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(OwedResponse {
                event_id: row.get(0)?,
                relay_url: row.get(1)?,
                channel_id: row.get(2)?,
                counterparty: row.get(3)?,
                kind: row.get::<_, i64>(4)? as u16,
                created_at_unix: row.get(5)?,
            })
        })?;
        let mut owed = Vec::new();
        for row in rows {
            owed.push(row?);
        }
        Ok(owed)
    }
}

/// Enqueue an event into the outbox within a live transaction. Idempotent on `dedup_key`: a second
/// enqueue with the same key is a no-op (`INSERT OR IGNORE`), which is what makes the transitions
/// that call this safe to replay.
fn enqueue_event(
    tx: &rusqlite::Transaction<'_>,
    dedup_key: &str,
    draft: &EventDraft,
    created_at_unix: i64,
    expires_at_unix: i64,
    now_unix: i64,
) -> Result<(), StoreError> {
    let draft_json = serde_json::to_string(draft)
        .map_err(|error| StoreError(format!("outbox draft encode: {error}")))?;
    tx.execute(
        "INSERT OR IGNORE INTO nostr_event_outbox
             (dedup_key, draft_json, created_at_unix, state, attempts, expires_at_unix, updated_at_unix)
         VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5)",
        params![dedup_key, draft_json, created_at_unix, expires_at_unix, now_unix],
    )?;
    Ok(())
}

/// Read a claim's `(state, offer_id)` from any connection-like handle (a transaction derefs to
/// one). `None` when no claim row exists.
fn claim_state(
    conn: &Connection,
    job_id: &str,
) -> Result<Option<(String, String)>, StoreError> {
    let row = conn
        .query_row(
            "SELECT state, offer_id FROM claims WHERE job_id = ?1",
            [job_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(row)
}

fn count(conn: &Connection, sql: &str) -> Result<i64, StoreError> {
    Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))?)
}

fn read_meta_i64(conn: &Connection, key: &str) -> Result<Option<i64>, StoreError> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM seller_meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    match value {
        Some(text) => text
            .parse::<i64>()
            .map(Some)
            .map_err(|error| StoreError(format!("seller_meta.{key} not an integer: {error}"))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::TagSpec;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-seller-store-{label}-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    fn fresh_store(label: &str) -> (SellerStore, std::path::PathBuf) {
        let path = temp_db(label);
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        (store, path)
    }

    fn sample_offer(id: &str) -> Offer {
        Offer {
            offer_id: id.to_owned(),
            buyer_pubkey: "b".repeat(64),
            amount_sats: 100,
            unit: "sat".to_owned(),
            task: "do the thing".to_owned(),
            deadline_unix: 10_000,
            targeted: true,
            requested_agent: None,
        }
    }

    /// A wire-valid draft carrying the protocol tags every mobee event needs.
    fn wire_draft(kind: u16) -> EventDraft {
        use crate::gateway::{MOBEE_TAG, PROTOCOL_VERSION};
        EventDraft::new(
            kind,
            vec![
                TagSpec::new(["t", MOBEE_TAG]),
                TagSpec::new(["v", PROTOCOL_VERSION]),
            ],
            "content",
        )
    }

    fn claim() -> EventDraft {
        wire_draft(crate::gateway::JOB_CLAIM_KIND)
    }

    fn result() -> EventDraft {
        wire_draft(crate::gateway::JOB_RESULT_KIND)
    }

    #[test]
    fn open_is_wal_and_carries_schema_and_start() {
        let (store, path) = fresh_store("wal");
        store.record_start(1234).expect("record start");
        let health = store.health().expect("health");
        assert_eq!(health.schema_version, SCHEMA_VERSION);
        assert_eq!(health.started_at_unix, 1234);
        assert_eq!(health.jobs, 0);

        let conn = Connection::open(&path).expect("reopen");
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH — the harness an offer requested is journaled with its other facts and READS BACK
    // across a reopen. Execution can be a restart away from the claim, so a request that lived only
    // in memory would let a resumed job run on whatever harness the node prefers now.
    #[test]
    fn requested_agent_survives_a_reopen() {
        let path = temp_db("requested-agent");
        let _ = std::fs::remove_file(&path);
        {
            let store = SellerStore::open(&path).expect("open");
            let mut offer = sample_offer("o1");
            offer.requested_agent = Some("codex".to_owned());
            store.record_offer(&offer, 1).expect("record");
            // An offer with no preference stays None — absence is a value here, not a default.
            store.record_offer(&sample_offer("o2"), 1).expect("record");
        }
        let store = SellerStore::open(&path).expect("reopen");
        assert_eq!(
            store.offer_row("o1").expect("row").expect("o1").requested_agent.as_deref(),
            Some("codex")
        );
        assert_eq!(
            store.offer_row("o2").expect("row").expect("o2").requested_agent,
            None
        );
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH — a store written by a binary from before this column opens, MIGRATES, and reads its
    // existing rows as "no preference". `CREATE TABLE IF NOT EXISTS` silently skips an existing
    // table, so without the ALTER an upgraded node would fail every offer read on a live store.
    #[test]
    fn a_store_from_before_the_column_migrates_and_reads_no_preference() {
        let path = temp_db("pre-agent-schema");
        let _ = std::fs::remove_file(&path);
        // The offers table exactly as the previous schema had it, holding a live row.
        {
            let conn = Connection::open(&path).expect("create old store");
            conn.execute_batch(
                "CREATE TABLE offers (
                     offer_id        TEXT PRIMARY KEY,
                     buyer_pubkey    TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     unit            TEXT NOT NULL,
                     task            TEXT NOT NULL,
                     deadline_unix   INTEGER NOT NULL,
                     targeted        INTEGER NOT NULL,
                     created_at_unix INTEGER NOT NULL
                 );
                 INSERT INTO offers VALUES ('old', 'buyer', 21, 'sat', 'task', 10000, 1, 1);",
            )
            .expect("old schema");
        }

        let store = SellerStore::open(&path).expect("open migrates");
        let row = store.offer_row("old").expect("read").expect("the pre-existing row survives");
        assert_eq!(row.amount_sats, 21, "the row is migrated, not replaced");
        assert_eq!(row.requested_agent, None, "an offer from before the column asked for no harness");
        // Migration is idempotent: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
        assert!(store.offer_row("old").expect("read").is_some());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_offer_is_idempotent() {
        let (store, path) = fresh_store("offer");
        let offer = sample_offer(&"a".repeat(64));
        assert!(store.record_offer(&offer, 1).expect("first"));
        assert!(!store.record_offer(&offer, 2).expect("second"), "re-seen offer is a no-op");
        assert_eq!(store.health().expect("h").offers, 1);
        // offer_facts serves the award-auth (buyer) and pay (amount/unit) reads.
        assert_eq!(
            store.offer_facts(&offer.offer_id).expect("facts"),
            Some((offer.buyer_pubkey.clone(), offer.amount_sats, offer.unit.clone()))
        );
        assert_eq!(store.offer_facts(&"z".repeat(64)).expect("absent"), None);
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH 2 (charter) — RED ON REVERT for the outbox. `claim_and_enqueue` must write the claim
    // row AND the outbox row atomically. This asserts the outbox MUTATION LANDED (a pending row
    // carrying the full wire-valid draft — right kind AND the `["v","0"]` + `["t","mobee"]` protocol
    // tags a live buyer requires), not merely that no error was returned. Deleting the
    // `enqueue_event` call in `claim_and_enqueue` leaves the claim row but no outbox row, so the
    // length / kind / tag assertions fail — the revert turns this test red.
    #[test]
    fn tooth_outbox_write_lands_atomically_with_the_claim() {
        use crate::gateway::{JOB_CLAIM_KIND, MOBEE_TAG, PROTOCOL_VERSION};
        let (store, path) = fresh_store("outbox-redonrevert");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        assert_eq!(
            store
                .claim_and_enqueue(&job, &offer, "creqA", &claim(), 500, 999, 1)
                .expect("claim"),
            Claimed::New
        );

        // The outbox row LANDED — pending, the claim kind, and the protocol tags, not yet published.
        let pending = store.pending_outbox(2).expect("pending");
        assert_eq!(pending.len(), 1, "exactly one pending outbox row must exist");
        let item = &pending[0];
        assert_eq!(item.dedup_key, format!("claim:{job}"));
        assert_eq!(item.draft.kind, JOB_CLAIM_KIND);
        assert_eq!(item.created_at_unix, 500);
        assert_eq!(item.attempts, 0);
        // The enqueued draft is wire-valid: it carries the version + namespace tags parse_offer/
        // the buyer require, so a signed event from it is not rejected on the wire.
        assert!(has_tag(&item.draft, "v", PROTOCOL_VERSION), "draft must carry [\"v\",\"0\"]");
        assert!(has_tag(&item.draft, "t", MOBEE_TAG), "draft must carry [\"t\",\"mobee\"]");

        let row = store.outbox_row(&format!("claim:{job}")).expect("row").expect("exists");
        assert_eq!(row.0, "pending");
        assert!(row.2.is_none(), "not yet published");
        let _ = std::fs::remove_file(&path);
    }

    fn has_tag(draft: &EventDraft, name: &str, value: &str) -> bool {
        draft
            .tags
            .iter()
            .any(|tag| tag.first() == Some(name) && tag.value() == Some(value))
    }

    #[test]
    fn claim_and_enqueue_is_idempotent_no_double_enqueue() {
        let (store, path) = fresh_store("claim-idem");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        assert_eq!(
            store.claim_and_enqueue(&job, &offer, "creqA", &claim(), 1, 999, 1).expect("first"),
            Claimed::New
        );
        // A replay carrying a DIFFERENT creq is a no-op: neither the outbox nor the journaled
        // claim-time creq is overwritten. The first creq — the one that was on the wire — stands.
        assert_eq!(
            store.claim_and_enqueue(&job, &offer, "creqB", &claim(), 1, 999, 2).expect("replay"),
            Claimed::Idempotent
        );
        assert_eq!(store.pending_outbox(3).expect("pending").len(), 1, "no second enqueue");
        assert_eq!(
            store.job_creq(&job).expect("creq").as_deref(),
            Some("creqA"),
            "the claim-time creq is immutable across replays"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn award_dedup_creates_one_job_and_ignores_replays() {
        let (store, path) = fresh_store("award");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let award = "w".repeat(64);
        let buyer = "b".repeat(64);
        store.claim_and_enqueue(&job, &offer, "creqA", &claim(), 1, 999, 1).expect("claim");

        assert_eq!(
            store.record_award(&award, &job, &buyer, 2).expect("award"),
            Awarded::New
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Awarded));
        // The award time is the durable, restart-stable delivery author-date (invariant 2 source).
        assert_eq!(store.job_award_time(&job).expect("award time"), Some(2));
        assert_eq!(store.job_award_time(&"z".repeat(64)).expect("absent"), None);

        // A re-seen award id is a dedup no-op — no second job, state unchanged.
        assert_eq!(
            store.record_award(&award, &job, &buyer, 3).expect("replay"),
            Awarded::Duplicate
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Awarded));

        // An award for an unknown claim is recorded but creates no job.
        let orphan_job = "k".repeat(64);
        let orphan_award = "x".repeat(64);
        assert_eq!(
            store.record_award(&orphan_award, &orphan_job, &buyer, 4).expect("orphan"),
            Awarded::NoClaim
        );
        assert_eq!(store.job_state(&orphan_job).expect("state"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deliver_is_idempotent_and_enqueues_result_once() {
        let (store, path) = fresh_store("deliver");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let buyer = "b".repeat(64);
        store.claim_and_enqueue(&job, &offer, "creqA", &claim(), 1, 999, 1).expect("claim");
        store.record_award(&"w".repeat(64), &job, &buyer, 2).expect("award");
        store.mark_executing(&job, 3).expect("exec");

        assert!(store
            .deliver_and_enqueue(&job, "ref-1", &result(), 4, 999, 5)
            .expect("deliver"));
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Delivered));
        // Replay: no second delivery, no second result enqueue.
        assert!(!store
            .deliver_and_enqueue(&job, "ref-1", &result(), 4, 999, 6)
            .expect("replay"));
        assert_eq!(
            store.outbox_row(&format!("result:{job}")).expect("row").expect("exists").0,
            "pending"
        );
        let _ = std::fs::remove_file(&path);
    }

    // Money-safe dedup: a replayed receipt never marks a job paid twice.
    #[test]
    fn collect_receipt_dedups_and_pays_once() {
        let (store, path) = fresh_store("collect");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let receipt = "r".repeat(64);
        store.claim_and_enqueue(&job, &offer, "creqA", &claim(), 1, 999, 1).expect("claim");
        store.record_award(&"w".repeat(64), &job, &"b".repeat(64), 2).expect("award");

        assert_eq!(
            store.collect_receipt(&receipt, &job, 100, 3).expect("collect"),
            Collected::New
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Paid));
        assert_eq!(
            store.collect_receipt(&receipt, &job, 100, 4).expect("replay"),
            Collected::Duplicate,
            "a replayed receipt must not credit twice"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expire_outbox_stops_the_publisher_from_sending() {
        let (store, path) = fresh_store("expire");
        let job = "j".repeat(64);
        store.claim_and_enqueue(&job, &"o".repeat(64), "creqA", &claim(), 1, 100, 1).expect("claim");
        // now=200 is past expires_at=100.
        assert_eq!(store.expire_outbox(200).expect("expire"), 1);
        assert!(store.pending_outbox(200).expect("pending").is_empty());
        assert_eq!(
            store.outbox_row(&format!("claim:{job}")).expect("row").expect("exists").0,
            "expired"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ── participation ────────────────────────────────────────────────────────────────────────────

    const RELAY: &str = "wss://relay.example";

    fn owed(event_id: &str, channel: &str, created_at_unix: i64) -> OwedResponse {
        OwedResponse {
            event_id: event_id.to_owned(),
            relay_url: RELAY.to_owned(),
            channel_id: channel.to_owned(),
            counterparty: "c".repeat(64),
            kind: 9,
            created_at_unix,
        }
    }

    #[test]
    fn each_filter_carries_its_own_cursor() {
        let (store, path) = fresh_store("cursors");
        store.advance_participation_cursor(RELAY, "membership", 500, 1).expect("membership");
        store.advance_participation_cursor(RELAY, "channel:abc", 100, 1).expect("channel");

        // The busy filter must not drag the quiet one forward past events it never saw.
        assert_eq!(store.participation_cursor(RELAY, "membership").expect("read"), Some(500));
        assert_eq!(store.participation_cursor(RELAY, "channel:abc").expect("read"), Some(100));
        // Same filter id on a different relay is a different cursor.
        assert_eq!(store.participation_cursor("wss://other", "membership").expect("read"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_cursor_never_moves_backwards() {
        let (store, path) = fresh_store("cursor-monotonic");
        store.advance_participation_cursor(RELAY, "membership", 500, 1).expect("forward");
        // A reconnect replays from before the cursor; an out-of-order event must not re-open the
        // window that had already closed.
        store.advance_participation_cursor(RELAY, "membership", 300, 2).expect("backward");
        assert_eq!(store.participation_cursor(RELAY, "membership").expect("read"), Some(500));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn joining_the_same_channel_twice_is_joining_it_once() {
        let (store, path) = fresh_store("join-idempotent");
        assert!(store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "e1", 100, 100).expect("first"));
        // The relay re-delivers the 44100 after a reconnect — SAME event, so same author timestamp.
        // The effect is the row, so writing it again writes it once.
        assert!(!store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "e1", 100, 101).expect("replay"));
        assert_eq!(store.joined_channels(RELAY).expect("channels"), ["chan-1"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn restart_resubscribes_exactly_the_channels_we_are_still_in() {
        let (store, path) = fresh_store("join-leave");
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "e1", 100, 100).expect("join 1");
        store.record_channel_joined(RELAY, "chan-2", "channel:chan-2", "e2", 100, 100).expect("join 2");
        store.record_channel_left(RELAY, "chan-1", "e3", 200, 200).expect("leave 1");

        assert_eq!(store.joined_channels(RELAY).expect("channels"), ["chan-2"]);

        // A re-add flips the same row back rather than inventing a second one.
        assert!(store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "e4", 300, 300).expect("re-add"));
        assert_eq!(store.joined_channels(RELAY).expect("channels"), ["chan-1", "chan-2"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn losing_a_channel_closes_the_debts_inside_it_and_nothing_else() {
        let (store, path) = fresh_store("owed-dropped");
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "e1", 100, 100).expect("join");
        // Both channels must be JOINED for their debts to exist at all — a debt is only recorded for
        // a channel we hold. Without joining chan-2 this test would pass on an empty control.
        store.record_channel_joined(RELAY, "chan-2", "channel:chan-2", "e2", 100, 100).expect("join 2");
        store.record_owed(&owed(&"a".repeat(64), "chan-1", 110), 111).expect("owed 1");
        store.record_owed(&owed(&"b".repeat(64), "chan-2", 120), 121).expect("owed 2");

        store.record_channel_left(RELAY, "chan-1", "e3", 200, 200).expect("leave");

        // A debt we can no longer discharge is closed, not left pending forever; a debt in a
        // channel we still hold is untouched.
        let outstanding = store.owed_responses().expect("owed");
        assert_eq!(outstanding.len(), 1);
        assert_eq!(outstanding[0].channel_id, "chan-2");
        let _ = std::fs::remove_file(&path);
    }

    /// Membership must be ordered by the AUTHOR's clock, not by which frame the relay flushed first.
    ///
    /// Replay makes delivery order arbitrary: every reconnect deliberately re-asks from before its
    /// cursor, so an old 44100 can legitimately arrive after a newer 44101. Applying that would
    /// re-subscribe us to a channel the relay has already removed us from — and the store would look
    /// perfectly consistent while doing it.
    #[test]
    fn a_replayed_old_add_cannot_undo_a_newer_removal() {
        let (store, path) = fresh_store("membership-order");
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-1", 100, 100).expect("join");
        store.record_channel_left(RELAY, "chan-1", "remove-1", 200, 200).expect("leave");

        // The stale 44100 the relay re-sends after a reconnect. Older by author time, later by
        // arrival — and it must lose.
        assert!(
            !store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-1", 100, 300).expect("stale add"),
            "a stale add reported a transition, so it was applied"
        );
        assert!(
            store.joined_channels(RELAY).expect("channels").is_empty(),
            "a replayed old 44100 resurrected a membership the relay had already revoked"
        );

        // The inverse: a stale removal must not drop a membership we legitimately re-acquired.
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-2", 400, 400).expect("re-add");
        assert!(
            !store.record_channel_left(RELAY, "chan-1", "remove-1", 200, 500).expect("stale remove"),
            "a stale removal reported a transition"
        );
        assert_eq!(store.joined_channels(RELAY).expect("channels"), ["chan-1"]);
        let _ = std::fs::remove_file(&path);
    }

    /// The high-water mark has to advance even when the state does NOT change, or ordering is only
    /// half-enforced: `add@3` is a no-op against an existing join, so if it left the stored timestamp
    /// at `add@1` then `remove@2` would compare against the wrong event and apply.
    #[test]
    fn a_newer_add_that_changes_nothing_still_advances_the_ordering_mark() {
        let (store, path) = fresh_store("membership-highwater");
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-1", 100, 100).expect("join");
        // Newer add, same state — reports no transition, but must still move the mark to 300.
        assert!(!store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-3", 300, 300).expect("newer add"));

        assert!(
            !store.record_channel_left(RELAY, "chan-1", "remove-2", 200, 400).expect("stale remove"),
            "a removal older than the newest add was applied"
        );
        assert_eq!(
            store.joined_channels(RELAY).expect("channels"),
            ["chan-1"],
            "the mark stayed at the first add, so a stale removal beat it"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The join row and the mention cursor must land together, or a crash between them leaves a joined
    /// channel whose filter opens at `now` — the lost-mentions window, reopened through a crash seam.
    #[test]
    fn a_join_seeds_its_cursor_in_the_same_write() {
        let (store, path) = fresh_store("join-seeds-cursor");
        assert!(
            store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-1", 1_000, 1_001)
                .expect("join")
        );
        assert_eq!(
            store.participation_cursor(RELAY, "channel:chan-1").expect("cursor"),
            Some(1_000),
            "the join committed without its cursor, so a restart would subscribe from now()"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The repair path for that crash seam. A joined row with no cursor can only be fixed by the
    /// replayed 44100 — which is NOT newer, reports no transition, and would be skipped by any seed
    /// placed behind the staleness check. So the seed has to be unconditional.
    #[test]
    fn a_replayed_invite_still_repairs_a_missing_cursor() {
        let (store, path) = fresh_store("join-repairs-cursor");
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-1", 1_000, 1_001)
            .expect("join");
        // Simulate the crash seam: the membership row survived, the cursor did not.
        {
            let conn = store.lock().expect("lock");
            conn.execute("DELETE FROM participation_cursors", []).expect("drop cursor");
        }
        assert_eq!(store.participation_cursor(RELAY, "channel:chan-1").expect("gone"), None);

        // The relay re-delivers the same 44100 after a reconnect: same id, same timestamp, no transition.
        assert!(
            !store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-1", 1_000, 9_999)
                .expect("replay")
        );
        assert_eq!(
            store.participation_cursor(RELAY, "channel:chan-1").expect("cursor"),
            Some(1_000),
            "the replayed invite did not restore the cursor, so the window stays open forever"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A `CLOSED`-derived leave carries a synthetic `closed:{reason}` marker instead of an event id, and
    /// `'c'` is inside the hex alphabet — so a same-second tie against a real re-add must not be settled
    /// by string sort. Ties resolve on meaning: the restrictive transition wins, fail-closed.
    #[test]
    fn a_same_second_closed_leave_beats_a_re_add_regardless_of_id_sorting() {
        let (store, path) = fresh_store("closed-tie");
        // A real hex id starting BELOW 'c', so a plain string sort would let the re-add win.
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", &"a".repeat(64), 500, 500)
            .expect("join");
        store.record_channel_left(RELAY, "chan-1", "closed:restricted: no access", 500, 500)
            .expect("closed leave");
        assert!(
            store.joined_channels(RELAY).expect("channels").is_empty(),
            "a same-second CLOSED leave lost to a re-add on id sorting instead of failing closed"
        );

        // And the tie must not swing the other way either: an id sorting ABOVE the marker must still
        // lose to it at the same second.
        let (store2, path2) = fresh_store("closed-tie-2");
        store2.record_channel_joined(RELAY, "chan-1", "channel:chan-1", &"f".repeat(64), 500, 500)
            .expect("join");
        store2.record_channel_left(RELAY, "chan-1", "closed:restricted: no access", 500, 500)
            .expect("closed leave");
        assert!(store2.joined_channels(RELAY).expect("channels").is_empty());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    /// A mention can arrive after the 44101 that removed us — queued behind it, or replayed inside the
    /// cursor skew. Recording it would mint a debt in a channel we can neither read nor answer, and
    /// nothing downstream would ever clear it.
    #[test]
    fn a_mention_arriving_after_we_left_creates_no_debt() {
        let (store, path) = fresh_store("owed-after-left");
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "add-1", 100, 100).expect("join");
        store.record_channel_left(RELAY, "chan-1", "remove-1", 200, 200).expect("leave");

        assert!(
            !store.record_owed(&owed(&"f".repeat(64), "chan-1", 150), 300).expect("late mention"),
            "a debt was recorded for a channel we had already left"
        );
        assert!(store.owed_responses().expect("owed").is_empty());

        // And the positive control: the same call in a channel we DO hold must still record, or this
        // test would pass just as well against a record_owed that never writes anything.
        store.record_channel_joined(RELAY, "chan-2", "channel:chan-2", "add-2", 100, 100).expect("join 2");
        assert!(store.record_owed(&owed(&"e".repeat(64), "chan-2", 150), 300).expect("live mention"));
        assert_eq!(store.owed_responses().expect("owed").len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_redelivered_message_is_one_debt_not_two() {
        let (store, path) = fresh_store("owed-dedup");
        let event = "d".repeat(64);
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "e1", 100, 100).expect("join");
        assert!(store.record_owed(&owed(&event, "chan-1", 110), 111).expect("first"));
        // Every reconnect deliberately re-asks from before its cursor, so this is the normal case.
        assert!(!store.record_owed(&owed(&event, "chan-1", 110), 999).expect("replay"));
        assert_eq!(store.owed_responses().expect("owed").len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn debts_come_back_oldest_first_with_the_authors_timestamp() {
        let (store, path) = fresh_store("owed-order");
        store.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "e1", 100, 100).expect("join");
        store.record_owed(&owed(&"2".repeat(64), "chan-1", 200), 1).expect("later");
        store.record_owed(&owed(&"1".repeat(64), "chan-1", 100), 2).expect("earlier");

        let outstanding = store.owed_responses().expect("owed");
        // Ordered by when they were ASKED, not when we happened to ingest them.
        assert_eq!(outstanding[0].created_at_unix, 100);
        assert_eq!(outstanding[1].created_at_unix, 200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_store_written_by_the_previous_binary_opens_and_carries_the_new_tables() {
        let (store, path) = fresh_store("participation-upgrade");
        // Simulate a v2 store: the participation tables absent, everything else present.
        {
            let conn = store.lock().expect("lock");
            conn.execute_batch(
                "DROP TABLE participation_cursors;
                 DROP TABLE participation_channels;
                 DROP TABLE participation_owed;",
            )
            .expect("drop");
            conn.execute(
                "UPDATE seller_meta SET value = '2' WHERE key = 'schema_version'",
                [],
            )
            .expect("downgrade");
        }
        drop(store);

        let reopened = SellerStore::open(&path).expect("reopen");
        assert_eq!(reopened.health().expect("health").schema_version, SCHEMA_VERSION);
        assert!(reopened.record_channel_joined(RELAY, "chan-1", "channel:chan-1", "e1", 100, 100).expect("join"));
        assert_eq!(reopened.participation_cursor(RELAY, "membership").expect("read"), None);
        let _ = std::fs::remove_file(&path);
    }
}
