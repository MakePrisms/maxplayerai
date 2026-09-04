//! The seller node's durable lifecycle state: `$MAXPLAYER_HOME/seller.sqlite`.
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

use crate::checks::EnvKind;
use crate::gateway::EventDraft;

/// Current on-disk schema version. v8 added `receipts.fee_bps` / `receipts.fee_sats` (the platform
/// fee, stage 1 — journaled, not remitted).
pub const SCHEMA_VERSION: i64 = 8;

/// The platform fee (stage 1) as journaled so far: what is owed on paper. Returned by
/// [`SellerStore::accrued_fees`]. Nothing in this stage moves the balance it reports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccruedFees {
    /// Sum of `fee_sats` over every receipt ever collected.
    pub total_fee_sats: u64,
    /// One entry per receipt row, oldest collection first — in practice one per paid job.
    pub by_job: Vec<JobFeeAccrual>,
}

/// One receipt's share of the accrued platform fee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFeeAccrual {
    pub job_id: String,
    /// The NET the seller received at the mint for this job — the base the fee was taken on.
    pub amount_sats: u64,
    /// The percentage in force at collection, in basis points (2.5% = 250).
    pub fee_bps: u32,
    /// `floor(amount_sats × fee_bps / 10_000)`, as written at collection.
    pub fee_sats: u64,
    pub received_at_unix: i64,
}

/// Resolve a nullable `payment` column into a [`crate::gateway::PaymentMode`].
///
/// NULL ⇒ [`crate::gateway::PaymentMode::Sat`] — a row written before the column existed, and every
/// such row was a priced job. An unrecognized value resolves the same way, fail-closed: a store this
/// binary cannot read a mode out of is read as PAID, never as free.
fn payment_mode_from_column(stored: Option<String>) -> crate::gateway::PaymentMode {
    match stored.as_deref().map(str::trim) {
        Some(crate::gateway::PAYMENT_NONE) => crate::gateway::PaymentMode::None,
        _ => crate::gateway::PaymentMode::Sat,
    }
}

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
    /// #686: the output type the buyer declared on the offer's `["output", …]` tag — a MIME / output
    /// type (`text/plain`, `application/json`). Mandatory on ingest, so a row this binary wrote always
    /// carries it; `None` ⇒ a row recorded before this column existed (absence, never a default —
    /// there is no output type to state that a buyer did not state).
    ///
    /// Journaled for the SAME reason as `requested_agent` above: execution can be a RESTART away from
    /// the claim, and the resumed job composes its agent prompt from this row. Unpersisted, the buyer's
    /// declared type would be gone for that job permanently.
    pub output: Option<String>,
    /// How this offer settles (§1.1), read off its `["param","payment", …]` tag at ingest.
    ///
    /// Journaled for the SAME reason as `requested_agent` and `output` above: execution can be a
    /// RESTART away from the claim, and the delivery row records the mode the job settled under. A
    /// row written before this column existed reads NULL ⇒ [`crate::gateway::PaymentMode::Sat`],
    /// which is correct by construction — every job recorded then was priced.
    pub payment_mode: crate::gateway::PaymentMode,
}

/// #591: the target + base a SERVED contribution job clones into its delivery workdir. The buyer's
/// pin is owner-scoped, so `owner_pubkey` records the target's identity; `clone_url` + `base_branch`
/// + `base_oid` are what the clone fetches and checks out. Absent ⇒ a from-scratch job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributionPin {
    pub owner_pubkey: String,
    pub clone_url: String,
    pub base_branch: String,
    pub base_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobChecks {
    pub job_id: String,
    pub declaration_bytes: Vec<u8>,
    pub env_kind: EnvKind,
    pub env_lock_ref: String,
    pub captured_at_unix: i64,
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
    /// Every variant, so a predicate over states can be checked against all of them rather than
    /// against the one that motivated it.
    pub const ALL: [Self; 5] = [
        Self::Awarded,
        Self::Executing,
        Self::Delivered,
        Self::Paid,
        Self::Failed,
    ];

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

    /// Execution is over for this job — nothing will run for it again, so a re-served offer naming
    /// it is not re-claimable. `Delivered` counts: the work is finished and only payment is
    /// outstanding, which is why it holds no execution slot either.
    pub(super) fn is_finished(self) -> bool {
        matches!(self, Self::Delivered | Self::Paid | Self::Failed)
    }

    /// The stored spelling — the same literal the write statements use.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Awarded => "awarded",
            Self::Executing => "executing",
            Self::Delivered => "delivered",
            Self::Paid => "paid",
            Self::Failed => "failed",
        }
    }

    /// Whether a job in this state is occupying execution capacity **right now**.
    ///
    /// This is the single definition of "in flight". [`SellerStore::jobs_in_flight`] builds its SQL
    /// from it and `should_resume_execution` answers with it, so the `queue_depth` on the wire and
    /// the set a restart re-drives cannot drift apart.
    ///
    /// `Delivered` is deliberately excluded: execution has finished and the job is awaiting payment,
    /// so it holds no slot — which is also why `resumable_jobs` selects it but resume does not
    /// execute it.
    pub fn occupies_execution_slot(self) -> bool {
        matches!(self, Self::Awarded | Self::Executing)
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
/// and every protocol/routing tag (`["v","1"]`, `["t","maxplayer"]`, the `e`/`p` tags) — so what the
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
                 requested_agent TEXT,
                 -- #686: the buyer's declared output type (the offer's `output` tag — a MIME / output
                 -- type). Mandatory on ingest, so this binary always writes it; NULL ⇒ an offer
                 -- recorded before this column existed, which states no output type to the agent.
                 output          TEXT,
                 -- The offer's PAYMENT MODE (spec 1.1): 'none' for a free job, 'sat' otherwise.
                 -- NULL => 'sat', which is what an offer recorded before this column existed reads
                 -- as: the same fail-closed direction the wire default takes.
                 --
                 -- Journaled for the SAME reason requested_agent and output are: execution can be a
                 -- RESTART away from the claim, and the delivery row below records the mode this
                 -- offer settled under. Unpersisted, a resumed free job would write its delivery as
                 -- paid.
                 payment         TEXT
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
                 --
                 -- EMPTY STRING => a FREE claim (spec 2.2): this seat claimed the job and authored
                 -- NO creq, because a free trade has no payment terms. It is NOT null, and that is
                 -- load-bearing: the presence of this ROW is how job_creq answers whether we claimed
                 -- the job at all (#814/#626), so a free claim must be present-and-empty rather than
                 -- absent. Widening the column to NULL would need a table rebuild, which migrate's
                 -- additive-only contract forbids on a live money store.
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
                 updated_at_unix INTEGER NOT NULL,
                 -- The delivery commit oid, journaled immediately AFTER a successful push and BEFORE
                 -- the receipt sign+enqueue (#552). On a still-`awarded`/`executing` row it means the
                 -- delivery was pushed but the enqueue was interrupted: resume FINALIZES from this
                 -- commit (re-sign + enqueue) instead of re-running the agent. NULL ⇒ never pushed.
                 pushed_commit   TEXT,
                 -- #563: a RELAY-DERIVED settled-elsewhere marker. Set only when a resume refine
                 -- fetched POSITIVE settlement evidence for this offer from the relay: our own
                 -- already-published result, or a buyer receipt (settled with us or another seat),
                 -- for the live-deadline residual resume_action would otherwise re-drive. Written
                 -- AFTER that evidence is in hand (arm-after-the-event), never speculatively, so a
                 -- crash mid-derive leaves the row re-checkable. Provenance-honest: relay-derived,
                 -- DISTINCT from a local deliveries row. NULL means not derived-settled; a unix ts
                 -- means when we derived it.
                 settled_elsewhere_at_unix INTEGER
             );
             -- One delivery per job (the seller-authored snapshot the daemon published).
             CREATE TABLE IF NOT EXISTS deliveries (
                 job_id          TEXT PRIMARY KEY,
                 result_ref      TEXT NOT NULL,
                 delivered_at_unix INTEGER NOT NULL,
                 -- Spec 3.2: how this job settled. 'none' => a FREE job: a free job still writes
                 -- the delivery record, with the payment recorded as none. 'sat' => a priced job.
                 -- NULL => a legacy row, which every reader resolves to 'sat'.
                 --
                 -- A free job's terminal jobs.state stays 'delivered' and never advances to 'paid':
                 -- widening the CHECK above needs a table rebuild, which migrate's additive-only
                 -- contract forbids on a live money store. THIS COLUMN is the fact that says the job
                 -- will never advance further. Operator tooling that reads delivered-but-not-paid as
                 -- ARREARS must read this column before it reports.
                 payment         TEXT
             );
             -- Collected receipts. `receipt_id` is UNIQUE — the dedup that stops a replayed
             -- payment from crediting the same job twice.
             CREATE TABLE IF NOT EXISTS receipts (
                 receipt_id      TEXT PRIMARY KEY,
                 job_id          TEXT NOT NULL,
                 amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                 received_at_unix INTEGER NOT NULL,
                 -- Platform fee (stage 1), written in the SAME insert as the receipt so no row can
                 -- exist without its fee. `fee_bps` is the percentage in force at collection, in
                 -- basis points (2.5% = 250); `fee_sats` is floor(amount_sats × fee_bps / 10_000).
                 -- ACCRUED, NOT REMITTED: these columns record what the fee came to; nothing reads
                 -- them to move money. A row from before v8 reads 0/0 — no fee was configured when
                 -- it was collected, so 0 is the fact, not a guess.
                 fee_bps         INTEGER NOT NULL DEFAULT 0 CHECK (fee_bps >= 0 AND fee_bps <= 10000),
                 fee_sats        INTEGER NOT NULL DEFAULT 0 CHECK (fee_sats >= 0)
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
             -- #591: the pinned target + base a SERVED contribution job clones into its delivery
             -- workdir. One row per contribution job, written at claim time (the only place the offer
             -- tags are in scope). ABSENT ⇒ a from-scratch job — the empty-workdir default. A store
             -- from a pre-#591 binary simply has no rows here, so the fallback is unchanged.
             CREATE TABLE IF NOT EXISTS contribution_pins (
                 job_id          TEXT PRIMARY KEY,
                 owner_pubkey    TEXT NOT NULL,
                 clone_url       TEXT NOT NULL,
                 base_branch     TEXT NOT NULL,
                 base_oid        TEXT NOT NULL,
                 created_at_unix INTEGER NOT NULL
             );
             -- #599: the exact checks declaration captured from the pinned base plus its resolved,
             -- immutable environment reference. Additive: older stores simply have no rows.
             CREATE TABLE IF NOT EXISTS job_checks (
                 job_id             TEXT PRIMARY KEY,
                 declaration_bytes  BLOB NOT NULL,
                 env_kind           TEXT NOT NULL,
                 env_lock_ref       TEXT NOT NULL,
                 captured_at_unix   INTEGER NOT NULL
             );",
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
    /// Every step is ADDITIVE and idempotent — a nullable or DEFAULT-valued column whose absence
    /// reads the same as its default. Nothing here rewrites or drops a row: this store holds live
    /// trade state.
    fn migrate(conn: &Connection) -> Result<(), StoreError> {
        if !Self::column_exists(conn, "offers", "requested_agent")? {
            conn.execute_batch("ALTER TABLE offers ADD COLUMN requested_agent TEXT;")?;
        }
        // #552: the pushed-delivery marker. A store from a pre-#552 binary reads NULL for its
        // awarded/executing rows and is armed going forward at push time. Pre-existing stale rows are
        // caught at resume by the deadline-lapse check (a passed deadline ⇒ fail, never re-drive); the
        // narrow live-deadline residual (pushed pre-marker, or settled elsewhere) is a tracked follow-up.
        if !Self::column_exists(conn, "jobs", "pushed_commit")? {
            conn.execute_batch("ALTER TABLE jobs ADD COLUMN pushed_commit TEXT;")?;
        }
        // #563: the relay-derived "settled elsewhere" marker. A store from a pre-#563 binary reads
        // NULL (not derived-settled) for its rows and is armed going forward at resume time. Additive
        // + idempotent, exactly like the columns above.
        if !Self::column_exists(conn, "jobs", "settled_elsewhere_at_unix")? {
            conn.execute_batch("ALTER TABLE jobs ADD COLUMN settled_elsewhere_at_unix INTEGER;")?;
        }
        // #686: the buyer's declared output type. A store from a pre-#686 binary reads NULL for its
        // existing offers — those jobs simply state no output type in their agent prompt — and is
        // armed going forward at the next ingest. Additive + idempotent, exactly like the columns above.
        if !Self::column_exists(conn, "offers", "output")? {
            conn.execute_batch("ALTER TABLE offers ADD COLUMN output TEXT;")?;
        }
        // §3.2 — the payment mode, on both the offer it was stated on and the delivery it settled
        // under. A store from a pre-free-lane binary reads NULL for its existing rows, which every
        // reader resolves to 'sat': those jobs were all priced, so the default is not a guess.
        // Additive + idempotent, exactly like the columns above; nothing is rewritten or dropped.
        if !Self::column_exists(conn, "offers", "payment")? {
            conn.execute_batch("ALTER TABLE offers ADD COLUMN payment TEXT;")?;
        }
        if !Self::column_exists(conn, "deliveries", "payment")? {
            conn.execute_batch("ALTER TABLE deliveries ADD COLUMN payment TEXT;")?;
        }
        // v8 — the platform fee (stage 1) journaled beside each receipt. A store from an earlier
        // binary reads 0 for both on every existing row, which is the truth of those rows: no fee was
        // configured when they were collected. `NOT NULL DEFAULT 0` is still additive — SQLite serves
        // the default for pre-existing rows without rewriting them. Additive + idempotent, exactly
        // like the columns above.
        if !Self::column_exists(conn, "receipts", "fee_bps")? {
            conn.execute_batch(
                "ALTER TABLE receipts ADD COLUMN fee_bps INTEGER NOT NULL DEFAULT 0
                     CHECK (fee_bps >= 0 AND fee_bps <= 10000);",
            )?;
        }
        if !Self::column_exists(conn, "receipts", "fee_sats")? {
            conn.execute_batch(
                "ALTER TABLE receipts ADD COLUMN fee_sats INTEGER NOT NULL DEFAULT 0
                     CHECK (fee_sats >= 0);",
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
                  requested_agent, output, payment)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                offer.output,
                offer.payment_mode.as_wire(),
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
                        requested_agent, output, payment
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
                        output: row.get(8)?,
                        // NULL ⇒ `Sat`. Resolved HERE rather than left to the caller so no reader
                        // of this row can accidentally treat "column absent" as a third state.
                        payment_mode: payment_mode_from_column(row.get::<_, Option<String>>(9)?),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// #591: persist the pin a served contribution job clones at execute time, keyed by job_id (the
    /// offer event id). INSERT OR IGNORE — idempotent with the offer/claim re-ingest, so a re-driven
    /// offer never double-writes. `claim_offer` writes this BEFORE `record_offer` so a crash can never
    /// leave an offer recorded (hence claimable/awardable/executable) without its pin — the only crash
    /// window strands a harmless orphan pin (no offer ⇒ no claim ⇒ no execute).
    pub fn record_contribution_pin(
        &self,
        job_id: &str,
        pin: &ContributionPin,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO contribution_pins
                 (job_id, owner_pubkey, clone_url, base_branch, base_oid, created_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                job_id,
                pin.owner_pubkey,
                pin.clone_url,
                pin.base_branch,
                pin.base_oid,
                now_unix,
            ],
        )?;
        Ok(changed == 1)
    }

    /// The pin for a job if it was recorded as a contribution; `None` ⇒ a from-scratch job (execute
    /// provisions an empty workdir). Read at execute time on BOTH the fresh-award and restart paths —
    /// the store is the only source of the served contribution's base there.
    pub fn contribution_pin(&self, job_id: &str) -> Result<Option<ContributionPin>, StoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT owner_pubkey, clone_url, base_branch, base_oid
                 FROM contribution_pins WHERE job_id = ?1",
                [job_id],
                |row| {
                    Ok(ContributionPin {
                        owner_pubkey: row.get(0)?,
                        clone_url: row.get(1)?,
                        base_branch: row.get(2)?,
                        base_oid: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn record_job_checks(
        &self,
        job_id: &str,
        declaration_bytes: &[u8],
        env_kind: EnvKind,
        env_lock_ref: &str,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO job_checks
                 (job_id, declaration_bytes, env_kind, env_lock_ref, captured_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(job_id) DO UPDATE SET
                 declaration_bytes = excluded.declaration_bytes,
                 env_kind = excluded.env_kind,
                 env_lock_ref = excluded.env_lock_ref,
                 captured_at_unix = excluded.captured_at_unix",
            params![
                job_id,
                declaration_bytes,
                env_kind.as_str(),
                env_lock_ref,
                now_unix,
            ],
        )?;
        Ok(())
    }

    pub fn job_checks(&self, job_id: &str) -> Result<Option<JobChecks>, StoreError> {
        let conn = self.lock()?;
        let raw = conn
            .query_row(
                "SELECT job_id, declaration_bytes, env_kind, env_lock_ref, captured_at_unix
                 FROM job_checks WHERE job_id = ?1",
                [job_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        raw.map(|(job_id, declaration_bytes, env_kind, env_lock_ref, captured_at_unix)| {
            let env_kind = EnvKind::from_wire(&env_kind)
                .ok_or_else(|| StoreError(format!("unknown persisted env_kind {env_kind:?}")))?;
            Ok(JobChecks {
                job_id,
                declaration_bytes,
                env_kind,
                env_lock_ref,
                captured_at_unix,
            })
        })
        .transpose()
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
        // `None` ⇒ a FREE claim (§2.2), journaled as the empty string so the row still exists and
        // `job_creq` still answers "yes, we claimed this".
        creq: Option<&str>,
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
            params![job_id, offer_id, creq.unwrap_or(""), now_unix],
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
    ///
    /// Returns the number of rows released — 0 or 1. A caller that ANNOUNCES a release must read
    /// this and log the disposition it actually got. The `state = 'claimed'` guard is deliberately
    /// narrow (it is what stops a release from regressing an awarded or terminal row), so a 0 is a
    /// normal outcome, not an error — and a caller that reports success on a 0 is reporting an
    /// action the UPDATE never performed. Use [`Self::claim_row_state`] to name the state instead.
    pub fn release_claim(&self, job_id: &str, now_unix: i64) -> Result<usize, StoreError> {
        let conn = self.lock()?;
        let released = conn.execute(
            "UPDATE claims SET state = 'released', updated_at_unix = ?2
             WHERE job_id = ?1 AND state = 'claimed'",
            params![job_id, now_unix],
        )?;
        Ok(released)
    }

    /// Offers recorded but never claimed and still fresh (`deadline_unix > now`): the capacity-skip
    /// set. `on_offer` records an offer BEFORE it reserves a slot, so an offer skipped for `SlotsBusy`
    /// leaves a row here with no claim. `reconsider_capacity_skips` re-drives these once a slot frees
    /// (#450) — a relay re-subscribe cannot, because the pool suppresses a re-delivery of an
    /// already-seen event. An offer that WAS claimed (even one whose claim later lapsed to `released`)
    /// has a claim row and is excluded: a lapsed-unawarded offer is not re-claimed.
    pub fn offers_awaiting_claim(&self, now_unix: i64) -> Result<Vec<Offer>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT offer_id, buyer_pubkey, amount_sats, unit, task, deadline_unix, targeted,
                    requested_agent, output, payment
             FROM offers
             WHERE deadline_unix > ?1
               AND offer_id NOT IN (SELECT job_id FROM claims)",
        )?;
        let rows = stmt.query_map([now_unix], |row| {
            Ok(Offer {
                offer_id: row.get(0)?,
                buyer_pubkey: row.get(1)?,
                amount_sats: row.get::<_, i64>(2)? as u64,
                unit: row.get(3)?,
                task: row.get(4)?,
                deadline_unix: row.get(5)?,
                targeted: row.get::<_, i64>(6)? != 0,
                requested_agent: row.get(7)?,
                output: row.get(8)?,
                payment_mode: payment_mode_from_column(row.get::<_, Option<String>>(9)?),
            })
        })?;
        let mut offers = Vec::new();
        for row in rows {
            offers.push(row?);
        }
        Ok(offers)
    }

    /// The claim row's state for `job_id` (`claimed` / `awarded` / `released`), or `None` if this
    /// node never parked a claim for it.
    ///
    /// Read by [`Self::release_claim`]'s callers to NAME the state when a release moved no row —
    /// the `state = 'claimed'` guard is narrow by design, and a log that cannot say which state
    /// blocked it reports a release it never made. Also the #450 capacity-skip regression's
    /// assertion that a lapsed claim's row survives as `released` (so a re-delivered offer dedups
    /// on it rather than being re-claimed) while the freed slot lets the capacity-skipped offer
    /// claim.
    pub fn claim_row_state(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let state = conn
            .query_row(
                "SELECT state FROM claims WHERE job_id = ?1",
                [job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(state)
    }

    // ---- Award ----------------------------------------------------------------------------------

    /// Record an award for `job_id`. The `award_id` (award event id) is deduped: the first sighting
    /// moves the claim to `awarded` and creates the job row; a re-seen award id is a
    /// [`Awarded::Duplicate`] no-op (never a second job). An award naming a claim this node never
    /// parked is recorded but creates no job ([`Awarded::NoClaim`]).
    ///
    /// ⛔ AUTHORIZATION IS THE CALLER'S. This writes the `awards` row on the strength of its
    /// arguments alone — it does NOT check that the award's author is the offer's buyer. Every caller
    /// must gate on that first (`on_award` via `match_award`, `on_accept` and the #814 suppression
    /// path inline), or a forged award writes a row and suppresses real work. Pass the buyer read
    /// from OUR OWN recorded offer, never the event's author — that is what keeps the check
    /// non-circular.
    ///
    /// #814 WIDENED WHAT A ROW MEANS, and a reader must know it: an `awards` row used to imply "we
    /// won this job", because the only caller held a claim. The suppression path now records an
    /// authentic buyer award for an offer we recorded but never claimed — someone ELSE's win — so the
    /// row means "an award for this job exists", nothing more. The discriminator for "we won" is a
    /// CLAIM row (what this function's own `claim_state` read uses), never the presence of an award.
    /// [`Self::offers_awarded_elsewhere`] is the complement, and encodes that in SQL.
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

    /// Buyer pubkey carried by the recorded AWARD for this job.
    pub fn job_award_buyer(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        Ok(conn.query_row(
            "SELECT buyer_pubkey FROM awards WHERE job_id = ?1 ORDER BY created_at_unix, award_id LIMIT 1",
            [job_id], |row| row.get(0)).optional()?)
    }

    /// #814 — offers this node recorded that were AWARDED TO SOMEONE ELSE and are still live:
    /// `(offer_id, buyer_pubkey, deadline_unix)` for every offer holding an award row, holding NO
    /// claim row of ours, whose `deadline_unix` has not passed. The boot re-hydration source for the
    /// in-memory suppression cache, so the claim gate is correct on the FIRST event after a restart
    /// rather than only after a relay backfill succeeds — "relay deafness manufactures absence"
    /// (#560/#563), so a redelivery that never arrives must not be what stands between us and
    /// re-publishing a losing claim.
    ///
    /// Three clauses, each load-bearing:
    /// - `NOT IN (SELECT job_id FROM claims)` IS the hard invariant of #814 in SQL: a job we hold a
    ///   claim for can never be re-hydrated as suppressed, so a resumed award is never stranded (the
    ///   #563 FOIL). It also carries the widened `awards` meaning — an award row alone no longer says
    ///   whose win it was, and only the absent claim says it was not ours. `claims` is keyed by
    ///   `job_id` and matched against `offers.offer_id` because a claim's job id IS its offer id
    ///   (see [`Self::offers_awaiting_claim`], which excludes on the same identity).
    /// - `deadline_unix > ?1` keeps this FAIL-OPEN and bounded: a suppression outlives neither the
    ///   offer it belongs to nor the gate's own `Lapsed` check, so the set cannot grow without limit.
    /// - `buyer_pubkey` comes from the OFFER, never from the award — the same non-circularity the
    ///   live path relies on. A forged award that somehow reached the table still re-hydrates under
    ///   the REAL buyer's key, so it can never satisfy the buyer-bound gate at claim time.
    ///
    /// `EXISTS` rather than a JOIN so two award rows for one job (they are possible — see
    /// [`Self::job_award_time`]) yield ONE row here, not a duplicate.
    pub fn offers_awarded_elsewhere(
        &self,
        now_unix: i64,
    ) -> Result<Vec<(String, String, i64)>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT offer_id, buyer_pubkey, deadline_unix
             FROM offers
             WHERE deadline_unix > ?1
               AND offer_id NOT IN (SELECT job_id FROM claims)
               AND EXISTS (SELECT 1 FROM awards WHERE awards.job_id = offers.offer_id)",
        )?;
        let rows = stmt.query_map([now_unix], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
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

    /// Journal the pushed delivery commit for a job (#552). Called immediately AFTER a successful
    /// push and BEFORE the receipt sign+enqueue, so a crash in that window leaves a durable marker:
    /// on resume the job FINALIZES from this commit (re-sign + enqueue) rather than re-running the
    /// agent. Idempotent — last write wins; does NOT change `state` (the atomic advance to
    /// `delivered` stays with `deliver_and_enqueue`).
    pub fn mark_pushed(&self, job_id: &str, commit: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET pushed_commit = ?2, updated_at_unix = ?3 WHERE job_id = ?1",
            params![job_id, commit, now_unix],
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
        payment_mode: crate::gateway::PaymentMode,
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
        // §3.2 — ruling 3's record, with the payment stated explicitly rather than inferred from
        // the absence of a receipt. A free job's row is written here and never advances past
        // `state = 'delivered'` below; `collect_receipt` is the only writer of `'paid'` and no
        // kind-1059 wrap ever arrives for a free job.
        tx.execute(
            "INSERT INTO deliveries (job_id, result_ref, delivered_at_unix, payment)
             VALUES (?1, ?2, ?3, ?4)",
            params![job_id, result_ref, now_unix, payment_mode.as_wire()],
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
    ///
    /// Returns the number of rows failed — 0 or 1. This is the write `ResumeAction::SkipLapsed`
    /// uses to heal a stale `awarded` row, so a caller that treats it as unconditional can report a
    /// heal that never happened: the `state != 'paid'` guard (and an absent row) both yield 0.
    pub fn fail_job(&self, job_id: &str, now_unix: i64) -> Result<usize, StoreError> {
        let conn = self.lock()?;
        let failed = conn.execute(
            "UPDATE jobs SET state = 'failed', updated_at_unix = ?2
             WHERE job_id = ?1 AND state != 'paid'",
            params![job_id, now_unix],
        )?;
        Ok(failed)
    }

    /// Record a collected receipt and mark the job paid. The `receipt_id` is deduped: the first
    /// sighting credits the job (`New`); a replay is a [`Collected::Duplicate`] no-op that never
    /// marks paid a second time. This is the money-safe boundary — a job is only ever `paid` once,
    /// keyed on the unique receipt id.
    ///
    /// The platform fee (stage 1) rides in the SAME insert: `fee_bps` is the percentage in force
    /// and `fee_sats` what it came to on `amount_sats` (the NET the mint handed over). One row, one
    /// write — a receipt can never exist without its fee, and a fee is never recorded for a payment
    /// that did not land. The caller computes both only after the redeem classified `Finalize`.
    /// Journaled, not remitted: nothing reads these columns to move money.
    pub fn collect_receipt(
        &self,
        receipt_id: &str,
        job_id: &str,
        amount_sats: u64,
        fee_bps: u32,
        fee_sats: u64,
        now_unix: i64,
    ) -> Result<Collected, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO receipts
                 (receipt_id, job_id, amount_sats, received_at_unix, fee_bps, fee_sats)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                receipt_id,
                job_id,
                amount_sats as i64,
                now_unix,
                i64::from(fee_bps),
                fee_sats as i64
            ],
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

    /// What the platform fee (stage 1) has come to: the all-time total and the receipt rows behind
    /// it, oldest collection first. This is the read-out a later remit stage settles against; it is
    /// a query and nothing more — no call here or anywhere in this stage moves the balance it reports.
    ///
    /// Rows collected before the fee existed report `fee_bps = 0, fee_sats = 0` (the migration
    /// default), which is what they owed. `by_job` carries one entry per receipt row; the collect
    /// path receipts a job at most once (`has_receipt` guards the redeem), so that is one per job.
    pub fn accrued_fees(&self) -> Result<AccruedFees, StoreError> {
        let conn = self.lock()?;
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(fee_sats), 0) FROM receipts",
            [],
            |row| row.get(0),
        )?;
        let mut statement = conn.prepare(
            "SELECT job_id, amount_sats, fee_bps, fee_sats, received_at_unix
             FROM receipts
             ORDER BY received_at_unix ASC, receipt_id ASC",
        )?;
        let by_job = statement
            .query_map([], |row| {
                Ok(JobFeeAccrual {
                    job_id: row.get(0)?,
                    amount_sats: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                    fee_bps: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                    fee_sats: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                    received_at_unix: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AccruedFees {
            total_fee_sats: u64::try_from(total).unwrap_or(0),
            by_job,
        })
    }

    /// Whether a delivery has been journaled for `job_id` (#552). A delivery row is written only by
    /// [`Self::deliver_and_enqueue`], atomically with the `delivered` state advance — so this is the
    /// durable proof the result was already produced and enqueued, independent of the `state` column
    /// (belt-and-braces against a lagged state).
    pub fn has_delivery(&self, job_id: &str) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let found = conn
            .query_row(
                "SELECT 1 FROM deliveries WHERE job_id = ?1 LIMIT 1",
                [job_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    /// The delivery commit oid journaled at push time for `job_id`, if any (#552). `Some` on a
    /// still-`awarded`/`executing` row means the delivery was pushed but the enqueue was interrupted
    /// — resume finalizes from it instead of re-running the agent. `None` ⇒ never pushed.
    pub fn pushed_commit(&self, job_id: &str) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        let commit: Option<String> = conn
            .query_row(
                "SELECT pushed_commit FROM jobs WHERE job_id = ?1",
                [job_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(commit)
    }

    /// #563 — mark a job RELAY-DERIVED as settled elsewhere: a resume refine fetched POSITIVE
    /// settlement evidence for its offer from the relay (our own already-published result, or a buyer
    /// receipt — settled with us or another seat). Written ONLY after that evidence is in hand
    /// (arm-after-the-event), never speculatively on the way into the query, so a crash between issuing
    /// the derive and getting evidence leaves the row re-checkable next restart. Idempotent — last
    /// write wins; does NOT change `state`. Provenance-honest: relay-DERIVED, distinct from a local
    /// `deliveries` row (which only [`Self::deliver_and_enqueue`] writes).
    pub fn mark_settled_elsewhere(&self, job_id: &str, now_unix: i64) -> Result<(), StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE jobs SET settled_elsewhere_at_unix = ?2, updated_at_unix = ?2 WHERE job_id = ?1",
            params![job_id, now_unix],
        )?;
        Ok(())
    }

    /// Whether `job_id` was relay-derived as settled elsewhere (see [`Self::mark_settled_elsewhere`]).
    /// A resume refine consults this FIRST and short-circuits — a durable marker means it need never
    /// re-query the relay.
    pub fn has_settled_elsewhere(&self, job_id: &str) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let found = conn
            .query_row(
                "SELECT 1 FROM jobs WHERE job_id = ?1 AND settled_elsewhere_at_unix IS NOT NULL",
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
    ///
    /// ORDERED, and that is what makes "restart-STABLE" true rather than merely usually-true. The
    /// `awards` PRIMARY KEY is the AWARD id, not the job id, so ONE JOB CAN HOLD MORE THAN ONE ROW —
    /// `SellerNodeRunner::on_accept`'s doc names the hazard in the code's own words, and
    /// `execute_job`'s notes a redundant second award "seen live in the smoke". A bare `SELECT` then
    /// returns whichever row SQLite hands back first, which is free to differ across restarts — and a
    /// differing authored-at is exactly the invariant-2 break this value exists to prevent. Taking the
    /// EARLIEST (`created_at_unix`, `award_id` as the tie-break) is deterministic for any row set, and
    /// matches [`Self::job_award_buyer`] one function below, which has always read this way.
    ///
    /// #814 adds a SECOND route to two rows — an award for an offer we recorded but never claimed is
    /// now persisted too — so this ordering is a precondition of that change, not a nicety.
    pub fn job_award_time(&self, job_id: &str) -> Result<Option<i64>, StoreError> {
        let conn = self.lock()?;
        let ts: Option<i64> = conn
            .query_row(
                "SELECT created_at_unix FROM awards WHERE job_id = ?1
                 ORDER BY created_at_unix, award_id LIMIT 1",
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
    /// The claim-time creq journaled for a job.
    ///
    /// ⛔ THREE STATES, NOT TWO. `None` ⇒ this node holds NO claim for the job — the discriminator
    /// #814/#626 rest on. `Some("")` ⇒ a claim exists and it is FREE (§2.2), which carries no
    /// payment terms. `Some(creq)` ⇒ a priced claim. A caller asking "did we claim this" wants
    /// `is_some()`; a caller wanting the payment TERMS must also reject the empty string.
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
    /// How many jobs are occupying execution capacity right now.
    ///
    /// ⚠ **Not [`HealthSnapshot::jobs`]**, which is `COUNT(*)` over every job row ever written and is
    /// never pruned. Reading that as "in flight" is what made a seat publish `accepting=n`
    /// permanently from its first job onward (#313): the count's healthy baseline grew with use, so
    /// the seat that had delivered the most looked the busiest and stopped being selectable.
    ///
    /// The state list comes from [`JobState::occupies_execution_slot`] rather than being written out
    /// here, so this count and the resume predicate have one definition between them.
    pub fn jobs_in_flight(&self) -> Result<u32, StoreError> {
        let conn = self.lock()?;
        let occupying: Vec<String> = JobState::ALL
            .iter()
            .filter(|state| state.occupies_execution_slot())
            .map(|state| format!("'{}'", state.as_str()))
            .collect();
        // Every element is a compile-time constant from JobState, so there is no untrusted input in
        // this string; a bound-parameter list cannot be spliced into `IN (...)` without building it.
        let total = count(
            &conn,
            &format!(
                "SELECT COUNT(*) FROM jobs WHERE state IN ({})",
                occupying.join(",")
            ),
        )?;
        Ok(u32::try_from(total).unwrap_or(u32::MAX))
    }

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
            "maxplayer-seller-store-{label}-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    fn fresh_store(label: &str) -> (SellerStore, std::path::PathBuf) {
        let path = temp_db(label);
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");
        (store, path)
    }

    /// Put a job row in an exact state. Direct SQL on purpose: driving five states through the
    /// public transition path would make the state-coverage test below a test of the transitions
    /// instead of a test of the predicate.
    fn insert_job(store: &SellerStore, job_id: &str, state: JobState) {
        let conn = store.lock().expect("lock");
        conn.execute(
            "INSERT INTO jobs (job_id, offer_id, agent_name, state, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, NULL, ?3, 1, 1)",
            params![job_id, format!("offer-{job_id}"), state.as_str()],
        )
        .expect("insert job");
    }

    /// The two narrowing mutators must REPORT that they moved nothing, not just decline to move it.
    ///
    /// Both guard on state (`release_claim` on `= 'claimed'`, `fail_job` on `!= 'paid'`), so zero
    /// rows is a normal outcome rather than an error — which is exactly why the count has to reach
    /// the caller. A discarded rowcount here is what let a losing seat announce a release it never
    /// performed while its claim sat at `awarded` (#626), and `fail_job` is the write the lapse heal
    /// depends on, so the same silence there would report a repair that did not happen.
    #[test]
    fn the_narrowing_mutators_report_when_they_move_nothing() {
        let (store, path) = fresh_store("rowcount");

        // fail_job: a real transition reports 1; `paid` is terminal and reports 0.
        insert_job(&store, "job-live", JobState::Awarded);
        assert_eq!(store.fail_job("job-live", 5).expect("fail live"), 1, "an awarded row fails");
        insert_job(&store, "job-paid", JobState::Paid);
        assert_eq!(
            store.fail_job("job-paid", 5).expect("fail paid"),
            0,
            "`paid` is terminal — the guard refuses, and the caller must be able to see that"
        );
        assert_eq!(
            store.fail_job("job-absent", 5).expect("fail absent"),
            0,
            "no row at all is also zero"
        );

        // release_claim: only a still-`claimed` row releases; an awarded one reports 0.
        let draft = crate::gateway::claim_draft("job-c", &"b".repeat(64), &"s".repeat(64), crate::gateway::ClaimPayment::Sat("creq"), &[], &Default::default());
        store
            .claim_and_enqueue("job-c", "job-c", Some("creq"), &draft, 1, 9_999_999_999, 1)
            .expect("claim");
        assert_eq!(store.release_claim("job-c", 6).expect("release"), 1, "a parked claim releases");
        assert_eq!(
            store.release_claim("job-c", 7).expect("re-release"),
            0,
            "already released — the second call moves nothing and says so"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The stored spellings, written out by hand.
    ///
    /// Deliberately NOT derived from `as_str` — the point is to disagree with it if it drifts. These
    /// same five literals also live in the `jobs.state` CHECK constraint and in `JobState::parse`, so
    /// a silent rename in one place would otherwise surface as a runtime CHECK violation or an
    /// "unknown job state" error rather than a failing test.
    #[test]
    fn job_state_spellings_are_the_literals_the_schema_stores() {
        assert_eq!(JobState::Awarded.as_str(), "awarded");
        assert_eq!(JobState::Executing.as_str(), "executing");
        assert_eq!(JobState::Delivered.as_str(), "delivered");
        assert_eq!(JobState::Paid.as_str(), "paid");
        assert_eq!(JobState::Failed.as_str(), "failed");
        for state in JobState::ALL {
            assert_eq!(
                JobState::parse(state.as_str()),
                Some(state),
                "{state:?} must round-trip through its stored spelling"
            );
        }
    }

    #[test]
    fn only_awarded_and_executing_occupy_an_execution_slot() {
        assert!(JobState::Awarded.occupies_execution_slot());
        assert!(JobState::Executing.occupies_execution_slot());
        // Delivered has finished executing and is awaiting payment — it holds no slot.
        assert!(!JobState::Delivered.occupies_execution_slot());
        assert!(!JobState::Paid.occupies_execution_slot());
        assert!(!JobState::Failed.occupies_execution_slot());
    }

    /// Enumerated over EVERY variant rather than the two that motivated the change, so adding a state
    /// without deciding whether it occupies a slot fails here instead of on the wire.
    #[test]
    fn jobs_in_flight_counts_exactly_the_occupying_states() {
        for state in JobState::ALL {
            let (store, path) = fresh_store(&format!("inflight-{}", state.as_str()));
            insert_job(&store, "job-1", state);
            let expected = u32::from(state.occupies_execution_slot());
            assert_eq!(
                store.jobs_in_flight().expect("count"),
                expected,
                "a single {state:?} job must count as {expected} in flight"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    /// ★ THE #313 REGRESSION, and it must start from a NON-EMPTY store.
    ///
    /// The old predicate was `health().jobs > 0` — `COUNT(*)` over every row ever written — so a seat
    /// that had finished work advertised `accepting=n` forever. A fixture starting from an empty
    /// store cannot tell the fix from the bug: both report zero. The discriminator is terminal rows
    /// PRESENT, and this test asserts the two counts DISAGREE, which is the whole defect.
    #[test]
    fn a_store_holding_only_terminal_jobs_reports_none_in_flight() {
        let (store, path) = fresh_store("terminal-only");
        insert_job(&store, "job-paid-1", JobState::Paid);
        insert_job(&store, "job-paid-2", JobState::Paid);
        insert_job(&store, "job-failed", JobState::Failed);
        insert_job(&store, "job-delivered", JobState::Delivered);

        assert_eq!(
            store.jobs_in_flight().expect("count"),
            0,
            "a seat whose jobs are all finished is FREE and must advertise accepting=y"
        );
        assert_eq!(
            store.health().expect("health").jobs,
            4,
            "health().jobs stays the lifetime total — that is its job, which is why it must not be \
             read as in-flight"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn jobs_in_flight_is_a_count_not_a_flag() {
        let (store, path) = fresh_store("inflight-depth");
        insert_job(&store, "job-a", JobState::Awarded);
        insert_job(&store, "job-b", JobState::Executing);
        insert_job(&store, "job-c", JobState::Awarded);
        insert_job(&store, "job-done", JobState::Paid);
        assert_eq!(
            store.jobs_in_flight().expect("count"),
            3,
            "three occupying jobs must report 3 — a 0/1 answer is the #313 shape"
        );
        let _ = std::fs::remove_file(&path);
    }

    fn sample_offer(id: &str) -> Offer {
        Offer {
            payment_mode: crate::gateway::PaymentMode::Sat,
            offer_id: id.to_owned(),
            buyer_pubkey: "b".repeat(64),
            amount_sats: 100,
            unit: "sat".to_owned(),
            task: "do the thing".to_owned(),
            deadline_unix: 10_000,
            targeted: true,
            requested_agent: None,
            output: Some("text/plain".to_owned()),
        }
    }

    /// A wire-valid draft carrying the protocol tags every maxplayer event needs.
    fn wire_draft(kind: u16) -> EventDraft {
        use crate::gateway::{MAXPLAYER_TAG, PROTOCOL_VERSION};
        EventDraft::new(
            kind,
            vec![
                TagSpec::new(["t", MAXPLAYER_TAG]),
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

    // TOOTH (#686) — the output type the buyer DECLARED is journaled with the other offer facts and
    // READS BACK across a reopen. Same reason as the harness request above: execution can be a
    // restart away from the claim, and the resumed job composes its agent prompt from this row, so a
    // type that lived only in memory would be gone for that job permanently.
    //
    // Bite (measured): drop `output` from the INSERT column list in `record_offer` (or from the
    // `offer_row` SELECT) and this test goes red — the reopened row reads None.
    #[test]
    fn the_declared_output_type_survives_a_reopen() {
        let path = temp_db("declared-output");
        let _ = std::fs::remove_file(&path);
        {
            let store = SellerStore::open(&path).expect("open");
            let mut offer = sample_offer("o1");
            offer.output = Some("application/json".to_owned());
            store.record_offer(&offer, 1).expect("record");
            // A second offer with a DIFFERENT type: the read must return each row's own value, which
            // a single hardcoded default would not.
            store.record_offer(&sample_offer("o2"), 1).expect("record");
        }
        let store = SellerStore::open(&path).expect("reopen");
        assert_eq!(
            store.offer_row("o1").expect("row").expect("o1").output.as_deref(),
            Some("application/json"),
            "the declared output type must survive a restart — the prompt is composed from this row"
        );
        assert_eq!(
            store.offer_row("o2").expect("row").expect("o2").output.as_deref(),
            Some("text/plain")
        );
        // The capacity-skip re-drive reads offers through a DIFFERENT statement; it carries the
        // field too, so a re-considered offer is not silently stripped of it.
        let awaiting = store.offers_awaiting_claim(1).expect("awaiting");
        let o1 = awaiting.iter().find(|offer| offer.offer_id == "o1").expect("o1 awaits a claim");
        assert_eq!(o1.output.as_deref(), Some("application/json"));
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
        // #686: the same store predates the `output` column. It migrates and its live row reads as
        // "no declared type" — that job's prompt simply states none, rather than the read failing.
        assert_eq!(row.output, None, "an offer from before the column declared no output type");
        // Forward from here the column is armed: a fresh ingest into the SAME migrated store keeps
        // its type, so the migration adds a working column and not just a silent one.
        store.record_offer(&sample_offer("new"), 2).expect("record into the migrated store");
        assert_eq!(
            store.offer_row("new").expect("read").expect("new row").output.as_deref(),
            Some("text/plain")
        );
        // Migration is idempotent: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
        assert!(store.offer_row("old").expect("read").is_some());
        let _ = std::fs::remove_file(&path);
    }

    // #563 — the relay-derived settled-elsewhere marker round-trips over a real row and DEFAULTS false.
    // A false default is load-bearing: absence of the marker must read as "not derived-settled" so the
    // resume refine still queries the relay (never a silent skip on a missing fact).
    #[test]
    fn settled_elsewhere_marker_round_trips_and_defaults_false() {
        let (store, path) = fresh_store("settled-elsewhere");
        insert_job(&store, "job-se", JobState::Awarded);
        assert!(
            !store.has_settled_elsewhere("job-se").expect("read unmarked"),
            "an un-derived job is not settled-elsewhere (default false ⇒ the refine still checks)"
        );
        store.mark_settled_elsewhere("job-se", 1_234).expect("mark");
        assert!(
            store.has_settled_elsewhere("job-se").expect("read marked"),
            "after the relay-derived marker the job reads settled-elsewhere"
        );
        // Idempotent: a later derive re-writes the ts, stays true, never errors.
        store.mark_settled_elsewhere("job-se", 5_678).expect("re-mark");
        assert!(store.has_settled_elsewhere("job-se").expect("read"), "idempotent — last write wins");
        let _ = std::fs::remove_file(&path);
    }

    // #591 — the contribution pin round-trips and is ABSENT for a from-scratch job (the empty-workdir
    // default execute_job falls back to). INSERT OR IGNORE makes a re-ingest of the same offer a
    // no-op, never a second write — the property the crash-safe pin-before-offer ordering relies on.
    #[test]
    fn contribution_pin_round_trips_and_absent_for_from_scratch() {
        let (store, path) = fresh_store("contribution-pin");
        assert_eq!(
            store.contribution_pin("scratch").expect("read"),
            None,
            "a from-scratch job has no pin"
        );
        let pin = ContributionPin {
            owner_pubkey: "b".repeat(64),
            clone_url: "https://relay.maxplayer.ai/git/owner/repo.git".to_owned(),
            base_branch: "main".to_owned(),
            base_oid: "a".repeat(40),
        };
        assert!(
            store.record_contribution_pin("job-c", &pin, 7).expect("record"),
            "the first write inserts"
        );
        assert_eq!(
            store.contribution_pin("job-c").expect("read"),
            Some(pin.clone()),
            "the pin reads back"
        );
        assert!(
            !store.record_contribution_pin("job-c", &pin, 8).expect("re-record"),
            "INSERT OR IGNORE ⇒ a re-ingest is idempotent"
        );
        assert_eq!(
            store.contribution_pin("job-c").expect("read"),
            Some(pin),
            "unchanged after the re-ingest"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn job_checks_round_trip_and_absent_row_is_none() {
        let (store, path) = fresh_store("job-checks");
        assert_eq!(store.job_checks("absent").expect("read absent"), None);
        let bytes = b"schema = 1\n# retain exact base bytes\n";
        store
            .record_job_checks(
                "checked-job",
                bytes,
                EnvKind::ContainerImage,
                "registry.example/checks@sha256:abcd",
                1234,
            )
            .expect("record checks");
        assert_eq!(
            store.job_checks("checked-job").expect("read checks"),
            Some(JobChecks {
                job_id: "checked-job".to_owned(),
                declaration_bytes: bytes.to_vec(),
                env_kind: EnvKind::ContainerImage,
                env_lock_ref: "registry.example/checks@sha256:abcd".to_owned(),
                captured_at_unix: 1234,
            })
        );
        let _ = std::fs::remove_file(&path);
    }

    // #591 — a v4 store (no contribution_pins) opens CLEAN under v5: the additive CREATE TABLE IF NOT
    // EXISTS adds the new table, the version bumps, and the pre-existing money-path row is UNTOUCHED
    // (no ALTER/DROP crosses claims/wallet tables). The store then persists a pin.
    #[test]
    fn a_v4_store_opens_clean_under_v5_and_gains_contribution_pins() {
        let path = temp_db("pre-contribution-pins");
        let _ = std::fs::remove_file(&path);
        // A v4 store: version 4 + a live money-path claims row, WITHOUT contribution_pins.
        {
            let conn = Connection::open(&path).expect("create v4 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '4');
                 CREATE TABLE claims (
                     job_id TEXT PRIMARY KEY, offer_id TEXT NOT NULL, state TEXT NOT NULL,
                     creq TEXT NOT NULL, created_at_unix INTEGER NOT NULL, updated_at_unix INTEGER NOT NULL
                 );
                 INSERT INTO claims VALUES ('live-job', 'live-job', 'awarded', 'live-creq', 1, 1);",
            )
            .expect("v4 schema");
        }
        let store = SellerStore::open(&path).expect("a v4 store opens clean under v5");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION,
            "the version bumped to v5"
        );
        let pin = ContributionPin {
            owner_pubkey: "b".repeat(64),
            clone_url: "https://x/git/o/r.git".to_owned(),
            base_branch: "main".to_owned(),
            base_oid: "a".repeat(40),
        };
        assert!(store.record_contribution_pin("live-job", &pin, 2).expect("record"));
        assert_eq!(store.contribution_pin("live-job").expect("read"), Some(pin));
        drop(store);
        // The pre-existing money-path row was NOT touched by the v5 migration.
        let conn = Connection::open(&path).expect("reopen raw");
        let creq: String = conn
            .query_row("SELECT creq FROM claims WHERE job_id = 'live-job'", [], |row| row.get(0))
            .expect("the v4 claims row survives the v5 migration");
        assert_eq!(creq, "live-creq");
        let _ = std::fs::remove_file(&path);
    }

    // TOOTH — a store written by a pre-#563 binary (a #552-era jobs table WITH pushed_commit but
    // WITHOUT settled_elsewhere_at_unix) opens, MIGRATES additively, and reads its existing rows as
    // "not settled-elsewhere". Without the ALTER an upgraded node would fail every has_settled_elsewhere
    // read on a live store.
    #[test]
    fn a_store_from_before_the_settled_elsewhere_column_migrates_and_reads_false() {
        let path = temp_db("pre-settled-elsewhere-schema");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create old store");
            conn.execute_batch(
                "CREATE TABLE jobs (
                     job_id          TEXT PRIMARY KEY,
                     offer_id        TEXT NOT NULL,
                     agent_name      TEXT,
                     state           TEXT NOT NULL
                         CHECK (state IN ('awarded','executing','delivered','paid','failed')),
                     created_at_unix INTEGER NOT NULL,
                     updated_at_unix INTEGER NOT NULL,
                     pushed_commit   TEXT
                 );
                 INSERT INTO jobs (job_id, offer_id, state, created_at_unix, updated_at_unix)
                 VALUES ('old-job', 'old-offer', 'awarded', 1, 1);",
            )
            .expect("old schema");
        }

        let store = SellerStore::open(&path).expect("open migrates");
        assert!(
            !store.has_settled_elsewhere("old-job").expect("read"),
            "a row from before the column is not derived-settled (the refine must still check the relay)"
        );
        // The migrated column is writable: the refine can arm it going forward on the live store.
        store.mark_settled_elsewhere("old-job", 2).expect("mark on migrated store");
        assert!(store.has_settled_elsewhere("old-job").expect("read"), "marker persists post-migration");
        // Idempotent: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
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
    // carrying the full wire-valid draft — right kind AND the `["v","1"]` + `["t","maxplayer"]` protocol
    // tags a live buyer requires), not merely that no error was returned. Deleting the
    // `enqueue_event` call in `claim_and_enqueue` leaves the claim row but no outbox row, so the
    // length / kind / tag assertions fail — the revert turns this test red.
    #[test]
    fn tooth_outbox_write_lands_atomically_with_the_claim() {
        use crate::gateway::{JOB_CLAIM_KIND, MAXPLAYER_TAG, PROTOCOL_VERSION};
        let (store, path) = fresh_store("outbox-redonrevert");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        assert_eq!(
            store
                .claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 500, 999, 1)
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
        assert!(has_tag(&item.draft, "v", PROTOCOL_VERSION), "draft must carry [\"v\",\"1\"]");
        assert!(has_tag(&item.draft, "t", MAXPLAYER_TAG), "draft must carry [\"t\",\"maxplayer\"]");

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
            store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("first"),
            Claimed::New
        );
        // A replay carrying a DIFFERENT creq is a no-op: neither the outbox nor the journaled
        // claim-time creq is overwritten. The first creq — the one that was on the wire — stands.
        assert_eq!(
            store.claim_and_enqueue(&job, &offer, Some("creqB"), &claim(), 1, 999, 2).expect("replay"),
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

    /// `job_award_time` must be DETERMINISTIC when one job holds more than one award row. The
    /// `awards` PRIMARY KEY is the award id, not the job id, so two rows are possible — a redundant
    /// award was "seen live in the smoke" (see `run.rs` `execute_job`). The value is the delivery
    /// commit's authored-at, so a reading that can vary across restarts breaks invariant 2
    /// (byte-identical re-created delivery), which is the property the buyer verifies.
    ///
    /// ⛔ THE ROW ORDER HERE IS THE TEST. Inserting the LATER-timestamped award FIRST is what makes
    /// the pre-fix query deterministically WRONG: an unordered `SELECT … LIMIT 1` walks the table in
    /// rowid order and hands back that first-inserted later row, while the ordered query returns the
    /// earlier one. Insert them the other way round and the pre-fix code returns the right answer BY
    /// LUCK — the test would pass against the very bug it exists to catch.
    ///
    /// RED ON REVERT: drop `ORDER BY created_at_unix, award_id LIMIT 1` from `job_award_time` and
    /// this fails with `assertion left == right failed … left: Some(900), right: Some(100)`.
    #[test]
    fn job_award_time_is_deterministic_when_a_job_holds_two_awards() {
        let (store, path) = fresh_store("award-time-order");
        let job = "j".repeat(64);
        let offer = "o".repeat(64);
        let buyer = "b".repeat(64);
        store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("claim");

        // The LATER award is inserted FIRST — see the note above.
        store.record_award(&"z".repeat(64), &job, &buyer, 900).expect("later award");
        store.record_award(&"a".repeat(64), &job, &buyer, 100).expect("earlier award");

        assert_eq!(
            store.job_award_time(&job).expect("award time"),
            Some(100),
            "with two award rows the EARLIEST must win, whatever order they were written in"
        );
        // The sibling read has always been ordered; assert they agree rather than trusting it.
        assert_eq!(
            store.job_award_buyer(&job).expect("award buyer"),
            Some(buyer),
            "job_award_buyer resolves the same row"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// #814 — the boot re-hydration source. An offer is returned ONLY when an award exists for it, we
    /// hold NO claim, and its deadline has not passed. Each of the three clauses is asserted by a
    /// counter-example, because a query that simply returned every offer would satisfy the happy path
    /// alone.
    ///
    /// The claim clause is the #563 FOIL in SQL: re-hydrating a suppression for a job we hold would
    /// strand our own award. RED ON REVERT: delete `AND offer_id NOT IN (SELECT job_id FROM claims)`
    /// and the "ours" case appears in the result set.
    #[test]
    fn offers_awarded_elsewhere_selects_only_live_unclaimed_awarded_offers() {
        let (store, path) = fresh_store("awarded-elsewhere");
        let buyer = "b".repeat(64);
        let now = 5_000;

        let offer_at = |id: &str, deadline: i64| {
            let mut offer = sample_offer(id);
            offer.buyer_pubkey = buyer.clone();
            offer.deadline_unix = deadline;
            store.record_offer(&offer, 1).expect("record offer");
        };
        // (1) THE CASE: recorded, awarded, unclaimed, still live.
        offer_at(&"1".repeat(64), 10_000);
        store.record_award(&"w1".repeat(32), &"1".repeat(64), &buyer, 2).expect("award 1");
        // (2) awarded and live, but WE HOLD THE CLAIM — ours, never suppressed.
        offer_at(&"2".repeat(64), 10_000);
        store
            .claim_and_enqueue(&"2".repeat(64), &"2".repeat(64), Some("creq"), &claim(), 1, 999, 1)
            .expect("claim 2");
        store.record_award(&"w2".repeat(32), &"2".repeat(64), &buyer, 2).expect("award 2");
        // (3) recorded and unclaimed, but NO award — nothing decided it.
        offer_at(&"3".repeat(64), 10_000);
        // (4) awarded and unclaimed, but its deadline has PASSED — fail-open, already `Lapsed`.
        offer_at(&"4".repeat(64), 4_000);
        store.record_award(&"w4".repeat(32), &"4".repeat(64), &buyer, 2).expect("award 4");

        let rows = store.offers_awarded_elsewhere(now).expect("read");
        assert_eq!(
            rows,
            vec![("1".repeat(64), buyer.clone(), 10_000)],
            "only the recorded + awarded + unclaimed + live offer re-hydrates"
        );

        // Two award rows for one job must still yield ONE row (EXISTS, not a JOIN).
        store.record_award(&"w5".repeat(32), &"1".repeat(64), &buyer, 3).expect("second award");
        assert_eq!(
            store.offers_awarded_elsewhere(now).expect("read again").len(),
            1,
            "a second award row must not duplicate the offer"
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
        store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("claim");

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
        store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("claim");
        store.record_award(&"w".repeat(64), &job, &buyer, 2).expect("award");
        store.mark_executing(&job, 3).expect("exec");

        assert!(store
            .deliver_and_enqueue(&job, "ref-1", crate::gateway::PaymentMode::Sat, &result(), 4, 999, 5)
            .expect("deliver"));
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Delivered));
        // Replay: no second delivery, no second result enqueue.
        assert!(!store
            .deliver_and_enqueue(&job, "ref-1", crate::gateway::PaymentMode::Sat, &result(), 4, 999, 6)
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
        store.claim_and_enqueue(&job, &offer, Some("creqA"), &claim(), 1, 999, 1).expect("claim");
        store.record_award(&"w".repeat(64), &job, &"b".repeat(64), 2).expect("award");

        assert_eq!(
            store
                .collect_receipt(&receipt, &job, 100, 0, 0, 3)
                .expect("collect"),
            Collected::New
        );
        assert_eq!(store.job_state(&job).expect("state"), Some(JobState::Paid));
        assert_eq!(
            store
                .collect_receipt(&receipt, &job, 100, 0, 0, 4)
                .expect("replay"),
            Collected::Duplicate,
            "a replayed receipt must not credit twice"
        );
        let _ = std::fs::remove_file(&path);
    }

    // Platform fee (stage 1) — the fee is journaled in the SAME row as the receipt and the read-out
    // sums exactly what was written: per job and all-time. A replay adds nothing to either.
    #[test]
    fn collect_receipt_journals_the_fee_in_the_same_row_and_accrued_fees_sums_it() {
        let (store, path) = fresh_store("fee-journal");
        assert_eq!(
            store.accrued_fees().expect("empty read-out"),
            AccruedFees::default(),
            "nothing collected ⇒ nothing accrued"
        );

        // Job A: 100 sats net at 2% (200 bp) ⇒ 2 sats. Job B: 1_000 sats at 2.5% (250 bp) ⇒ 25.
        let job_a = "a".repeat(64);
        let job_b = "b".repeat(64);
        store
            .claim_and_enqueue(&job_a, &"o".repeat(64), Some("creqA"), &claim(), 1, 999, 1)
            .expect("claim a");
        store
            .record_award(&"w".repeat(64), &job_a, &"b".repeat(64), 2)
            .expect("award a");
        assert_eq!(
            store
                .collect_receipt(&"r".repeat(64), &job_a, 100, 200, 2, 3)
                .expect("collect a"),
            Collected::New
        );
        assert_eq!(
            store.job_state(&job_a).expect("state"),
            Some(JobState::Paid),
            "the receipt still marks paid"
        );
        assert_eq!(
            store
                .collect_receipt(&"s".repeat(64), &job_b, 1_000, 250, 25, 4)
                .expect("collect b"),
            Collected::New
        );

        let accrued = store.accrued_fees().expect("read-out");
        assert_eq!(
            accrued.total_fee_sats, 27,
            "all-time total is the sum of the rows"
        );
        assert_eq!(
            accrued.by_job,
            vec![
                JobFeeAccrual {
                    job_id: job_a.clone(),
                    amount_sats: 100,
                    fee_bps: 200,
                    fee_sats: 2,
                    received_at_unix: 3
                },
                JobFeeAccrual {
                    job_id: job_b.clone(),
                    amount_sats: 1_000,
                    fee_bps: 250,
                    fee_sats: 25,
                    received_at_unix: 4
                },
            ],
            "one row per receipt, oldest first, each carrying the rate in force and what it came to"
        );

        // A replayed wrap — even one claiming a different fee — is a dedup no-op and accrues nothing.
        assert_eq!(
            store
                .collect_receipt(&"r".repeat(64), &job_a, 100, 10_000, 100, 5)
                .expect("replay"),
            Collected::Duplicate
        );
        assert_eq!(
            store.accrued_fees().expect("read-out").total_fee_sats,
            27,
            "a replay accrues nothing"
        );

        // The row survives a close/reopen: read back off disk, not from memory.
        drop(store);
        let store = SellerStore::open(&path).expect("reopen");
        assert_eq!(store.accrued_fees().expect("read-out").total_fee_sats, 27);
        let _ = std::fs::remove_file(&path);
    }

    // Platform fee (stage 1) — a store written by a pre-v8 binary (a receipts table WITHOUT the fee
    // columns) opens, migrates additively, and reads 0 for every existing receipt: no fee was
    // configured when those payments were collected. The migrated store then journals a fee on its
    // next collection, and a second open is a no-op.
    #[test]
    fn a_store_from_before_the_fee_columns_migrates_and_reads_zero() {
        let path = temp_db("pre-fee-columns");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create old store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '7');
                 CREATE TABLE receipts (
                     receipt_id      TEXT PRIMARY KEY,
                     job_id          TEXT NOT NULL,
                     amount_sats     INTEGER NOT NULL CHECK (amount_sats >= 0),
                     received_at_unix INTEGER NOT NULL
                 );
                 INSERT INTO receipts VALUES ('old-receipt', 'old-job', 21, 7);",
            )
            .expect("v7 schema");
        }

        let store = SellerStore::open(&path).expect("a v7 store opens clean under v8");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        let accrued = store.accrued_fees().expect("read-out on a migrated store");
        assert_eq!(
            accrued.total_fee_sats, 0,
            "nothing accrued before the fee existed"
        );
        assert_eq!(
            accrued.by_job,
            vec![JobFeeAccrual {
                job_id: "old-job".to_owned(),
                amount_sats: 21,
                fee_bps: 0,
                fee_sats: 0,
                received_at_unix: 7,
            }],
            "the pre-existing receipt is untouched and reads a fee of 0/0"
        );
        assert!(
            store.has_receipt("old-job").expect("read"),
            "the legacy receipt still counts as paid"
        );

        // The migrated columns are writable: the next collection journals its fee.
        assert_eq!(
            store
                .collect_receipt("new-receipt", "new-job", 100, 200, 2, 8)
                .expect("collect on migrated store"),
            Collected::New
        );
        assert_eq!(store.accrued_fees().expect("read-out").total_fee_sats, 2);

        // RE-ENTRANT: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(
            store.health().expect("health").schema_version,
            SCHEMA_VERSION
        );
        assert_eq!(store.accrued_fees().expect("read-out").by_job.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expire_outbox_stops_the_publisher_from_sending() {
        let (store, path) = fresh_store("expire");
        let job = "j".repeat(64);
        store.claim_and_enqueue(&job, &"o".repeat(64), Some("creqA"), &claim(), 1, 100, 1).expect("claim");
        // now=200 is past expires_at=100.
        assert_eq!(store.expire_outbox(200).expect("expire"), 1);
        assert!(store.pending_outbox(200).expect("pending").is_empty());
        assert_eq!(
            store.outbox_row(&format!("claim:{job}")).expect("row").expect("exists").0,
            "expired"
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod free_lane_tests {
    use super::*;
    use crate::gateway::PaymentMode;
    use rusqlite::Connection;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db(label: &str) -> std::path::PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "maxplayer-free-lane-store-{label}-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    fn wire_draft(kind: u16) -> EventDraft {
        EventDraft::new(kind, vec![crate::gateway::TagSpec::new(["t", "maxplayer"])], "")
    }

    fn offer_row(job: &str, mode: PaymentMode) -> Offer {
        Offer {
            offer_id: job.to_owned(),
            buyer_pubkey: "b".repeat(64),
            amount_sats: if mode.is_free() { 0 } else { 21 },
            unit: "sat".to_owned(),
            task: "t".to_owned(),
            deadline_unix: 2_000_000_000,
            targeted: true,
            requested_agent: None,
            output: Some("text/plain".to_owned()),
            payment_mode: mode,
        }
    }

    fn payment_column(path: &std::path::Path, table: &str, job: &str) -> Option<String> {
        let conn = Connection::open(path).expect("reopen raw");
        conn.query_row(
            &format!("SELECT payment FROM {table} WHERE {} = ?1", if table == "offers" { "offer_id" } else { "job_id" }),
            [job],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("read payment column")
    }

    /// §3.2 — RULING 3's RECORD. A free job still writes a delivery row, and that row says `none`.
    ///
    /// Both modes are asserted from the same store, because "writes 'none'" alone would pass a
    /// writer that wrote `none` for everything — which would mis-report every priced delivery in
    /// the market as unpaid-forever.
    #[test]
    fn a_free_delivery_records_payment_none_and_a_priced_one_records_sat() {
        let path = temp_db("delivery-payment");
        let _ = std::fs::remove_file(&path);
        let store = SellerStore::open(&path).expect("open");

        for (job, mode, expected) in [("free-job", PaymentMode::None, "none"), ("paid-job", PaymentMode::Sat, "sat")] {
            store.record_offer(&offer_row(job, mode), 1).expect("record offer");
            store
                .claim_and_enqueue(job, job, if mode.is_free() { None } else { Some("creqA") }, &wire_draft(crate::gateway::JOB_CLAIM_KIND), 1, 9_999, 1)
                .expect("claim");
            store
                .record_award(&format!("award-{job}"), job, &"b".repeat(64), 2)
                .expect("award");
            assert!(
                store
                    .deliver_and_enqueue(job, "ref", mode, &wire_draft(crate::gateway::JOB_RESULT_KIND), 3, 9_999, 3)
                    .expect("deliver"),
                "the delivery row must be written for BOTH modes — ruling 3"
            );
            assert_eq!(
                payment_column(&path, "deliveries", job).as_deref(),
                Some(expected),
                "{job} delivery row payment column"
            );
        }

        // A free job's terminal state stays 'delivered' — it never advances to 'paid', and the
        // deliveries.payment column is the fact that says so.
        assert_eq!(store.job_state("free-job").expect("state"), Some(JobState::Delivered));
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// The offer's mode is JOURNALED, so a delivery that happens a RESTART after the claim still
    /// records the mode the job was posted under.
    ///
    /// Without the persisted column a resumed free job would read its offer back as `Sat` — the
    /// fail-closed default — and write its delivery as PAID, which is the one row an operator's
    /// arrears tooling reads.
    #[test]
    fn the_offers_payment_mode_survives_a_restart() {
        let path = temp_db("offer-mode-restart");
        let _ = std::fs::remove_file(&path);
        {
            let store = SellerStore::open(&path).expect("open");
            store.record_offer(&offer_row("free-job", PaymentMode::None), 1).expect("record");
            store.record_offer(&offer_row("paid-job", PaymentMode::Sat), 1).expect("record");
        }
        let store = SellerStore::open(&path).expect("reopen — the process died and came back");
        assert_eq!(
            store.offer_row("free-job").expect("read").expect("row").payment_mode,
            PaymentMode::None
        );
        assert_eq!(
            store.offer_row("paid-job").expect("read").expect("row").payment_mode,
            PaymentMode::Sat
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// The v6→v7 migration is ADDITIVE, IDEMPOTENT and RE-ENTRANT on a live store, and it does not
    /// touch the `jobs.state` CHECK.
    ///
    /// The pre-existing money-path rows are read back after the migration, so a migration that
    /// rewrote or dropped a row fails here rather than in production. The second open proves
    /// re-entrance: `column_exists` must make the ALTERs no-ops, not errors.
    #[test]
    fn a_v6_store_migrates_to_v7_additively_and_re_entrantly() {
        let path = temp_db("v6-to-v7");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("create v6 store");
            conn.execute_batch(
                "CREATE TABLE seller_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO seller_meta VALUES ('schema_version', '6');
                 CREATE TABLE offers (
                     offer_id TEXT PRIMARY KEY, buyer_pubkey TEXT NOT NULL,
                     amount_sats INTEGER NOT NULL CHECK (amount_sats >= 0), unit TEXT NOT NULL,
                     task TEXT NOT NULL, deadline_unix INTEGER NOT NULL, targeted INTEGER NOT NULL,
                     created_at_unix INTEGER NOT NULL, requested_agent TEXT, output TEXT
                 );
                 INSERT INTO offers VALUES ('legacy-job','bb','21','sat','t',2000000000,1,1,NULL,'text/plain');
                 CREATE TABLE deliveries (
                     job_id TEXT PRIMARY KEY, result_ref TEXT NOT NULL, delivered_at_unix INTEGER NOT NULL
                 );
                 INSERT INTO deliveries VALUES ('legacy-job','legacy-ref',7);",
            )
            .expect("v6 schema");
        }

        let store =
            SellerStore::open(&path).expect("a v6 store opens clean under the current schema");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
        assert_eq!(
            SCHEMA_VERSION, 8,
            "v7 was the free lane; v8 added the receipt fee columns"
        );

        // The legacy rows SURVIVE and read as PAID — correct by construction, because every job
        // recorded before this column existed was priced.
        let legacy = store.offer_row("legacy-job").expect("read").expect("the v6 offer survives");
        assert_eq!(legacy.amount_sats, 21, "the pre-existing money-path row is untouched");
        assert_eq!(
            legacy.payment_mode,
            PaymentMode::Sat,
            "a NULL payment column resolves to PAID, never to a third state"
        );
        assert_eq!(
            payment_column(&path, "deliveries", "legacy-job"),
            None,
            "the migration ADDS a nullable column; it does not backfill or rewrite a live row"
        );

        // RE-ENTRANT: opening again neither errors nor double-adds.
        drop(store);
        let store = SellerStore::open(&path).expect("second open is a no-op");
        assert_eq!(store.health().expect("health").schema_version, SCHEMA_VERSION);
        let again = store.offer_row("legacy-job").expect("read").expect("row");
        assert_eq!(again.amount_sats, 21);
        drop(store);

        // §3.2 — the `jobs.state` CHECK is UNTOUCHED: 'settled_free' was rejected as a terminal
        // state precisely because widening this constraint needs a table rebuild, which migrate's
        // additive-only contract forbids on a live money store.
        let conn = Connection::open(&path).expect("reopen raw");
        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='jobs'", [], |row| row.get(0))
            .expect("jobs DDL");
        assert!(
            ddl.contains("CHECK (state IN ('awarded','executing','delivered','paid','failed'))"),
            "the jobs.state CHECK must be byte-unchanged by the free lane: {ddl}"
        );
        assert!(
            !ddl.contains("settled_free"),
            "no new terminal state was added — deliveries.payment carries the fact instead: {ddl}"
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}

