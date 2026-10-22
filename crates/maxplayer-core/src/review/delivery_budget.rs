//! Before agent execution, reserve the full review message, not just task bytes.
//! This is a size probe only: placeholder result IDs/signatures are NEVER sent.
use super::*;

/// Return whether inline delivery is safe for every protocol-admitted answer.
/// Git is always supported by the buyer protocol. If even Git cannot fit, refuse
/// before agent execution rather than producing work that cannot be reviewed.
pub(crate) async fn seller_delivery_budget(
    home: &MaxplayerHome,
    signer: &crate::seller_node::signer::SignerHandle,
    job: &str,
    inline_requested: bool,
) -> Result<bool, String> {
    let Some((mut ctx, offer)) =
        crate::seller_node::privacy::known(home, signer.public_key_hex(), job)
            .map_err(|e| e.to_string())?
    else {
        return Ok(inline_requested);
    };
    if !is_private(&offer) {
        return Ok(inline_requested);
    }
    let (claim, award) = ctx
        .store
        .selection(job)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| reviewer_error("invalid_subject"))?;
    let task_envelope = if pc::wire::validate_private(&offer, &ctx.policy.host)
        .map_err(|_| reviewer_error("invalid_subject"))?
        .get("discovery")
        == Some("targeted")
    {
        Some(
            ctx.accept_content(&offer, &offer, None, None, None, Timestamp::now().as_secs())
                .map_err(|e| e.to_string())?
                .envelope()
                .to_owned(),
        )
    } else {
        None
    };
    let request = Request {
        subject: Subject {
            offer: job.into(),
            event: job.into(),
            kind: JOB_OFFER_KIND,
            commit: None,
        },
        offer,
        task_envelope,
        delivery: None,
        followup: None,
    };
    let reviewer =
        PublicKey::from_hex(&ctx.policy.service).map_err(|_| reviewer_error("invalid_subject"))?;
    let identity = Identity::Seller(signer);
    let git = budget_request(&request, &claim, &award, &ctx.policy, false)?;
    identity
        .request(git.draft(&reviewer.to_hex())?, reviewer)
        .await?;
    if !inline_requested {
        return Ok(false);
    }
    let inline = budget_request(&request, &claim, &award, &ctx.policy, true)?;
    match identity
        .request(inline.draft(&reviewer.to_hex())?, reviewer)
        .await
    {
        Ok(_) => Ok(true),
        Err(error) if permanent_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn budget_request(
    request: &Request,
    claim: &Event,
    award: &Event,
    policy: &pc::runtime::Policy,
    inline: bool,
) -> Result<Request, String> {
    use crate::gateway;
    let o = pc::wire::validate_private(&request.offer, &policy.host)
        .map_err(|_| reviewer_error("invalid_subject"))?;
    let id = request.offer.id.to_hex();
    let buyer = request.offer.pubkey.to_hex();
    let seller = claim.pubkey.to_hex();
    let amount = pc::wire::decimal(o.required("amount").map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let hex64 = "f".repeat(64);
    let note = format!("delivery commit {hex64}");
    let content = pc::carriers::prepare_content(
        &request.offer,
        Some(claim),
        Some(award),
        &seller,
        &policy.service,
        pc::ContentType::Answer,
        if inline { "x".into() } else { note.clone() },
        None,
        &policy.host,
    )
    .map_err(|e| e.to_string())?;
    let local = if inline {
        gateway::inline_result_draft(
            &id,
            &buyer,
            "text/plain",
            amount,
            &hex64,
            &"f".repeat(128),
            "x",
            &[],
        )
    } else {
        gateway::git_result_draft(
            &id,
            &buyer,
            "text/plain",
            &policy
                .host
                .job_repo(&buyer, o.required("job").map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?,
            &format!("refs/heads/delivery/{hex64}"),
            &hex64,
            amount,
            &hex64,
            &"f".repeat(128),
            note,
            &[],
        )
    };
    let draft = pc::carriers::project(
        &request.offer,
        &local,
        Some(award),
        Some(&content),
        &policy.host,
    )
    .map_err(|e| e.to_string())?;
    // Signature and ID are fixed-length on the wire. The largest timestamp also
    // reserves space for the actual timestamp at delivery, including a restart.
    let mut result = award.clone();
    result.pubkey = claim.pubkey;
    result.kind = Kind::Custom(JOB_RESULT_KIND);
    result.created_at = Timestamp::from(u64::MAX);
    result.tags = Tags::from_list(
        draft
            .tags
            .into_iter()
            .map(|tag| Tag::parse(tag.0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| reviewer_error("invalid_subject"))?,
    );
    result.content = draft.content;
    let answer_envelope = if inline {
        // Envelope keys/nonce are fixed, body_b64 never needs JSON escaping.
        // Reserve the FULL envelope ceiling plus escaping slack, not 4096 raw
        // answer bytes: quotes, newlines and control bytes can expand sixfold.
        Some(format!(
            "{}{}",
            "A".repeat(pc::MAX_ENVELOPE_BYTES),
            "\"".repeat(64)
        ))
    } else {
        Some(content.envelope().to_owned())
    };
    Ok(Request {
        subject: Subject {
            offer: id,
            event: result.id.to_hex(),
            kind: JOB_RESULT_KIND,
            commit: (!inline).then_some(hex64),
        },
        offer: request.offer.clone(),
        task_envelope: request.task_envelope.clone(),
        delivery: Some(pc::evidence::PrivateEvidence {
            offer: request.offer.clone(),
            task_envelope: request.task_envelope.clone(),
            claim: claim.clone(),
            award: award.clone(),
            result,
            answer_envelope,
        }),
        followup: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(e: &pc::evidence::PrivateEvidence) -> Request {
        Request {
            subject: Subject {
                offer: e.offer.id.to_hex(),
                event: e.result.id.to_hex(),
                kind: JOB_RESULT_KIND,
                commit: None,
            },
            offer: e.offer.clone(),
            task_envelope: e.task_envelope.clone(),
            delivery: Some(e.clone()),
            followup: None,
        }
    }
    async fn wraps(r: &Request, buyer: &Keys, service: PublicKey) -> bool {
        let draft = r.draft(&service.to_hex()).unwrap();
        let inner = crate::gateway::nostr::event_builder(&draft)
            .unwrap()
            .sign_with_keys(buyer)
            .unwrap();
        wrap(buyer, service, &inner).await.is_ok()
    }
    fn home(
        e: &pc::evidence::PrivateEvidence,
        policy: &pc::runtime::Policy,
    ) -> (tempfile::TempDir, MaxplayerHome) {
        let dir = tempfile::tempdir().unwrap();
        let mut home = crate::home::bootstrap(dir.path()).unwrap();
        home.config.privacy.service_pubkey = Some(policy.service.clone());
        home.config.privacy.git_base = Some(policy.host.git_prefix.clone());
        home.config.accepted_mints = policy.host.accepted_mints.clone();
        std::fs::write(&home.key_path, format!("{:064x}", 2)).unwrap();
        let mut ctx = pc::channel::ContentContext::open(&home, &e.claim.pubkey.to_hex()).unwrap();
        let now = Timestamp::now().as_secs();
        if let Some(task) = &e.task_envelope {
            ctx.stage(&pc::PreparedContent::decode(task).unwrap(), now)
                .unwrap();
        }
        ctx.resolve_offer(&e.offer, now).unwrap();
        ctx.select(&e.offer, &e.claim, &e.award).unwrap();
        (dir, home)
    }
    #[tokio::test]
    async fn private_delivery_budget_accounts_for_real_metadata_and_escaped_answers_before_work() {
        for targeted in [false, true] {
            let mints: Vec<String> = (0..3)
                .map(|i| format!("https://mint{i}.example/{}", "m".repeat(500)))
                .collect();
            let (small, _, _, _) = pc::evidence::inline_fixture_with_metadata(
                targeted,
                true,
                2_000_000_000,
                mints.clone(),
                "x",
                "x",
                true,
            );
            let overhead = if let Some(env) = &small.task_envelope {
                pc::PreparedContent::decode(env).unwrap().body_bytes().len() - 1
            } else {
                small
                    .offer
                    .tags
                    .iter()
                    .find(|t| t.as_slice()[0] == "i")
                    .unwrap()
                    .as_slice()[1]
                    .len()
                    - 1
            };
            for task_unit in ["x", "\"", "é"] {
                let cost = serde_json::to_string(task_unit).unwrap().len() - 2;
                let task = task_unit.repeat((pc::MAX_BODY_BYTES - overhead) / cost);
                for answer_unit in ["\"", "\n", "\u{1}"] {
                    let (e, _, buyer, policy) = pc::evidence::inline_fixture_with_metadata(
                        targeted,
                        true,
                        2_000_000_000,
                        mints.clone(),
                        &task,
                        &format!(
                            "x{}",
                            answer_unit.repeat(if answer_unit == "\u{1}" {
                                2000
                            } else {
                                crate::seller_node::run::INLINE_ANSWER_MAX_BYTES - 1
                            })
                        ),
                        true,
                    );
                    let r = request(&e);
                    e.validate(&buyer.public_key().to_hex(), &policy).unwrap_or_else(|error| panic!("targeted={targeted}, task={task_unit:?}, answer={answer_unit:?}: {error}"));
                    r.validate(&buyer.public_key(), &policy).unwrap();
                    let service = PublicKey::from_hex(&policy.service).unwrap();
                    assert!(
                        !wraps(&r, &buyer, service).await,
                        "regression must reproduce the old oversized delivery"
                    );
                    let git = budget_request(&r, &e.claim, &e.award, &policy, false).unwrap();
                    assert!(
                        wraps(&git, &buyer, service).await,
                        "reserved Git result must fit"
                    );
                    let (_dir, home) = home(&e, &policy);
                    let signer = crate::seller_node::signer::spawn(&home).unwrap();
                    assert!(
                        !seller_delivery_budget(&home, &signer, &e.offer.id.to_hex(), true)
                            .await
                            .unwrap(),
                        "must select files before running the agent"
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn private_delivery_budget_keeps_small_inline_and_refuses_oversized_git_before_work() {
        let (e, _, _, policy) = pc::evidence::inline_fixture_for(true);
        let (_dir, home) = home(&e, &policy);
        let signer = crate::seller_node::signer::spawn(&home).unwrap();
        assert!(
            seller_delivery_budget(&home, &signer, &e.offer.id.to_hex(), true)
                .await
                .unwrap()
        );
        assert!(
            !seller_delivery_budget(&home, &signer, &e.offer.id.to_hex(), false)
                .await
                .unwrap()
        );
        // A valid but unusually long signed invoice can exhaust even the Git budget.
        let mints = (0..5)
            .map(|i| format!("https://mint{i}.example/{}", "m".repeat(2000)))
            .collect();
        let (e, _, _, policy) = pc::evidence::inline_fixture_with_text(
            true,
            true,
            2_000_000_000,
            mints,
            &"x".repeat(15000),
            "x",
        );
        let (_dir, home) = self::home(&e, &policy);
        let signer = crate::seller_node::signer::spawn(&home).unwrap();
        let error = seller_delivery_budget(&home, &signer, &e.offer.id.to_hex(), true)
            .await
            .unwrap_err();
        assert!(permanent_error(&error), "{error}");
    }
}
