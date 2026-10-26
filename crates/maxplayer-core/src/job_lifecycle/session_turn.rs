//! Session jobs (docs/specs/session-jobs.md §2.4, §8.2, §8.4): accept ONE turn ≥ 2 result.
//!
//! A turn's bind is a normal [`AcceptedBind`] with `followup_id` set, stored at
//! `jobs/<followup_id>.json`; turn 1 keeps the job's own bind and is never touched here. Every
//! accept-time gate of [`accept_claim_async`] runs again per turn — seller co-signature, the
//! offer's job-hash, the inline/contribution rules, the free/priced agreement — plus the turn's
//! own payment request: `payment_id = followup_id`, amount = the offer's, mints ⊆ the claim's.
use super::*;

/// The accept-bind of turn `followup_id`, accepting that turn's delivered result (and publishing
/// its ACCEPT with `["e", result, "", "reply"]`) when no bind exists yet. Turn 1 must already be
/// accepted: its bind names the awarded claim, seller and sealed funding mint every turn reuses.
pub async fn accept_turn_async(
    home: &MaxplayerHome,
    job_id: &str,
    followup_id: &str,
) -> Result<AcceptedBind, JobLifecycleError> {
    let keys = buyer_keys(home)?;
    let first = load_accepted_bind(home, job_id)?.ok_or_else(|| {
        JobLifecycleError::Input(format!("collect turn 1 of {job_id} before a later turn"))
    })?;
    if first.followup_id.is_some() || followup_id == job_id {
        return Err(JobLifecycleError::Input(
            "turn bind names the job itself".into(),
        ));
    }
    if let Some(bind) = load_accepted_bind(home, followup_id)? {
        return finish_existing(home, &keys, bind, job_id, followup_id).await;
    }
    let view = fetch_job_view_async(
        home,
        &keys,
        job_id,
        Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS),
        now_unix(),
    )
    .await?;
    let offer = view
        .offer
        .as_ref()
        .ok_or_else(|| JobLifecycleError::NotFound(format!("offer {job_id}")))?;
    let claim = view
        .claims
        .iter()
        .find(|claim| claim.claim_id == first.claim_id)
        .ok_or_else(|| JobLifecycleError::NotFound(format!("claim {}", first.claim_id)))?;
    if !claim.session || claim.seller_pubkey != first.seller_pubkey {
        return Err(JobLifecycleError::Input(
            "the awarded claim did not advertise sessions; a turn cannot be accepted".into(),
        ));
    }
    let result = view
        .results
        .iter()
        .find(|r| {
            r.seller_pubkey == first.seller_pubkey
                && r.session.reply.as_deref() == Some(followup_id)
                && r.has_delivery()
        })
        .ok_or_else(|| {
            JobLifecycleError::NotFound(format!("no delivered result for follow-up {followup_id}"))
        })?;

    let review_subject = crate::review::Subject {
        offer: job_id.to_owned(),
        event: result.result_id.clone(),
        kind: crate::kinds::JOB_RESULT_KIND,
        commit: result.commit_oid.clone(),
    };
    let review_id = crate::review::check_buyer(
        home,
        &keys,
        &review_subject,
        &first.seller_pubkey,
        result.private_evidence.as_ref(),
    )
    .await
    .map_err(JobLifecycleError::Input)?;
    let private_verified = result
        .private_evidence
        .as_ref()
        .map(|evidence| {
            let policy = crate::private_content::runtime::Policy::for_evidence(home, evidence)
                .map_err(|e| JobLifecycleError::Input(e.to_string()))?;
            if evidence.offer.id.to_hex() != job_id
                || evidence.claim.id.to_hex() != first.claim_id
                || evidence.result.id.to_hex() != result.result_id
            {
                return Err(JobLifecycleError::Input(
                    "private turn selection mismatch".into(),
                ));
            }
            evidence
                .validate(&keys.public_key().to_hex(), &policy)
                .map_err(|e| JobLifecycleError::Input(e.to_string()))
        })
        .transpose()?;

    // One accept per follow-up at a time; the loser re-reads the winner's bind.
    let _turn_lock = acquire_job_lock(home, followup_id)?;
    if let Some(bind) = load_accepted_bind(home, followup_id)? {
        return finish_existing(home, &keys, bind, job_id, followup_id).await;
    }
    if let Some(id) = review_id {
        crate::review::record_pass(home, &review_subject, &id).map_err(JobLifecycleError::Input)?;
    }
    let seller_signature = require_seller_signature(&result.seller_signature)?;
    let contribution_offer = offer.contribution.is_some()
        || offer.job_class.as_deref() == Some(crate::contribution::JOB_CLASS_CONTRIBUTION);
    if result.inline_answer.is_some() && (!offer_accepts_inline(offer) || contribution_offer) {
        return Err(JobLifecycleError::Input(
            "this turn is an inline delivery the offer never allowed — refused".into(),
        ));
    }
    let (repo, branch, commit_oid, inline_answer, delivery_kind) =
        match result.inline_answer.clone() {
            Some(answer) => (
                String::new(),
                String::new(),
                private_verified
                    .as_ref()
                    .map(|v| v.integrity.clone())
                    .unwrap_or_else(|| crate::receipt::result_content_hash_hex(&answer)),
                Some(answer),
                Some(crate::receipt::DeliveryKind::Inline.as_str().to_owned()),
            ),
            None => (
                result
                    .repo
                    .clone()
                    .ok_or_else(|| JobLifecycleError::Input("result missing repo".into()))?,
                result
                    .branch
                    .clone()
                    .ok_or_else(|| JobLifecycleError::Input("result missing branch".into()))?,
                result
                    .commit_oid
                    .clone()
                    .ok_or_else(|| JobLifecycleError::Input("result missing commit_oid".into()))?,
                None,
                None,
            ),
        };
    // Every turn binds the OFFER's job-hash (§7.1 as amended), exactly like turn 1.
    let job_hash = private_verified
        .as_ref()
        .map(|v| v.preimage.job_hash.clone())
        .unwrap_or_else(|| job_hash_for_offer(job_id, &offer.task, offer.amount_sats));
    if result.job_hash.as_deref() != Some(job_hash.as_str()) {
        return Err(JobLifecycleError::Input(format!(
            "turn result job-hash {:?} does not match the offer's {job_hash} — refused",
            result.job_hash
        )));
    }
    let contribution = resolve_accepted_contribution(offer, result, &commit_oid)?;
    let payment_mode = verify_free_accept(offer, claim)?;
    if payment_mode != first.payment_mode {
        return Err(JobLifecycleError::Input(
            "turn payment mode differs from turn 1".into(),
        ));
    }
    let (amount_sats, creq_hash, accepted_mints, funding_mint, delivery_mint) = if payment_mode
        .is_free()
    {
        if result.session.creq.is_some() {
            return Err(JobLifecycleError::Input(
                "a free session turn carries a payment request — refused".into(),
            ));
        }
        (0, None, Vec::new(), None, None)
    } else {
        let creq = result.session.creq.as_deref();
        let mints = turn_creq_mints(creq, followup_id, offer.amount_sats, &first.accepted_mints)?;
        let (pin, balances) = accept_pin_and_balances(
            &home.root.join(crate::buyer::STATE_DB_FILE),
            followup_id,
            || async {
                let balances = crate::wallet_ops::balances_async(home)
                    .await
                    .map_err(|error| error.to_string())?;
                crate::buyer::store::available_balances(
                    &home.root.join(crate::buyer::STATE_DB_FILE),
                    followup_id,
                    home.config.default_mint(),
                    &balances,
                )
                .map_err(|error| error.to_string())
            },
        )
        .await?;
        let seed = accept_source_seed(
            home.config.default_mint(),
            &mints,
            home.config.allow_real_mints,
            &pin,
            balances
                .as_ref()
                .map(|read| read.as_ref().map(Vec::as_slice).map_err(String::as_str)),
            offer.amount_sats,
        );
        let plan = crate::crossmint::plan_payment(&seed, &mints, home.config.allow_real_mints)
            .map_err(|error| JobLifecycleError::Input(error.to_string()))?;
        let (funding, delivery) = seal_bind_mints(&plan);
        (
            offer.amount_sats,
            creq.map(crate::gateway::creq_hash_hex),
            mints,
            Some(funding),
            Some(delivery),
        )
    };
    let mut bind = AcceptedBind {
        private_evidence: result.private_evidence.clone(),
        job_id: job_id.to_owned(),
        claim_id: first.claim_id.clone(),
        result_id: result.result_id.clone(),
        seller_pubkey: first.seller_pubkey.clone(),
        commit_oid,
        repo,
        branch,
        delivery_kind,
        inline_answer,
        job_hash,
        amount_sats,
        accept_event_id: String::new(),
        accepted_at: 0,
        seller_signature,
        creq_hash,
        accepted_mints,
        funding_mint,
        delivery_mint,
        agent_used: result.harness.clone(),
        model_used: result.model.clone(),
        contribution,
        payment_mode,
        followup_id: Some(followup_id.to_owned()),
    };
    // Same durability order as turn 1: bind, publish, finalize.
    validate_private_bind(home, &bind)?;
    write_accepted_bind(home, &bind)?;
    bind.accept_event_id = publish_turn_accept(home, &keys, &bind).await?;
    bind.accepted_at = now_unix();
    write_accepted_bind(home, &bind)?;
    Ok(bind)
}

async fn finish_existing(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    mut bind: AcceptedBind,
    job_id: &str,
    followup_id: &str,
) -> Result<AcceptedBind, JobLifecycleError> {
    if bind.job_id != job_id || bind.followup_id.as_deref() != Some(followup_id) {
        return Err(JobLifecycleError::Input(
            "turn bind names another job or turn".into(),
        ));
    }
    validate_private_bind(home, &bind)?;
    if bind.accept_event_id.is_empty() {
        bind.accept_event_id = publish_turn_accept(home, keys, &bind).await?;
        bind.accepted_at = now_unix();
        write_accepted_bind(home, &bind)?;
    }
    Ok(bind)
}

/// The turn's creq (§8.2): payment id = the follow-up id, amount = the offer's, unit sat, and only
/// mints the claim already quoted. Returns the turn's accepted mints.
pub(crate) fn turn_creq_mints(
    creq: Option<&str>,
    followup_id: &str,
    amount_sats: u64,
    claim_mints: &[String],
) -> Result<Vec<String>, JobLifecycleError> {
    let mints = verify_accepted_claim_creq(creq, followup_id, amount_sats)?;
    if let Some(extra) = mints.iter().find(|m| !claim_mints.contains(m)) {
        return Err(JobLifecycleError::Input(format!(
            "turn creq quotes mint {extra} the claim did not — refused"
        )));
    }
    Ok(mints)
}

/// The turn's ACCEPT: today's accept draft plus `["e", result, "", "reply"]` (§2.4), projected
/// exactly like turn 1's on a v2 job.
async fn publish_turn_accept(
    home: &MaxplayerHome,
    keys: &nostr_sdk::Keys,
    bind: &AcceptedBind,
) -> Result<String, JobLifecycleError> {
    let mut draft = accept_draft(
        &bind.job_id,
        &bind.claim_id,
        &keys.public_key().to_hex(),
        &bind.seller_pubkey,
    );
    crate::session::bind_accept_to_result(&mut draft, &bind.result_id);
    let draft = if let Some(evidence) = &bind.private_evidence {
        let policy = crate::private_content::runtime::Policy::for_evidence(home, evidence)
            .map_err(|e| JobLifecycleError::Input(e.to_string()))?;
        crate::private_content::carriers::project(
            &evidence.offer,
            &draft,
            Some(&evidence.award),
            None,
            &policy.host,
        )
        .map_err(|e| JobLifecycleError::Input(e.to_string()))?
    } else {
        draft
    };
    publish_draft_async(home, keys, &draft).await
}

/// The commit a git turn must descend from (§8.4): the newest earlier git turn's accepted commit,
/// turn 1 included. `earlier` lists the earlier turns' follow-up ids, oldest first. `None` when
/// every earlier turn was inline: the seller's session base is then its own provisioning commit,
/// which the buyer never saw, so there is nothing to compare against.
pub fn turn_parent_commit(
    home: &MaxplayerHome,
    job_id: &str,
    earlier: &[String],
) -> Result<Option<String>, JobLifecycleError> {
    for id in earlier.iter().rev().map(String::as_str).chain([job_id]) {
        let bind = load_accepted_bind(home, id)?.ok_or_else(|| {
            JobLifecycleError::Input(format!("earlier turn {id} of {job_id} is not accepted"))
        })?;
        if !bind_is_inline_kind(bind.delivery_kind.as_deref()) {
            return Ok(Some(bind.commit_oid));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(job: &str, followup: Option<&str>, commit: &str, inline: bool) -> AcceptedBind {
        AcceptedBind {
            private_evidence: None,
            job_id: job.into(),
            claim_id: "c".into(),
            result_id: format!("r-{commit}"),
            seller_pubkey: "aa".repeat(32),
            commit_oid: commit.into(),
            repo: String::new(),
            branch: String::new(),
            delivery_kind: inline.then(|| crate::receipt::DeliveryKind::Inline.as_str().into()),
            inline_answer: inline.then(|| "answer".into()),
            job_hash: "ff".repeat(32),
            amount_sats: 0,
            accept_event_id: "e".into(),
            accepted_at: 1,
            seller_signature: "ab".repeat(32),
            creq_hash: None,
            accepted_mints: Vec::new(),
            funding_mint: None,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: None,
            payment_mode: PaymentMode::None,
            followup_id: followup.map(str::to_owned),
        }
    }

    /// Failure test 10 (buyer half, bind chain): the parent a git turn must have is the newest
    /// earlier GIT turn's commit — inline turns are skipped, turn 1 is the floor — and a turn
    /// bind lives at its follow-up's path, never over turn 1's.
    #[test]
    fn turn_parent_is_the_newest_earlier_git_turn() {
        let dir = tempfile::tempdir().unwrap();
        let home = crate::home::bootstrap(dir.path()).unwrap();
        let (c1, c2) = ("11".repeat(20), "22".repeat(20));
        write_accepted_bind(&home, &bind("job", None, &c1, false)).unwrap();
        assert_eq!(
            turn_parent_commit(&home, "job", &[]).unwrap(),
            Some(c1.clone())
        );
        write_accepted_bind(&home, &bind("job", Some("f2"), &c2, false)).unwrap();
        write_accepted_bind(&home, &bind("job", Some("f3"), &"33".repeat(32), true)).unwrap();
        assert_eq!(
            load_accepted_bind(&home, "job")
                .unwrap()
                .unwrap()
                .commit_oid,
            c1
        );
        assert_eq!(
            turn_parent_commit(&home, "job", &["f2".into(), "f3".into()]).unwrap(),
            Some(c2)
        );
        // An unaccepted earlier turn refuses rather than skipping to an older parent.
        assert!(turn_parent_commit(&home, "job", &["f9".into()]).is_err());
        // Every earlier turn inline: nothing the buyer saw to compare against.
        let dir2 = tempfile::tempdir().unwrap();
        let home2 = crate::home::bootstrap(dir2.path()).unwrap();
        write_accepted_bind(&home2, &bind("job", None, &"44".repeat(32), true)).unwrap();
        assert_eq!(turn_parent_commit(&home2, "job", &[]).unwrap(), None);
    }

    #[test]
    fn turn_creq_names_the_followup_and_quotes_only_claim_mints() {
        let mint = "https://testnut.cashu.space";
        let seller = nostr_sdk::Keys::generate().public_key().to_hex();
        let creq = |id: &str, amount: u64, mints: &[&str]| {
            crate::gateway::creq::build_seller_creq(
                id,
                amount,
                "sat",
                &mints.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
                &seller,
            )
            .unwrap()
        };
        use std::str::FromStr;
        let claim = vec![cashu::MintUrl::from_str(mint).unwrap().to_string()];
        let ok = creq("fu", 21, &[mint]);
        assert_eq!(turn_creq_mints(Some(&ok), "fu", 21, &claim).unwrap(), claim);
        // The job's id, another amount, a foreign mint, or no creq at all: refused.
        for (raw, id, amount) in [
            (creq("job", 21, &[mint]), "fu", 21),
            (creq("fu", 22, &[mint]), "fu", 21),
            (creq("fu", 21, &["https://other.example"]), "fu", 21),
        ] {
            assert!(turn_creq_mints(Some(&raw), id, amount, &claim).is_err());
        }
        assert!(turn_creq_mints(None, "fu", 21, &claim).is_err());
    }
}
