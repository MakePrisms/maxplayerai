//! Private job repository bindings and participant permissions. Queries deliberately
//! use the primary pool; neither cached membership nor a replica can grant access.
use crate::{Db, Result};
use buzz_core::CommunityId;
use sqlx::Row;

/// Immutable offer binding with an optional immutable selected-award binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateJobRepo {
    /// Buyer namespace and offer author.
    pub buyer: String,
    /// Random routing ID; distinct from signed offer ID.
    pub job_id: String,
    /// Signed offer identity reserved for this buyer/job key.
    pub offer_id: String,
    /// Target with pre-claim read access, if any.
    pub target: Option<String>,
    /// Selected seller; no writes before a validated award.
    pub seller: Option<String>,
    /// Exact immutable award identity.
    pub award_id: Option<String>,
    /// Trusted service recipient, never taken from an offer tag.
    pub service: String,
    /// Existing terminal lifecycle disables writes, not historical reads.
    pub closed: bool,
    /// Input publication prevents additional buyer uploads.
    pub input_frozen: bool,
}
impl PrivateJobRepo {
    /// Knowing an opaque URL or being a relay member grants no extra read access.
    pub fn can_read(&self, actor: &str) -> bool {
        actor == self.buyer
            || actor == self.service
            || self.target.as_deref() == Some(actor)
            || self.seller.as_deref() == Some(actor)
    }
    /// Roles may only create their own immutable generated refs. Token ref scope
    /// is applied separately, as an additional restriction.
    pub fn can_create_ref(&self, actor: &str, reference: &str, old: &str, new: &str) -> bool {
        if self.closed
            || old != "0000000000000000000000000000000000000000"
            || new.len() != 40
            || new.bytes().all(|b| b == b'0')
            || !new.bytes().all(hex_byte)
        {
            return false;
        }
        let input = reference.strip_prefix("refs/heads/input/");
        let delivery = reference.strip_prefix("refs/heads/delivery/");
        if let Some(id) = input {
            actor == self.buyer
                && !self.input_frozen
                && self.award_id.is_none()
                && self.target.is_some()
                && hex32(id)
        } else if let Some(id) = delivery {
            self.seller.as_deref() == Some(actor) && self.award_id.is_some() && hex32(id)
        } else {
            false
        }
    }
}
fn hex_byte(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}
fn hex32(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(hex_byte)
}
impl Db {
    /// Read authoritative job ACL state. Database errors must deny, not fall back to public.
    pub async fn private_job_repo(
        &self,
        community: CommunityId,
        buyer: &str,
        job: &str,
    ) -> Result<Option<PrivateJobRepo>> {
        let row=sqlx::query("SELECT buyer,job_id,offer_id,target,seller,award_id,service,
            closed OR EXISTS (SELECT 1 FROM events e WHERE e.community_id=j.community_id
                AND e.pubkey=decode(j.buyer,'hex') AND e.kind IN (3400,3406,3407)
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(e.tags) t WHERE t=jsonb_build_array('e',j.offer_id,'','root'))
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(e.tags) t WHERE t=jsonb_build_array('job',j.job_id))
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(e.tags) t WHERE t=jsonb_build_array('v','2'))) AS closed,
            input_frozen OR EXISTS (SELECT 1 FROM events e WHERE e.community_id=j.community_id AND e.id=decode(j.offer_id,'hex')) AS input_frozen
            FROM private_job_repositories j WHERE community_id=$1 AND buyer=$2 AND job_id=$3")
            .bind(community.as_uuid()).bind(buyer).bind(job).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| PrivateJobRepo {
            buyer: r.get("buyer"),
            job_id: r.get("job_id"),
            offer_id: r.get("offer_id"),
            target: r.get("target"),
            seller: r.get("seller"),
            award_id: r.get("award_id"),
            service: r.get("service"),
            closed: r.get("closed"),
            input_frozen: r.get("input_frozen"),
        }))
    }
    /// Atomically reserve the exact offer and bind at most one award. False means
    /// conflict; no winner is selected by arrival time and no old job ID is recycled.
    /// Callers authenticate all signed inputs BEFORE this operation.
    pub async fn ensure_private_job_repo(
        &self,
        community: CommunityId,
        job: &PrivateJobRepo,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO private_job_repositories(community_id,buyer,job_id,offer_id,target,service) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
            .bind(community.as_uuid()).bind(&job.buyer).bind(&job.job_id).bind(&job.offer_id).bind(&job.target).bind(&job.service).execute(&mut *tx).await?;
        let row=sqlx::query("SELECT offer_id,target,service,seller,award_id,closed FROM private_job_repositories WHERE community_id=$1 AND buyer=$2 AND job_id=$3 FOR UPDATE")
            .bind(community.as_uuid()).bind(&job.buyer).bind(&job.job_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Ok(false);
        };
        if row.get::<String, _>("offer_id") != job.offer_id
            || row.get::<Option<String>, _>("target") != job.target
            || row.get::<String, _>("service") != job.service
        {
            return Ok(false);
        }
        let old_award: Option<String> = row.get("award_id");
        let old_seller: Option<String> = row.get("seller");
        if job.award_id.is_some() {
            if old_award.is_some() && (old_award != job.award_id || old_seller != job.seller) {
                return Ok(false);
            }
            if old_award.is_none() {
                if row.get::<bool, _>("closed") {
                    return Ok(false);
                }
                sqlx::query("UPDATE private_job_repositories SET award_id=$4,seller=$5,input_frozen=TRUE WHERE community_id=$1 AND buyer=$2 AND job_id=$3")
                    .bind(community.as_uuid()).bind(&job.buyer).bind(&job.job_id).bind(&job.award_id).bind(&job.seller).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PrivateJobRepo {
        PrivateJobRepo {
            buyer: "11".repeat(32),
            job_id: "22".repeat(32),
            offer_id: "33".repeat(32),
            target: Some("44".repeat(32)),
            seller: None,
            award_id: None,
            service: "55".repeat(32),
            closed: false,
            input_frozen: false,
        }
    }
    #[test]
    fn outsiders_are_denied_and_award_only_grants_selected_delivery_writes() {
        let mut j = fixture();
        let input = format!("refs/heads/input/{}", "66".repeat(32));
        let delivery = format!("refs/heads/delivery/{}", "77".repeat(32));
        let zero = "00".repeat(20);
        let oid = "88".repeat(20);
        let seller = j.target.clone().unwrap();
        assert!(!j.can_read(&"99".repeat(32)));
        assert!(j.can_read(&seller));
        assert!(j.can_read(&j.service));
        assert!(j.can_create_ref(&j.buyer, &input, &zero, &oid));
        assert!(!j.can_create_ref(&seller, &delivery, &zero, &oid));
        j.seller = Some(seller.clone());
        j.award_id = Some("aa".repeat(32));
        j.input_frozen = true;
        assert!(j.can_create_ref(&seller, &delivery, &zero, &oid));
        assert!(!j.can_create_ref(&seller, &input, &zero, &oid));
        assert!(!j.can_create_ref(&j.buyer, &delivery, &zero, &oid));
        assert!(!j.can_create_ref(&seller, &delivery, &oid, &oid));
        assert!(!j.can_create_ref(&seller, &delivery, &oid, &zero));
        assert!(!j.can_create_ref(&j.service, &delivery, &zero, &oid));
        j.closed = true;
        assert!(!j.can_create_ref(&seller, &delivery, &zero, &oid));
        assert!(j.can_read(&seller));
    }
}

#[cfg(test)]
mod postgres_tests {
    use super::*;
    use sqlx::PgPool;
    use uuid::Uuid;
    #[tokio::test]
    #[ignore = "requires disposable Postgres via PRIVATE_JOB_TEST_DATABASE_URL"]
    async fn atomic_award_tenant_isolation_primary_reads_and_publication_freeze() {
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
                .bind(format!("{}.example.invalid", id))
                .execute(&pool)
                .await
                .unwrap();
            communities.push(CommunityId::from_uuid(id));
        }
        let job = PrivateJobRepo {
            buyer: "11".repeat(32),
            job_id: "22".repeat(32),
            offer_id: "33".repeat(32),
            target: None,
            seller: None,
            award_id: None,
            service: "44".repeat(32),
            closed: false,
            input_frozen: false,
        };
        assert!(
            db.ensure_private_job_repo(communities[0], &job)
                .await
                .unwrap()
        );
        assert!(
            db.private_job_repo(communities[1], &job.buyer, &job.job_id)
                .await
                .unwrap()
                .is_none()
        );
        let mut wrong_offer = job.clone();
        wrong_offer.offer_id = "aa".repeat(32);
        assert!(
            !db.ensure_private_job_repo(communities[0], &wrong_offer)
                .await
                .unwrap()
        );
        assert!(
            db.ensure_private_job_repo(communities[1], &wrong_offer)
                .await
                .unwrap()
        );
        let mut a = job.clone();
        a.seller = Some("55".repeat(32));
        a.award_id = Some("66".repeat(32));
        let mut b = job.clone();
        b.seller = Some("77".repeat(32));
        b.award_id = Some("88".repeat(32));
        let (ra, rb) = tokio::join!(
            db.ensure_private_job_repo(communities[0], &a),
            db.ensure_private_job_repo(communities[0], &b)
        );
        assert_ne!(ra.unwrap(), rb.unwrap());
        let selected = db
            .private_job_repo(communities[0], &job.buyer, &job.job_id)
            .await
            .unwrap()
            .unwrap();
        assert!(selected.input_frozen);
        assert!(selected.seller == a.seller || selected.seller == b.seller);
        let winner = if selected.seller == a.seller { &a } else { &b };
        assert!(
            db.ensure_private_job_repo(communities[0], winner)
                .await
                .unwrap()
        );
        // A poisoned/unavailable replica cannot serve job ACL decisions.
        let replica = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost:1/never")
            .unwrap();
        let db = Db::from_pools(pool.clone(), replica);
        assert_eq!(
            db.private_job_repo(communities[0], &job.buyer, &job.job_id)
                .await
                .unwrap()
                .unwrap(),
            selected
        );
        let mut input = job.clone();
        input.job_id = "ab".repeat(32);
        input.offer_id = "cd".repeat(32);
        input.target = Some("ef".repeat(32));
        assert!(
            db.ensure_private_job_repo(communities[0], &input)
                .await
                .unwrap()
        );
        async fn event(
            pool: &PgPool,
            c: CommunityId,
            id: &str,
            buyer: &str,
            kind: i32,
            tags: serde_json::Value,
        ) {
            sqlx::query("INSERT INTO events(community_id,id,pubkey,created_at,kind,tags,content,sig) VALUES($1,decode($2,'hex'),decode($3,'hex'),NOW(),$4,$5,'',decode($6,'hex'))")
                .bind(c.as_uuid()).bind(id).bind(buyer).bind(kind).bind(tags).bind("00".repeat(64)).execute(pool).await.unwrap();
        }
        event(
            &pool,
            communities[0],
            &input.offer_id,
            &input.buyer,
            3401,
            serde_json::json!([]),
        )
        .await;
        assert!(
            db.private_job_repo(communities[0], &input.buyer, &input.job_id)
                .await
                .unwrap()
                .unwrap()
                .input_frozen
        );
        // JSON array containment is order-insensitive; exact tag row equality is required.
        event(
            &pool,
            communities[0],
            &"de".repeat(32),
            &job.buyer,
            3406,
            serde_json::json!([
                ["root", "", job.offer_id, "e"],
                ["job", job.job_id],
                ["v", "2"]
            ]),
        )
        .await;
        assert!(
            !db.private_job_repo(communities[0], &job.buyer, &job.job_id)
                .await
                .unwrap()
                .unwrap()
                .closed
        );
        event(
            &pool,
            communities[0],
            &"df".repeat(32),
            &job.buyer,
            3406,
            serde_json::json!([
                ["e", job.offer_id, "", "root"],
                ["job", job.job_id],
                ["v", "2"]
            ]),
        )
        .await;
        let closed = db
            .private_job_repo(communities[0], &job.buyer, &job.job_id)
            .await
            .unwrap()
            .unwrap();
        assert!(closed.closed);
        assert!(closed.can_read(winner.seller.as_ref().unwrap()));
        assert!(
            !db.private_job_repo(communities[1], &job.buyer, &job.job_id)
                .await
                .unwrap()
                .unwrap()
                .closed
        );
        pool.close().await;
    }
}
