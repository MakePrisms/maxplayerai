//! Every SQL statement the service runs. All timestamps are passed in from the
//! service clock (never `now()` in SQL) so expiry and rate windows are testable.

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{PgConnection, PgPool, Row};

use crate::relay::MemberAction;
use crate::validate;

// ---------------------------------------------------------------- replay + rate limits

/// Record a NIP-98 event id. `false` = already used (a replay).
pub async fn mark_nip98_seen(db: &PgPool, event_id: &str, now: DateTime<Utc>) -> Result<bool> {
    let r = sqlx::query(
        "INSERT INTO nip98_seen (event_id, seen_at) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(event_id)
    .bind(now)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn count_rate(
    db: &PgPool,
    bucket: &str,
    action: &str,
    since: DateTime<Utc>,
) -> Result<i64> {
    let row = sqlx::query(
        "SELECT count(*) AS n FROM rate_events WHERE bucket = $1 AND action = $2 AND created_at > $3",
    )
    .bind(bucket)
    .bind(action)
    .bind(since)
    .fetch_one(db)
    .await?;
    Ok(row.try_get("n")?)
}

pub async fn record_rate(
    db: &PgPool,
    buckets: &[String],
    action: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    for bucket in buckets {
        sqlx::query("INSERT INTO rate_events (bucket, action, created_at) VALUES ($1, $2, $3)")
            .bind(bucket)
            .bind(action)
            .bind(now)
            .execute(db)
            .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------- registrations

#[derive(Debug, Clone)]
pub struct Registration {
    pub pubkey: String,
    pub email: String,
    pub label: String,
    pub terms_version: String,
    pub privacy_version: String,
    pub accepted_at: DateTime<Utc>,
    pub code_entered_by: String,
    pub created_at: DateTime<Utc>,
}

const REG_COLUMNS: &str = "pubkey, email, label, terms_version, privacy_version, accepted_at, code_entered_by, created_at";

fn reg_from_row(row: &sqlx::postgres::PgRow) -> Result<Registration> {
    Ok(Registration {
        pubkey: row.try_get("pubkey")?,
        email: row.try_get("email")?,
        label: row.try_get("label")?,
        terms_version: row.try_get("terms_version")?,
        privacy_version: row.try_get("privacy_version")?,
        accepted_at: row.try_get("accepted_at")?,
        code_entered_by: row.try_get("code_entered_by")?,
        created_at: row.try_get("created_at")?,
    })
}

pub async fn get_registration(db: &PgPool, pubkey: &str) -> Result<Option<Registration>> {
    let sql = format!("SELECT {REG_COLUMNS} FROM registrations WHERE pubkey = $1");
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(pubkey)
        .fetch_optional(db)
        .await?;
    row.as_ref().map(reg_from_row).transpose()
}

pub async fn registered_pubkeys(db: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT pubkey FROM registrations")
        .fetch_all(db)
        .await?;
    rows.iter().map(|r| Ok(r.try_get("pubkey")?)).collect()
}

/// Operator removal: delete the row and queue the NIP-43 remove in one transaction.
/// Returns (row existed, outbox id).
pub async fn remove_registration(
    db: &PgPool,
    pubkey: &str,
    now: DateTime<Utc>,
) -> Result<(bool, i64)> {
    let mut tx = db.begin().await?;
    let deleted = sqlx::query("DELETE FROM registrations WHERE pubkey = $1")
        .bind(pubkey)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    let id = enqueue(&mut tx, pubkey, MemberAction::Remove, now).await?;
    tx.commit().await?;
    Ok((deleted, id))
}

// ---------------------------------------------------------------- codes

pub struct NewCode<'a> {
    pub email: &'a str,
    pub code_hash: &'a [u8],
    pub pubkey: &'a str,
    pub label: &'a str,
    pub terms_version: &'a str,
    pub privacy_version: &'a str,
    pub notice_version: &'a str,
    pub expires_at: DateTime<Utc>,
}

pub async fn last_code_created(db: &PgPool, pubkey: &str) -> Result<Option<DateTime<Utc>>> {
    let row = sqlx::query("SELECT max(created_at) AS t FROM email_codes WHERE pubkey = $1")
        .bind(pubkey)
        .fetch_one(db)
        .await?;
    Ok(row.try_get("t")?)
}

/// Revoke every live code for this key and store the new one (hash only).
pub async fn issue_code(db: &PgPool, c: &NewCode<'_>, now: DateTime<Utc>) -> Result<i64> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE email_codes SET revoked_at = $2 WHERE pubkey = $1 AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(c.pubkey)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query(
        "INSERT INTO email_codes (email, code_hash, purpose, pubkey, label, terms_version, privacy_version, \
         notice_version, attempts, expires_at, created_at) \
         VALUES ($1, $2, 'link', $3, $4, $5, $6, $7, 0, $8, $9) RETURNING id",
    )
    .bind(c.email)
    .bind(c.code_hash)
    .bind(c.pubkey)
    .bind(c.label)
    .bind(c.terms_version)
    .bind(c.privacy_version)
    .bind(c.notice_version)
    .bind(c.expires_at)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.try_get("id")?)
}

pub async fn revoke_code(db: &PgPool, id: i64, now: DateTime<Utc>) -> Result<()> {
    sqlx::query("UPDATE email_codes SET revoked_at = $2 WHERE id = $1 AND used_at IS NULL")
        .bind(id)
        .bind(now)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug)]
pub enum ConfirmOutcome {
    NoPendingCode,
    Expired,
    Exhausted,
    Wrong {
        attempts_left: i32,
    },
    /// The key is linked to a different email (only reachable through a race with
    /// the request-time check).
    ConflictingEmail,
    Linked {
        registration: Registration,
        inserted: bool,
        notice_version: String,
        outbox_id: i64,
    },
}

/// Check a code against the key's newest live code and, on a match, link the key:
/// registration row (insert, or idempotent update for the same email), marketing
/// consent row (automatic opt-in, only if this email has none), and a pending NIP-43
/// add, all in one transaction.
pub async fn confirm_code(
    db: &PgPool,
    pepper: &[u8; 32],
    pubkey: &str,
    code: Option<&str>,
    entered_by: &str,
    max_attempts: i32,
    now: DateTime<Utc>,
) -> Result<ConfirmOutcome> {
    let mut tx = db.begin().await?;
    let row = sqlx::query(
        "SELECT id, email, code_hash, label, terms_version, privacy_version, notice_version, attempts, expires_at \
         FROM email_codes WHERE pubkey = $1 AND used_at IS NULL AND revoked_at IS NULL \
         ORDER BY created_at DESC, id DESC LIMIT 1 FOR UPDATE",
    )
    .bind(pubkey)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(ConfirmOutcome::NoPendingCode);
    };
    let id: i64 = row.try_get("id")?;
    let expires_at: DateTime<Utc> = row.try_get("expires_at")?;
    let attempts: i32 = row.try_get("attempts")?;
    if expires_at <= now {
        return Ok(ConfirmOutcome::Expired);
    }
    if attempts >= max_attempts {
        return Ok(ConfirmOutcome::Exhausted);
    }
    let stored: Vec<u8> = row.try_get("code_hash")?;
    let matches = code.is_some_and(|c| validate::code_matches(pepper, pubkey, c, &stored));
    if !matches {
        let row = sqlx::query(
            "UPDATE email_codes SET attempts = attempts + 1 WHERE id = $1 RETURNING attempts",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        let used: i32 = row.try_get("attempts")?;
        let left = max_attempts - used;
        return Ok(if left <= 0 {
            ConfirmOutcome::Exhausted
        } else {
            ConfirmOutcome::Wrong {
                attempts_left: left,
            }
        });
    }

    let email: String = row.try_get("email")?;
    let label: String = row.try_get("label")?;
    let terms_version: String = row.try_get("terms_version")?;
    let privacy_version: String = row.try_get("privacy_version")?;
    let notice_version: String = row.try_get("notice_version")?;

    sqlx::query("UPDATE email_codes SET used_at = $2 WHERE id = $1")
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?;

    let sql = format!(
        "INSERT INTO registrations (pubkey, email, label, terms_version, privacy_version, accepted_at, \
         code_entered_by, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $6, $6) \
         ON CONFLICT (pubkey) DO UPDATE SET label = EXCLUDED.label, terms_version = EXCLUDED.terms_version, \
         privacy_version = EXCLUDED.privacy_version, accepted_at = EXCLUDED.accepted_at, \
         code_entered_by = EXCLUDED.code_entered_by, updated_at = EXCLUDED.updated_at \
         WHERE registrations.email = EXCLUDED.email \
         RETURNING {REG_COLUMNS}, (xmax = 0) AS inserted"
    );
    let reg_row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(pubkey)
        .bind(&email)
        .bind(&label)
        .bind(&terms_version)
        .bind(&privacy_version)
        .bind(now)
        .bind(entered_by)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(reg_row) = reg_row else {
        // Rolls back: the code stays unused.
        return Ok(ConfirmOutcome::ConflictingEmail);
    };
    let registration = reg_from_row(&reg_row)?;
    let inserted: bool = reg_row.try_get("inserted")?;

    sqlx::query(
        "INSERT INTO marketing_consents (email, opted_in_at, method, notice_version, updated_at) \
         VALUES ($1, $2, 'signup_default', $3, $2) ON CONFLICT (email) DO NOTHING",
    )
    .bind(&email)
    .bind(now)
    .bind(&notice_version)
    .execute(&mut *tx)
    .await?;

    let outbox_id = enqueue(&mut tx, pubkey, MemberAction::Add, now).await?;
    tx.commit().await?;
    Ok(ConfirmOutcome::Linked {
        registration,
        inserted,
        notice_version,
        outbox_id,
    })
}

// ---------------------------------------------------------------- member sync outbox

#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub id: i64,
    pub pubkey: String,
    pub action: MemberAction,
    pub attempts: i32,
}

/// Queue a NIP-43 command. A pending command of the opposite action for the same
/// key is superseded; a pending one of the same action is reused.
pub async fn enqueue(
    conn: &mut PgConnection,
    pubkey: &str,
    action: MemberAction,
    now: DateTime<Utc>,
) -> Result<i64> {
    sqlx::query(
        "UPDATE member_sync_outbox SET status = 'superseded', updated_at = $3 \
         WHERE pubkey = $1 AND status = 'pending' AND action <> $2",
    )
    .bind(pubkey)
    .bind(action.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let row = sqlx::query(
        "INSERT INTO member_sync_outbox (pubkey, action, status, next_attempt_at, created_at, updated_at) \
         VALUES ($1, $2, 'pending', $3, $3, $3) \
         ON CONFLICT (pubkey, action) WHERE status = 'pending' \
         DO UPDATE SET next_attempt_at = LEAST(member_sync_outbox.next_attempt_at, EXCLUDED.next_attempt_at), \
         updated_at = EXCLUDED.updated_at RETURNING id",
    )
    .bind(pubkey)
    .bind(action.as_str())
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    Ok(row.try_get("id")?)
}

pub async fn enqueue_pool(
    db: &PgPool,
    pubkey: &str,
    action: MemberAction,
    now: DateTime<Utc>,
) -> Result<i64> {
    let mut conn = db.acquire().await?;
    enqueue(&mut conn, pubkey, action, now).await
}

const LEASE_SECS: i64 = 60;

fn outbox_from_row(row: &sqlx::postgres::PgRow) -> Result<OutboxItem> {
    let action: String = row.try_get("action")?;
    Ok(OutboxItem {
        id: row.try_get("id")?,
        pubkey: row.try_get("pubkey")?,
        action: MemberAction::parse(&action).ok_or_else(|| anyhow::anyhow!("bad outbox action"))?,
        attempts: row.try_get("attempts")?,
    })
}

/// Lease up to `limit` due commands.
pub async fn claim_due(db: &PgPool, now: DateTime<Utc>, limit: i64) -> Result<Vec<OutboxItem>> {
    let rows = sqlx::query(
        "UPDATE member_sync_outbox SET lease_until = $2, updated_at = $1 WHERE id IN ( \
           SELECT id FROM member_sync_outbox WHERE status = 'pending' AND next_attempt_at <= $1 \
           AND (lease_until IS NULL OR lease_until < $1) ORDER BY id LIMIT $3 FOR UPDATE SKIP LOCKED) \
         RETURNING id, pubkey, action, attempts",
    )
    .bind(now)
    .bind(now + TimeDelta::seconds(LEASE_SECS))
    .bind(limit)
    .fetch_all(db)
    .await?;
    let mut items: Vec<OutboxItem> = rows.iter().map(outbox_from_row).collect::<Result<_>>()?;
    items.sort_by_key(|i| i.id);
    Ok(items)
}

/// Lease one specific command now, ignoring its backoff (used right after confirm
/// and by the operator `remove` command).
pub async fn claim_one(db: &PgPool, id: i64, now: DateTime<Utc>) -> Result<Option<OutboxItem>> {
    let row = sqlx::query(
        "UPDATE member_sync_outbox SET lease_until = $2, updated_at = $1 \
         WHERE id = $3 AND status = 'pending' AND (lease_until IS NULL OR lease_until < $1) \
         RETURNING id, pubkey, action, attempts",
    )
    .bind(now)
    .bind(now + TimeDelta::seconds(LEASE_SECS))
    .bind(id)
    .fetch_optional(db)
    .await?;
    row.as_ref().map(outbox_from_row).transpose()
}

pub async fn mark_outbox(db: &PgPool, id: i64, status: &str, now: DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "UPDATE member_sync_outbox SET status = $2, attempts = attempts + 1, lease_until = NULL, \
         last_error = NULL, done_at = $3, updated_at = $3 WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(status)
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn mark_outbox_failed(
    db: &PgPool,
    id: i64,
    error: &str,
    next_attempt_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE member_sync_outbox SET attempts = attempts + 1, lease_until = NULL, last_error = $2, \
         next_attempt_at = $3, updated_at = $4 WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .bind(error)
    .bind(next_attempt_at)
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn outbox_status(db: &PgPool, id: i64) -> Result<Option<String>> {
    let row = sqlx::query("SELECT status FROM member_sync_outbox WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?;
    row.map(|r| Ok(r.try_get("status")?)).transpose()
}

/// `pending` when an add for this key still waits, `done` once one was confirmed,
/// else `none`.
pub async fn member_sync_state(db: &PgPool, pubkey: &str) -> Result<&'static str> {
    let row = sqlx::query(
        "SELECT bool_or(status = 'pending') AS pending, bool_or(status = 'done') AS done \
         FROM member_sync_outbox WHERE pubkey = $1 AND action = 'add'",
    )
    .bind(pubkey)
    .fetch_one(db)
    .await?;
    let pending: Option<bool> = row.try_get("pending")?;
    let done: Option<bool> = row.try_get("done")?;
    Ok(match (pending, done) {
        (Some(true), _) => "pending",
        (_, Some(true)) => "done",
        _ => "none",
    })
}

// ---------------------------------------------------------------- marketing

#[derive(Debug, Clone)]
pub struct Consent {
    pub method: String,
    pub notice_version: String,
    pub unsubscribed_at: Option<DateTime<Utc>>,
    pub unsubscribe_method: Option<String>,
}

pub async fn get_consent(db: &PgPool, email: &str) -> Result<Option<Consent>> {
    let row = sqlx::query(
        "SELECT method, notice_version, unsubscribed_at, unsubscribe_method FROM marketing_consents WHERE email = $1",
    )
    .bind(email)
    .fetch_optional(db)
    .await?;
    row.map(|r| {
        Ok(Consent {
            method: r.try_get("method")?,
            notice_version: r.try_get("notice_version")?,
            unsubscribed_at: r.try_get("unsubscribed_at")?,
            unsubscribe_method: r.try_get("unsubscribe_method")?,
        })
    })
    .transpose()
}

/// Turn product news on (method `cli`, new opt-in time and notice version).
pub async fn marketing_on(
    db: &PgPool,
    email: &str,
    notice_version: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO marketing_consents (email, opted_in_at, method, notice_version, updated_at) \
         VALUES ($1, $2, 'cli', $3, $2) ON CONFLICT (email) DO UPDATE SET opted_in_at = EXCLUDED.opted_in_at, \
         method = 'cli', notice_version = EXCLUDED.notice_version, unsubscribed_at = NULL, \
         unsubscribe_method = NULL, updated_at = EXCLUDED.updated_at",
    )
    .bind(email)
    .bind(now)
    .bind(notice_version)
    .execute(db)
    .await?;
    Ok(())
}

/// Record an unsubscribe. Keeps the first unsubscribe time if already off. A row is
/// created if none exists, so an unsubscribe is never lost.
pub async fn marketing_off(
    db: &PgPool,
    email: &str,
    method: &str,
    default_notice: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO marketing_consents (email, opted_in_at, method, notice_version, unsubscribed_at, \
         unsubscribe_method, updated_at) VALUES ($1, $2, 'signup_default', $4, $2, $3, $2) \
         ON CONFLICT (email) DO UPDATE SET \
         unsubscribed_at = COALESCE(marketing_consents.unsubscribed_at, EXCLUDED.unsubscribed_at), \
         unsubscribe_method = COALESCE(marketing_consents.unsubscribe_method, EXCLUDED.unsubscribe_method), \
         updated_at = EXCLUDED.updated_at",
    )
    .bind(email)
    .bind(now)
    .bind(method)
    .bind(default_notice)
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- webhooks

/// `false` = this webhook message id was already handled.
pub async fn mark_webhook_seen(
    db: &PgPool,
    webhook_id: &str,
    event_type: &str,
    now: DateTime<Utc>,
) -> Result<bool> {
    let r = sqlx::query(
        "INSERT INTO webhook_events (webhook_id, event_type, received_at) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(webhook_id)
    .bind(event_type)
    .bind(now)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn record_bounce(
    db: &PgPool,
    email: &str,
    bounced_at: DateTime<Utc>,
    event_type: &str,
    webhook_id: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO email_bounces (email, bounced_at, event_type, webhook_id, created_at) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (webhook_id, email) DO NOTHING",
    )
    .bind(email)
    .bind(bounced_at)
    .bind(event_type)
    .bind(webhook_id)
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- cleanup

/// The "daily job" of spec §5: drop codes, counters and replay records nobody needs.
pub async fn cleanup(db: &PgPool, now: DateTime<Utc>) -> Result<u64> {
    let mut n = 0;
    for (sql, age) in [
        (
            "DELETE FROM email_codes WHERE created_at < $1",
            TimeDelta::days(1),
        ),
        (
            "DELETE FROM rate_events WHERE created_at < $1",
            TimeDelta::days(1),
        ),
        (
            "DELETE FROM nip98_seen WHERE seen_at < $1",
            TimeDelta::hours(1),
        ),
        (
            "DELETE FROM webhook_events WHERE received_at < $1",
            TimeDelta::days(30),
        ),
        (
            "DELETE FROM member_sync_outbox WHERE status <> 'pending' AND updated_at < $1",
            TimeDelta::days(30),
        ),
    ] {
        n += sqlx::query(sql)
            .bind(now - age)
            .execute(db)
            .await?
            .rows_affected();
    }
    Ok(n)
}

// ---------------------------------------------------------------- managed members

pub async fn add_managed(db: &PgPool, pubkey: &str, now: DateTime<Utc>) -> Result<()> {
    sqlx::query("INSERT INTO managed_members (pubkey, first_added_at) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(pubkey)
        .bind(now)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn remove_managed(db: &PgPool, pubkey: &str) -> Result<()> {
    sqlx::query("DELETE FROM managed_members WHERE pubkey = $1")
        .bind(pubkey)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn managed_pubkeys(db: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT pubkey FROM managed_members")
        .fetch_all(db)
        .await?;
    rows.iter().map(|r| Ok(r.try_get("pubkey")?)).collect()
}
