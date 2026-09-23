//! Durable content outbox/inbox. One transaction stores all required recipients before
//! a caller can send any copy; service receipt is never an execution/payment ACK.
use super::{Binding, Error, PreparedContent, Result};
use nostr_sdk::prelude::{Event, JsonUtil, Keys, PublicKey};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;

const MAX_PENDING_RECORDS: i64 = 1024;
const MAX_PENDING_PER_AUTHOR: i64 = 128;
const UNKNOWN_TTL_SECS: u64 = 24 * 60 * 60;
pub struct ContentStore {
    db: Connection,
}
fn db_error(_: rusqlite::Error) -> Error {
    Error("content database failure")
}
impl ContentStore {
    pub fn open(path: &Path) -> Result<Self> {
        // Establish restrictive mode BEFORE sqlite writes plaintext (including rollback journals).
        use std::fs::OpenOptions;
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(path)
            .map_err(|_| Error("cannot open private content database"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| Error("cannot secure content database"))?;
        }
        let db = Connection::open(path).map_err(db_error)?;
        Self::initialize(db)
    }
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory().map_err(db_error)?)
    }
    fn initialize(db: Connection) -> Result<Self> {
        db.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_error)?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS content_records (
                job TEXT NOT NULL, id TEXT NOT NULL, envelope TEXT NOT NULL,
                commitment TEXT NOT NULL, author TEXT NOT NULL,
                received INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(job,id));
            CREATE TABLE IF NOT EXISTS content_outbox (
                job TEXT NOT NULL, id TEXT NOT NULL, recipient TEXT NOT NULL,
                relay_accepted INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(job,id,recipient),
                FOREIGN KEY(job,id) REFERENCES content_records(job,id));
            CREATE TABLE IF NOT EXISTS content_offers (
                buyer TEXT NOT NULL, job TEXT NOT NULL, offer TEXT NOT NULL,
                PRIMARY KEY(buyer,job));
            CREATE TABLE IF NOT EXISTS content_carriers (
                id TEXT PRIMARY KEY, author TEXT NOT NULL, job TEXT NOT NULL,
                event TEXT NOT NULL, relay_accepted INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS content_unknown (
                author TEXT NOT NULL, job TEXT NOT NULL, id TEXT NOT NULL,
                envelope TEXT NOT NULL, expires_at INTEGER NOT NULL,
                PRIMARY KEY(author,job,id));
            CREATE TABLE IF NOT EXISTS content_intents (
                key TEXT PRIMARY KEY, envelope TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS content_prepared (
                job TEXT NOT NULL, id TEXT NOT NULL, author TEXT NOT NULL, envelope TEXT NOT NULL,
                PRIMARY KEY(job,id,author));
            CREATE TABLE IF NOT EXISTS content_selection (
                offer TEXT PRIMARY KEY, claim TEXT NOT NULL, award TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS content_events (
                id TEXT PRIMARY KEY, root TEXT NOT NULL, event TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS content_events_root ON content_events(root);
            CREATE TABLE IF NOT EXISTS content_scan (
                recipient TEXT PRIMARY KEY, next_start INTEGER NOT NULL, through INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS content_cursor (
                recipient TEXT PRIMARY KEY, received_at INTEGER NOT NULL);",
        )
        .map_err(db_error)?;
        Ok(Self { db })
    }
    /// Call only after authenticating the signed offer; a job ID is never reusable.
    pub fn reserve_offer(&mut self, buyer: &str, job: &str, offer: &str) -> Result<()> {
        for id in [buyer, job, offer] {
            super::require_hex(id, 32)?;
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        tx.execute(
            "INSERT OR IGNORE INTO content_offers VALUES (?1,?2,?3)",
            params![buyer, job, offer],
        )
        .map_err(db_error)?;
        let old: String = tx
            .query_row(
                "SELECT offer FROM content_offers WHERE buyer=?1 AND job=?2",
                params![buyer, job],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        if old != offer {
            return Err(Error("job id already bound to another offer"));
        }
        tx.commit().map_err(db_error)
    }
    /// Open discovery has no encrypted task, but its exact signed OFFER still
    /// belongs in the durable outbox before the first send.
    pub fn enqueue_open_offer(
        &mut self,
        offer: &Event,
        service: &str,
        host: &super::wire::HostPolicy,
    ) -> Result<()> {
        let tags = super::wire::validate_private(offer, host)?;
        if offer.kind.as_u16() != 3401 || tags.get("discovery") != Some("open") {
            return Err(Error("not an open discovery offer"));
        }
        self.remember_event(offer, offer, &offer.pubkey.to_hex(), service, host)?;
        self.db
            .execute(
                "INSERT OR IGNORE INTO content_carriers(id,author,job,event) VALUES(?1,?2,?3,?4)",
                params![
                    offer.id.to_hex(),
                    offer.pubkey.to_hex(),
                    tags.required("job")?,
                    offer.as_json()
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn enqueue(&mut self, content: &PreparedContent, expected: &Binding<'_>) -> Result<()> {
        self.enqueue_inner(content, expected, None)
    }
    /// Verify the signed carrier chain, then persist the exact signed event and all
    /// recipient copies in ONE transaction. A crash cannot orphan an unpublished offer.
    pub fn enqueue_signed(
        &mut self,
        content: &PreparedContent,
        context: &SignedContext<'_>,
    ) -> Result<()> {
        context.validate(content)?;
        self.enqueue_inner(content, &context.binding(content)?, Some(context.carrier))
    }
    fn enqueue_inner(
        &mut self,
        content: &PreparedContent,
        expected: &Binding<'_>,
        carrier: Option<&Event>,
    ) -> Result<()> {
        content.validate_binding(expected)?;
        let b = content.body();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT envelope FROM content_records WHERE job=?1 AND id=?2",
                params![b.job_id, b.message_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if old.as_ref().is_some_and(|v| v != content.envelope()) {
            return Err(Error("conflicting logical content id"));
        }
        if old.is_none() {
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM (SELECT DISTINCT job,id FROM content_outbox WHERE relay_accepted=0)",[],|r|r.get(0)).map_err(db_error)?;
            let author_count: i64 = tx.query_row("SELECT COUNT(*) FROM content_records r WHERE author=?1 AND EXISTS (SELECT 1 FROM content_outbox o WHERE o.job=r.job AND o.id=r.id AND o.relay_accepted=0)",[&b.author],|r|r.get(0)).map_err(db_error)?;
            if count >= MAX_PENDING_RECORDS || author_count >= MAX_PENDING_PER_AUTHOR {
                return Err(Error("content outbox full"));
            }
            tx.execute("INSERT INTO content_records(job,id,envelope,commitment,author) VALUES(?1,?2,?3,?4,?5)",params![b.job_id,b.message_id,content.envelope(),content.commitment(),b.author]).map_err(db_error)?;
        }
        for recipient in &b.recipients {
            tx.execute(
                "INSERT OR IGNORE INTO content_outbox(job,id,recipient) VALUES(?1,?2,?3)",
                params![b.job_id, b.message_id, recipient],
            )
            .map_err(db_error)?;
        }
        if let Some(carrier) = carrier {
            if carrier.kind.as_u16() == 3401 {
                tx.execute(
                    "INSERT OR IGNORE INTO content_offers VALUES(?1,?2,?3)",
                    params![carrier.pubkey.to_hex(), b.job_id, carrier.id.to_hex()],
                )
                .map_err(db_error)?;
                let reserved: String = tx
                    .query_row(
                        "SELECT offer FROM content_offers WHERE buyer=?1 AND job=?2",
                        params![carrier.pubkey.to_hex(), b.job_id],
                        |r| r.get(0),
                    )
                    .map_err(db_error)?;
                if reserved != carrier.id.to_hex() {
                    return Err(Error("job id already bound to another offer"));
                }
            }
            tx.execute(
                "INSERT OR IGNORE INTO content_carriers(id,author,job,event) VALUES(?1,?2,?3,?4)",
                params![
                    carrier.id.to_hex(),
                    carrier.pubkey.to_hex(),
                    b.job_id,
                    carrier.as_json()
                ],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }
    /// Pin signed public evidence for a trade this local participant is following.
    /// Syntax is checked here; callers still validate the entire chain on every use.
    /// Never use cached presence as proof of an award or content availability.
    pub fn remember_event(
        &mut self,
        event: &Event,
        offer: &Event,
        recipient: &str,
        service: &str,
        host: &super::wire::HostPolicy,
    ) -> Result<()> {
        let o = super::wire::validate_private(offer, host)?;
        let e = super::wire::validate_private(event, host)?;
        super::require_hex(recipient, 32)?;
        super::require_hex(service, 32)?;
        let root = offer.id.to_hex();
        if offer.kind.as_u16() != 3401
            || e.get("job") != o.get("job")
            || (event.id != offer.id && e.get("root") != Some(root.as_str()))
            || (o.get("discovery") == Some("targeted")
                && recipient != service
                && recipient != offer.pubkey.to_hex()
                && !o.participants.contains(recipient))
        {
            return Err(Error("event evidence is outside local trade scope"));
        }
        self.reserve_offer(&offer.pubkey.to_hex(), o.required("job")?, &root)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM content_events WHERE root=?1",
                [&root],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        let known: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM content_events WHERE id=?1)",
                [event.id.to_hex()],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        if !known && count >= 2048 {
            return Err(Error("trade evidence cache full"));
        }
        for item in [offer, event] {
            tx.execute(
                "INSERT OR IGNORE INTO content_events(id,root,event) VALUES(?1,?2,?3)",
                params![item.id.to_hex(), root, item.as_json()],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }
    /// Public v2 evidence lives in its own database; this only caches signed
    /// events. Every use revalidates the full selection chain.
    pub fn remember_public(&mut self, offer: &Event, event: &Event) -> Result<()> {
        super::public_v2::validate_offer(offer)?;
        if event.id != offer.id {
            super::public_v2::validate_child(offer, event)?;
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM content_events WHERE root=?1",
                [offer.id.to_hex()],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        let known: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM content_events WHERE id=?1)",
                [event.id.to_hex()],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        if !known && count >= 2048 {
            return Err(Error("public trade evidence cache full"));
        }
        for item in [offer, event] {
            tx.execute(
                "INSERT OR IGNORE INTO content_events VALUES(?1,?2,?3)",
                params![item.id.to_hex(), offer.id.to_hex(), item.as_json()],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }
    pub fn select_public(&mut self, offer: &Event, claim: &Event, award: &Event) -> Result<()> {
        super::public_v2::validate_selection(offer, claim, award)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        tx.execute(
            "INSERT OR IGNORE INTO content_selection VALUES(?1,?2,?3)",
            params![offer.id.to_hex(), claim.id.to_hex(), award.id.to_hex()],
        )
        .map_err(db_error)?;
        let ids: (String, String) = tx
            .query_row(
                "SELECT claim,award FROM content_selection WHERE offer=?1",
                [offer.id.to_hex()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db_error)?;
        if ids != (claim.id.to_hex(), award.id.to_hex()) {
            return Err(Error("public trade already selected"));
        }
        tx.commit().map_err(db_error)
    }
    /// Signed but NOT necessarily chain-authorized evidence. Resolve exact references
    /// and validate selection/content binding before execution, display or settlement.
    pub fn event(&self, id: &str) -> Result<Option<Event>> {
        super::require_hex(id, 32)?;
        let json: Option<String> = self
            .db
            .query_row("SELECT event FROM content_events WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_error)?;
        let event = json
            .map(|json| super::wire::parse_signed(&json))
            .transpose()?;
        if event.as_ref().is_some_and(|event| event.id.to_hex() != id) {
            return Err(Error("stored event identity mismatch"));
        }
        Ok(event)
    }
    /// Retrieve locally authored immutable bytes for restart/retry. This is not an
    /// inbox accessor and cannot make somebody else's unverified content executable.
    pub fn authored(&self, job: &str, id: &str, author: &str) -> Result<Option<PreparedContent>> {
        let json: Option<String> = self
            .db
            .query_row(
                "SELECT envelope FROM content_records WHERE job=?1 AND id=?2 AND author=?3 UNION SELECT envelope FROM content_prepared WHERE job=?1 AND id=?2 AND author=?3",
                params![job, id, author],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        json.map(|json| PreparedContent::decode(&json)).transpose()
    }
    pub fn offer_for_job(&self, buyer: &str, job: &str) -> Result<Option<Event>> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT offer FROM content_offers WHERE buyer=?1 AND job=?2",
                params![buyer, job],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        match id {
            Some(id) => self.event(&id),
            None => Ok(None),
        }
    }
    /// The selected chain is immutable locally just as it is at the private Git host.
    /// Evidence inserts may survive a crash; only this final transaction admits execution.
    pub fn remember_selection(
        &mut self,
        offer: &Event,
        claim: &Event,
        award: &Event,
        recipient: &str,
        service: &str,
        host: &super::wire::HostPolicy,
    ) -> Result<()> {
        super::lifecycle::validate_selection(offer, claim, award, host)?;
        if recipient != offer.pubkey.to_hex()
            && recipient != claim.pubkey.to_hex()
            && recipient != service
        {
            return Err(Error("not a selected trade participant"));
        }
        for event in [offer, claim, award] {
            self.remember_event(event, offer, recipient, service, host)?;
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        tx.execute(
            "INSERT OR IGNORE INTO content_selection VALUES(?1,?2,?3)",
            params![offer.id.to_hex(), claim.id.to_hex(), award.id.to_hex()],
        )
        .map_err(db_error)?;
        let ids: (String, String) = tx
            .query_row(
                "SELECT claim,award FROM content_selection WHERE offer=?1",
                [offer.id.to_hex()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db_error)?;
        if ids != (claim.id.to_hex(), award.id.to_hex()) {
            return Err(Error("trade already has a different selection"));
        }
        tx.commit().map_err(db_error)
    }
    pub fn selection(&self, offer: &str) -> Result<Option<(Event, Event)>> {
        let ids: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT claim,award FROM content_selection WHERE offer=?1",
                [offer],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_error)?;
        match ids {
            None => Ok(None),
            Some((claim, award)) => Ok(Some((
                self.event(&claim)?
                    .ok_or(Error("selected claim evidence missing"))?,
                self.event(&award)?
                    .ok_or(Error("selected award evidence missing"))?,
            ))),
        }
    }
    /// Allocate the immutable nonce/body once, before computing a result cosignature.
    /// A restart with changed answer/context is a conflict, not a new message under
    /// the existing delivery intent. It must become a separately authorized revision.
    pub fn prepare_once(
        &mut self,
        key: &str,
        mut body: super::ContentBody,
    ) -> Result<PreparedContent> {
        if key.is_empty() || key.len() > 256 {
            return Err(Error("invalid content intent key"));
        }
        body.validate()?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT envelope FROM content_intents WHERE key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let content = if let Some(old) = old {
            let content = PreparedContent::decode(&old)?;
            body.message_id = content.body().message_id.clone();
            if &body != content.body() {
                return Err(Error("content intent changed after preparation"));
            }
            content
        } else {
            let content = PreparedContent::new(body)?;
            tx.execute(
                "INSERT INTO content_intents VALUES(?1,?2)",
                params![key, content.envelope()],
            )
            .map_err(db_error)?;
            content
        };
        let existing: Option<String> = tx
            .query_row(
                "SELECT envelope FROM content_prepared WHERE job=?1 AND id=?2 AND author=?3",
                params![
                    content.body().job_id,
                    content.body().message_id,
                    content.body().author
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if existing
            .as_deref()
            .is_some_and(|bytes| bytes != content.envelope())
        {
            return Err(Error(
                "prepared content identity reused with different bytes",
            ));
        }
        tx.execute(
            "INSERT OR IGNORE INTO content_prepared VALUES(?1,?2,?3,?4)",
            params![
                content.body().job_id,
                content.body().message_id,
                content.body().author,
                content.envelope()
            ],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(content)
    }
    pub fn pending_carriers(&self, author: &str, limit: usize) -> Result<Vec<Event>> {
        super::require_hex(author, 32)?;
        let mut query=self.db.prepare("SELECT event FROM content_carriers WHERE author=?1 AND relay_accepted=0 ORDER BY rowid LIMIT ?2").map_err(db_error)?;
        let rows = query
            .query_map(params![author, limit.min(256)], |r| r.get::<_, String>(0))
            .map_err(db_error)?;
        rows.map(|r| super::wire::parse_signed(&r.map_err(db_error)?))
            .collect()
    }
    pub fn carrier_accepted(&mut self, event: &Event) -> Result<()> {
        if self
            .db
            .execute(
                "UPDATE content_carriers SET relay_accepted=1 WHERE id=?1 AND author=?2",
                params![event.id.to_hex(), event.pubkey.to_hex()],
            )
            .map_err(db_error)?
            != 1
        {
            return Err(Error("unknown content carrier"));
        }
        Ok(())
    }
    pub fn pending(&self, author: &str, limit: usize) -> Result<Vec<PendingCopy>> {
        super::require_hex(author, 32)?;
        let mut q = self.db.prepare("SELECT r.envelope,o.recipient FROM content_outbox o JOIN content_records r USING(job,id) WHERE r.author=?1 AND o.relay_accepted=0 ORDER BY o.rowid LIMIT ?2").map_err(db_error)?;
        let rows = q
            .query_map(params![author, limit.min(256)], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(db_error)?;
        rows.map(|row| {
            let (envelope, recipient) = row.map_err(db_error)?;
            Ok(PendingCopy {
                content: PreparedContent::decode(&envelope)?,
                recipient,
            })
        })
        .collect()
    }
    /// Only relay publication success calls this. It is NOT a service-decryption acknowledgment.
    pub fn relay_accepted(&mut self, job: &str, id: &str, recipient: &str) -> Result<()> {
        if self.db.execute("UPDATE content_outbox SET relay_accepted=1 WHERE job=?1 AND id=?2 AND recipient=?3",params![job,id,recipient]).map_err(db_error)? != 1 {
            return Err(Error("unknown content copy"));
        }
        Ok(())
    }
    /// Verified inbox insertion, usable equally by buyer, seller and independent service consumer.
    /// Unknown-job messages are not inserted here or exposed to an executor.
    pub fn receive(
        &mut self,
        content: &PreparedContent,
        expected: &Binding<'_>,
        recipient: &str,
        _received_at: u64,
    ) -> Result<bool> {
        content.validate_binding(expected)?;
        if !content.body().recipients.iter().any(|r| r == recipient) {
            return Err(Error("not a content recipient"));
        }
        let b = content.body();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let old: Option<(String, bool)> = tx
            .query_row(
                "SELECT envelope,received FROM content_records WHERE job=?1 AND id=?2",
                params![b.job_id, b.message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_error)?;
        if old.as_ref().is_some_and(|v| v.0 != content.envelope()) {
            return Err(Error("conflicting logical content id"));
        }
        if let Some(previous) = &b.supersedes {
            let predecessor: Option<String> = tx
                .query_row(
                    "SELECT envelope FROM content_records WHERE job=?1 AND id=?2 AND received=1",
                    params![b.job_id, previous],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_error)?;
            let predecessor = PreparedContent::decode(
                &predecessor.ok_or(Error("unknown revision predecessor"))?,
            )?;
            let p = predecessor.body();
            if p.author != b.author
                || p.kind != b.kind
                || p.offer_id != b.offer_id
                || p.award_id != b.award_id
                || p.recipients != b.recipients
                || p.revision.checked_add(1) != Some(b.revision)
            {
                return Err(Error("invalid revision chain"));
            }
        }
        let fresh = old.as_ref().is_none_or(|v| !v.1);
        tx.execute("INSERT INTO content_records(job,id,envelope,commitment,author,received) VALUES(?1,?2,?3,?4,?5,1) ON CONFLICT(job,id) DO UPDATE SET received=1",params![b.job_id,b.message_id,content.envelope(),content.commitment(),b.author]).map_err(db_error)?;
        tx.execute(
            "DELETE FROM content_unknown WHERE author=?1 AND job=?2 AND id=?3",
            params![b.author, b.job_id, b.message_id],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(fresh)
    }
    pub fn get(&self, job: &str, id: &str) -> Result<Option<PreparedContent>> {
        let value: Option<String> = self
            .db
            .query_row(
                "SELECT envelope FROM content_records WHERE job=?1 AND id=?2 AND received=1",
                params![job, id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        value.map(|v| PreparedContent::decode(&v)).transpose()
    }
    /// Buffer only AFTER transport::unwrap_content verifies author and recipient. This is
    /// not the verified inbox: get() never exposes these rows. The authenticated inner
    /// author, not the ephemeral outer wrapper key, owns the per-sender quota.
    pub fn stage(&mut self, content: &PreparedContent, recipient: &str, now: u64) -> Result<bool> {
        if !content.body().recipients.iter().any(|r| r == recipient) {
            return Err(Error("not a content recipient"));
        }
        let b = content.body();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        tx.execute(
            "DELETE FROM content_unknown WHERE expires_at <= ?1",
            [now.min(i64::MAX as u64)],
        )
        .map_err(db_error)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT envelope FROM content_unknown WHERE author=?1 AND job=?2 AND id=?3",
                params![b.author, b.job_id, b.message_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if let Some(old) = old {
            if old != content.envelope() {
                return Err(Error("conflicting pending logical content id"));
            }
            tx.commit().map_err(db_error)?;
            return Ok(false);
        }
        let known: Option<String> = tx.query_row("SELECT envelope FROM content_records WHERE author=?1 AND job=?2 AND id=?3 AND received=1",params![b.author,b.job_id,b.message_id],|r|r.get(0)).optional().map_err(db_error)?;
        if let Some(known) = known {
            if known != content.envelope() {
                return Err(Error("conflicting logical content id"));
            }
            tx.commit().map_err(db_error)?;
            return Ok(false);
        }
        let (total, sender): (i64, i64) = tx
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(author=?1),0) FROM content_unknown",
                [&b.author],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db_error)?;
        if total >= MAX_PENDING_RECORDS || sender >= MAX_PENDING_PER_AUTHOR {
            return Err(Error("pending content inbox full"));
        }
        tx.execute(
            "INSERT INTO content_unknown VALUES(?1,?2,?3,?4,?5)",
            params![
                b.author,
                b.job_id,
                b.message_id,
                content.envelope(),
                now.saturating_add(UNKNOWN_TTL_SECS).min(i64::MAX as u64)
            ],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(true)
    }
    /// Private staging accessor. Its output still needs wire::bind_content before receive().
    pub fn staged(
        &self,
        author: &str,
        job: &str,
        id: &str,
        now: u64,
    ) -> Result<Option<PreparedContent>> {
        let value: Option<String> = self.db.query_row("SELECT envelope FROM content_unknown WHERE author=?1 AND job=?2 AND id=?3 AND expires_at>?4",params![author,job,id,now.min(i64::MAX as u64)],|r|r.get(0)).optional().map_err(db_error)?;
        value.map(|v| PreparedContent::decode(&v)).transpose()
    }
    /// Advance only after a complete bounded backfill interval reached EOSE. Receiving
    /// one new live event must not jump past older wrappers not yet downloaded.
    /// An incomplete scan resumes without re-applying overlap on every batch.
    /// Otherwise high-rate histories can consume every query budget on the same
    /// overlap forever and never reach newly arrived content.
    pub fn scan_window(&mut self, recipient: &str, now: u64) -> Result<(u64, u64)> {
        super::require_hex(recipient, 32)?;
        let old: Option<(u64, u64)> = self
            .db
            .query_row(
                "SELECT next_start,through FROM content_scan WHERE recipient=?1",
                [recipient],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_error)?;
        if let Some(window) = old {
            return Ok(window);
        }
        let start = self.receive_since(recipient)?;
        self.db
            .execute(
                "INSERT INTO content_scan VALUES(?1,?2,?3)",
                params![recipient, start, now],
            )
            .map_err(db_error)?;
        Ok((start, now))
    }
    pub fn scan_progress(&mut self, recipient: &str, through: u64) -> Result<()> {
        // Called only after a complete EOSE interval has been staged durably.
        self.complete_backfill(recipient, through)?;
        self.db
            .execute(
                "UPDATE content_scan SET next_start=?2 WHERE recipient=?1",
                params![recipient, through.saturating_add(1)],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn scan_finished(&mut self, recipient: &str) -> Result<()> {
        self.db
            .execute(
                "DELETE FROM content_scan WHERE recipient=?1 AND next_start>through",
                [recipient],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn complete_backfill(&mut self, recipient: &str, through: u64) -> Result<()> {
        super::require_hex(recipient, 32)?;
        self.db.execute("INSERT INTO content_cursor VALUES(?1,?2) ON CONFLICT(recipient) DO UPDATE SET received_at=MAX(received_at,excluded.received_at)",params![recipient,through.min(i64::MAX as u64)]).map_err(db_error)?;
        Ok(())
    }
    pub fn receive_since(&self, recipient: &str) -> Result<u64> {
        let time: Option<u64> = self
            .db
            .query_row(
                "SELECT received_at FROM content_cursor WHERE recipient=?1",
                [recipient],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        Ok(super::transport::receive_since(time.unwrap_or(0)))
    }
}
pub struct PendingCopy {
    pub content: PreparedContent,
    pub recipient: String,
}
impl PendingCopy {
    /// Fresh outer event on EVERY attempt; stable logical ID and immutable envelope underneath.
    pub async fn wrap(&self, keys: &Keys) -> Result<Event> {
        if keys.public_key().to_hex() != self.content.body().author {
            return Err(Error("wrong content signer"));
        }
        super::transport::wrap(
            keys,
            PublicKey::from_hex(&self.recipient).map_err(|_| Error("invalid recipient"))?,
            self.content.envelope().into(),
        )
        .await
    }
}

/// All events must be fetched/pinned by their exact signed IDs, not latest-by-author.
pub struct SignedContext<'a> {
    pub carrier: &'a Event,
    pub offer: &'a Event,
    pub award: Option<&'a Event>,
    pub claim: Option<&'a Event>,
    pub result: Option<&'a Event>,
    pub service: &'a str,
    pub host: &'a super::wire::HostPolicy,
}
impl SignedContext<'_> {
    pub fn validate(&self, content: &PreparedContent) -> Result<()> {
        super::wire::bind_content(
            content,
            self.carrier,
            self.offer,
            self.award,
            self.claim,
            self.result,
            self.service,
            self.host,
        )
    }
    fn binding<'a>(&'a self, content: &'a PreparedContent) -> Result<Binding<'a>> {
        // validate() authenticated the complete participant set and exact signed references.
        // Borrow stable body values only AFTER that check. Never expose this as a validator.
        let body = content.body();
        let buyer = self.offer.pubkey.to_hex();
        let seller = if self.carrier.kind.as_u16() == 3401 {
            super::wire::validate_private(self.offer, self.host)?
                .participants
                .into_iter()
                .next()
                .ok_or(Error("missing target"))?
        } else if let Some(claim) = self.claim {
            claim.pubkey.to_hex()
        } else {
            self.carrier.pubkey.to_hex()
        };
        let find = |key: &str| {
            body.recipients
                .iter()
                .find(|p| p.as_str() == key)
                .map(String::as_str)
                .ok_or(Error("missing content participant"))
        };
        Ok(Binding {
            buyer: find(&buyer)?,
            seller: find(&seller)?,
            service: self.service,
            author: &body.author,
            job_id: &body.job_id,
            offer_id: body.offer_id.as_deref(),
            award_id: body.award_id.as_deref(),
            message_id: &body.message_id,
            commitment: content.commitment(),
            kind: body.kind,
        })
    }
}
impl ContentStore {
    /// Independent buyer, seller and service consumers use the same signed validation.
    pub fn receive_signed(
        &mut self,
        content: &PreparedContent,
        context: &SignedContext<'_>,
        recipient: &str,
        now: u64,
    ) -> Result<bool> {
        context.validate(content)?;
        let tags = super::wire::validate_private(context.offer, context.host)?;
        self.reserve_offer(
            &context.offer.pubkey.to_hex(),
            tags.required("job")?,
            &context.offer.id.to_hex(),
        )?;
        self.receive(content, &context.binding(content)?, recipient, now)
    }
}
