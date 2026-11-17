//! MAXPLAYER: team repository bindings (docs/specs/maxplayer-team.md §3–4). A team repo
//! lives in its owner's opaque 64-hex namespace, like a private job repo, and its
//! member list is the only source of access. Queries use the primary pool: neither a
//! replica nor cached membership can grant access.
use crate::{Db, Result};
use buzz_core::CommunityId;
use sqlx::Row;

/// Most members a team may list, owner included (spec §7).
pub const MAX_TEAM_MEMBERS: usize = 16;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamRepo {
    /// Creator and namespace owner; always a member, never removable.
    pub owner: String,
    /// Random routing ID (64 lowercase hex).
    pub team_id: String,
    /// Sorted, deduplicated member pubkeys, owner included.
    pub members: Vec<String>,
    /// Trusted service reader, copied from relay configuration at creation.
    pub service: String,
    /// Bumped on every member-list change; writers must name the version they read.
    pub version: i64,
}

impl TeamRepo {
    pub fn is_member(&self, actor: &str) -> bool {
        self.members.iter().any(|m| m == actor)
    }
    /// Members and the service read. Knowing the URL or relay membership grants nothing.
    pub fn can_read(&self, actor: &str) -> bool {
        self.is_member(actor) || actor == self.service
    }
    /// Members write. The service never writes.
    pub fn can_write(&self, actor: &str) -> bool {
        self.is_member(actor)
    }
    /// Branches: create or fast-forward. Tags: create only. No deletions, no
    /// non-fast-forward updates, nothing outside `refs/heads/` and `refs/tags/`.
    /// `is_ancestor` is the hook's HMAC-protected `merge-base --is-ancestor old new`.
    pub fn can_update_ref(
        &self,
        actor: &str,
        reference: &str,
        old: &str,
        new: &str,
        is_ancestor: bool,
    ) -> bool {
        if !self.can_write(actor) || !oid(new) || new == ZERO_OID || !oid(old) {
            return false;
        }
        if let Some(branch) = reference.strip_prefix("refs/heads/") {
            !branch.is_empty() && (old == ZERO_OID || is_ancestor)
        } else if let Some(tag) = reference.strip_prefix("refs/tags/") {
            !tag.is_empty() && old == ZERO_OID
        } else {
            false
        }
    }
}

fn oid(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Serializes team creation against private-job provisioning for one namespace key, so
/// the same `(community, owner, id)` can never be both a team and a job repository.
pub(crate) const NAMESPACE_LOCK_SQL: &str =
    "SELECT pg_advisory_xact_lock(hashtextextended($1::text || ':' || $2 || ':' || $3, 0))";

impl Db {
    /// Read authoritative team ACL state. Database errors must deny, not fall back to public.
    pub async fn team_repo(
        &self,
        community: CommunityId,
        owner: &str,
        team_id: &str,
    ) -> Result<Option<TeamRepo>> {
        let row = sqlx::query(
            "SELECT owner,team_id,members,service,version FROM team_repositories
             WHERE community_id=$1 AND owner=$2 AND team_id=$3",
        )
        .bind(community.as_uuid())
        .bind(owner)
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| TeamRepo {
            owner: r.get("owner"),
            team_id: r.get("team_id"),
            members: r.get("members"),
            service: r.get("service"),
            version: r.get("version"),
        }))
    }

    /// Create (`expected_version == 0`) or replace the member list of an existing team
    /// whose stored version equals `expected_version`. `members` must already be
    /// validated, sorted and deduplicated, with the owner included. `None` means
    /// conflict: the team exists (on create), the version moved (on update), or the id
    /// is already a private job repository.
    pub async fn put_team_repo(
        &self,
        community: CommunityId,
        owner: &str,
        team_id: &str,
        members: &[String],
        service: &str,
        expected_version: i64,
    ) -> Result<Option<TeamRepo>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(NAMESPACE_LOCK_SQL)
            .bind(community.as_uuid().to_string())
            .bind(owner)
            .bind(team_id)
            .execute(&mut *tx)
            .await?;
        let row = if expected_version == 0 {
            let job: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM private_job_repositories WHERE community_id=$1 AND buyer=$2 AND job_id=$3",
            )
            .bind(community.as_uuid())
            .bind(owner)
            .bind(team_id)
            .fetch_optional(&mut *tx)
            .await?;
            if job.is_some() {
                return Ok(None);
            }
            sqlx::query(
                "INSERT INTO team_repositories(community_id,owner,team_id,members,service)
                 VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING
                 RETURNING owner,team_id,members,service,version",
            )
            .bind(community.as_uuid())
            .bind(owner)
            .bind(team_id)
            .bind(members)
            .bind(service)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            sqlx::query(
                "UPDATE team_repositories SET members=$4, version=version+1
                 WHERE community_id=$1 AND owner=$2 AND team_id=$3 AND version=$5
                 RETURNING owner,team_id,members,service,version",
            )
            .bind(community.as_uuid())
            .bind(owner)
            .bind(team_id)
            .bind(members)
            .bind(expected_version)
            .fetch_optional(&mut *tx)
            .await?
        };
        let Some(r) = row else {
            return Ok(None);
        };
        tx.commit().await?;
        Ok(Some(TeamRepo {
            owner: r.get("owner"),
            team_id: r.get("team_id"),
            members: r.get("members"),
            service: r.get("service"),
            version: r.get("version"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn team() -> TeamRepo {
        TeamRepo {
            owner: "11".repeat(32),
            team_id: "22".repeat(32),
            members: vec!["11".repeat(32), "33".repeat(32)],
            service: "55".repeat(32),
            version: 1,
        }
    }
    #[test]
    fn members_read_and_write_service_reads_only_outsiders_nothing() {
        let t = team();
        let (member, outsider) = ("33".repeat(32), "99".repeat(32));
        assert!(t.can_read(&t.owner) && t.can_write(&t.owner));
        assert!(t.can_read(&member) && t.can_write(&member));
        assert!(t.can_read(&t.service) && !t.can_write(&t.service));
        assert!(!t.can_read(&outsider) && !t.can_write(&outsider));
    }
    #[test]
    fn branches_create_or_fast_forward_tags_create_nothing_is_deleted() {
        let t = team();
        let m = "33".repeat(32);
        let (a, b) = ("aa".repeat(20), "bb".repeat(20));
        assert!(t.can_update_ref(&m, "refs/heads/main", ZERO_OID, &a, false));
        assert!(t.can_update_ref(&m, "refs/heads/main", &a, &b, true));
        assert!(!t.can_update_ref(&m, "refs/heads/main", &a, &b, false), "non-ff");
        assert!(!t.can_update_ref(&m, "refs/heads/main", &a, ZERO_OID, false), "delete");
        assert!(t.can_update_ref(&m, "refs/tags/v1", ZERO_OID, &a, false));
        assert!(!t.can_update_ref(&m, "refs/tags/v1", &a, &b, true), "tag move");
        assert!(!t.can_update_ref(&m, "refs/notes/x", ZERO_OID, &a, false));
        assert!(!t.can_update_ref(&m, "refs/heads/", ZERO_OID, &a, false));
        assert!(!t.can_update_ref(&t.service, "refs/heads/main", ZERO_OID, &a, false));
        assert!(!t.can_update_ref(&"99".repeat(32), "refs/heads/x", ZERO_OID, &a, false));
        assert!(!t.can_update_ref(&m, "refs/heads/main", ZERO_OID, &a.to_uppercase(), false));
    }
}

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use sqlx::PgPool;
    use uuid::Uuid;
    #[tokio::test]
    #[ignore = "requires disposable Postgres via PRIVATE_JOB_TEST_DATABASE_URL"]
    async fn create_update_versioning_tenant_isolation_and_job_namespace_exclusion() {
        let url = std::env::var("PRIVATE_JOB_TEST_DATABASE_URL")
            .expect("explicit disposable test database required");
        let pool = PgPool::connect(&url).await.unwrap();
        crate::migration::run_migrations(&pool).await.unwrap();
        let db = Db::from_pool(pool.clone());
        let mut communities = vec![];
        for _ in 0..2 {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO communities(id,host) VALUES($1,$2)")
                .bind(id)
                .bind(format!("{id}.example.invalid"))
                .execute(&pool)
                .await
                .unwrap();
            communities.push(CommunityId::from_uuid(id));
        }
        let (owner, team_id, service) = ("a1".repeat(32), "a2".repeat(32), "a5".repeat(32));
        let solo = vec![owner.clone()];
        let created = db
            .put_team_repo(communities[0], &owner, &team_id, &solo, &service, 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created.version, 1);
        // A second create, and an update naming a stale version, both conflict.
        assert!(db
            .put_team_repo(communities[0], &owner, &team_id, &solo, &service, 0)
            .await
            .unwrap()
            .is_none());
        let pair = vec![owner.clone(), "a3".repeat(32)];
        assert!(db
            .put_team_repo(communities[0], &owner, &team_id, &pair, &service, 7)
            .await
            .unwrap()
            .is_none());
        let updated = db
            .put_team_repo(communities[0], &owner, &team_id, &pair, &service, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((updated.version, updated.members.clone()), (2, pair));
        assert_eq!(
            db.team_repo(communities[0], &owner, &team_id).await.unwrap(),
            Some(updated)
        );
        assert!(db
            .team_repo(communities[1], &owner, &team_id)
            .await
            .unwrap()
            .is_none());
        // The table refuses a member list without its owner.
        assert!(db
            .put_team_repo(communities[0], &owner, &team_id, &["a3".repeat(32)], &service, 2)
            .await
            .is_err());
        // A team id can't take over a private job repository, nor the reverse.
        let job = crate::private_jobs::PrivateJobRepo {
            buyer: owner.clone(),
            job_id: "a6".repeat(32),
            offer_id: "a7".repeat(32),
            target: None,
            seller: None,
            award_id: None,
            service: service.clone(),
            closed: false,
            input_frozen: false,
            session: false,
        };
        assert!(db.ensure_private_job_repo(communities[0], &job).await.unwrap());
        assert!(db
            .put_team_repo(communities[0], &owner, &job.job_id, &solo, &service, 0)
            .await
            .unwrap()
            .is_none());
        let mut clash = job.clone();
        clash.job_id = team_id.clone();
        clash.offer_id = "a8".repeat(32);
        assert!(!db.ensure_private_job_repo(communities[0], &clash).await.unwrap());
    }
}
