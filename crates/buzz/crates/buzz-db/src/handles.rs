//! Explicit, tenant-scoped handle ownership with atomic rename and cooldown.
use crate::{Db, Result};
use buzz_core::CommunityId;
use sqlx::{Postgres, Transaction};

/// Normalize the public name rule without accepting Unicode or whitespace aliases.
pub fn normalize_name(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    ((3..=30).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
    .then_some(name)
}

impl Db {
    /// Read availability. Anonymous callers cannot bypass the previous-owner cooldown.
    pub async fn handle_reason(
        &self,
        community: CommunityId,
        name: &str,
        owner: Option<&[u8]>,
    ) -> Result<Option<String>> {
        let mut tx = self.pool().begin().await?;
        let reason = reason(&mut tx, community, name, owner).await?;
        tx.commit().await?;
        Ok(reason)
    }

    /// Claim or rename. Serialize mutations per community, including absent-name races.
    /// Failed renames leave the existing handle untouched. Repeated claims are idempotent.
    pub async fn claim_handle(
        &self,
        community: CommunityId,
        name: &str,
        pubkey: &[u8],
    ) -> Result<Option<String>> {
        let Some(name) = normalize_name(name) else {
            return Ok(Some("invalid".into()));
        };
        let mut tx = self.pool().begin().await?;
        lock(&mut tx, community).await?;
        if let Some(reason) = reason(&mut tx, community, &name, Some(pubkey)).await? {
            return Ok(Some(reason));
        }
        let current: Option<String> = sqlx::query_scalar(
            "SELECT name FROM handles WHERE community_id=$1 AND pubkey=$2 AND status='active'",
        )
        .bind(community.as_uuid())
        .bind(pubkey)
        .fetch_optional(&mut *tx)
        .await?;
        if current.as_deref() != Some(&name) {
            release(&mut tx, community, pubkey).await?;
            sqlx::query(
                "INSERT INTO handles (community_id,name,pubkey,status) VALUES ($1,$2,$3,'active')",
            )
            .bind(community.as_uuid())
            .bind(name)
            .bind(pubkey)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(None)
    }

    /// Release only the authenticated owner's active handle; safe to repeat.
    pub async fn release_handle(&self, community: CommunityId, pubkey: &[u8]) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        lock(&mut tx, community).await?;
        release(&mut tx, community, pubkey).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Public lookup; blocked names never verify, even if an old claim remains.
    pub async fn lookup_handle(
        &self,
        community: CommunityId,
        name: &str,
    ) -> Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar("SELECT h.pubkey FROM handles h WHERE h.community_id=$1 AND h.name=$2 AND h.status='active' AND NOT EXISTS (SELECT 1 FROM reserved_handles r WHERE r.community_id=h.community_id AND r.name=h.name)")
            .bind(community.as_uuid()).bind(name).fetch_optional(self.pool()).await?)
    }

    /// Resolve up to 500 explicit public keys; no registry listing API.
    pub async fn batch_handles(
        &self,
        community: CommunityId,
        pubkeys: &[Vec<u8>],
    ) -> Result<Vec<(Vec<u8>, String)>> {
        if pubkeys.len() > 500 {
            return Err(crate::DbError::InvalidData("at most 500 pubkeys".into()));
        }
        Ok(sqlx::query_as("SELECT h.pubkey,h.name FROM handles h WHERE h.community_id=$1 AND h.pubkey=ANY($2) AND h.status='active' AND NOT EXISTS (SELECT 1 FROM reserved_handles r WHERE r.community_id=h.community_id AND r.name=h.name)")
            .bind(community.as_uuid()).bind(pubkeys).fetch_all(self.pool()).await?)
    }
}

async fn lock(tx: &mut Transaction<'_, Postgres>, community: CommunityId) -> Result<()> {
    // Match the deletion executor's ordering: advisory fence before community row.
    // The shared fence remains held until commit, so deletion cannot race the rename.
    sqlx::query("SELECT assert_community_write_allowed($1)")
        .bind(community.as_uuid())
        .execute(&mut **tx)
        .await?;
    // Use the existing community row, not a hash lock with collision/ordering hazards.
    sqlx::query("SELECT id FROM communities WHERE id=$1 FOR UPDATE")
        .bind(community.as_uuid())
        .fetch_one(&mut **tx)
        .await?;
    Ok(())
}
async fn release(
    tx: &mut Transaction<'_, Postgres>,
    community: CommunityId,
    pubkey: &[u8],
) -> Result<()> {
    sqlx::query("UPDATE handles SET status='released',released_at=clock_timestamp() WHERE community_id=$1 AND pubkey=$2 AND status='active'")
        .bind(community.as_uuid()).bind(pubkey).execute(&mut **tx).await?;
    Ok(())
}
async fn reason(
    tx: &mut Transaction<'_, Postgres>,
    community: CommunityId,
    name: &str,
    owner: Option<&[u8]>,
) -> Result<Option<String>> {
    if normalize_name(name).as_deref() != Some(name) {
        return Ok(Some("invalid".into()));
    }
    Ok(sqlx::query_scalar::<_, Option<String>>("SELECT CASE
        WHEN EXISTS (SELECT 1 FROM reserved_handles WHERE community_id=$1 AND name=$2) THEN 'reserved'
        WHEN EXISTS (SELECT 1 FROM handles WHERE community_id=$1 AND name=$2 AND status='active' AND ($3::bytea IS NULL OR pubkey<>$3)) THEN 'taken'
        WHEN EXISTS (SELECT 1 FROM handles WHERE community_id=$1 AND name=$2 AND status='released' AND released_at>clock_timestamp()-INTERVAL '30 days' AND ($3::bytea IS NULL OR pubkey<>$3)) THEN 'cooldown'
        ELSE NULL END")
        .bind(community.as_uuid()).bind(name).bind(owner).fetch_one(&mut **tx).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handle_names() {
        assert_eq!(normalize_name("Alice._-9").as_deref(), Some("alice._-9"));
        for name in [
            "", "ab", "_alice", "a+b", " alice", "alice ", "Älice", "a@b", "a/b",
        ] {
            assert!(normalize_name(name).is_none(), "{name}");
        }
        assert!(normalize_name(&"a".repeat(30)).is_some());
        assert!(normalize_name(&"a".repeat(31)).is_none());
    }
}
