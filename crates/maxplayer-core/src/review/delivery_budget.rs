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
    let Some(probe) = probe(home, signer, job)? else {
        return Ok(inline_requested);
    };
    let Probe {
        request,
        claim,
        award,
        policy,
        reviewer,
    } = &probe;
    let identity = Identity::Seller(signer);
    let git = budget_request(request, claim, award, policy, Reserve::Git)?;
    identity
        .request(git.draft(&reviewer.to_hex())?, *reviewer)
        .await?;
    if !inline_requested {
        return Ok(false);
    }
    let inline = budget_request(request, claim, award, policy, Reserve::InlineCeiling)?;
    fits(&identity, &inline, reviewer).await
}

/// The session tags an inline RESULT carries on top of a one-shot result (session-jobs §2.3),
/// exactly the arguments [`crate::session::append_result_tags`] is called with when the result is
/// built, so the probe sizes the same tags the published result has.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResultTags<'a> {
    /// Turn n ≥ 2: the follow-up the result replies to (`["e", id, "", "reply"]`).
    pub(crate) followup_id: Option<&'a str>,
    /// `turn_status` (written only when not `done`).
    pub(crate) status: crate::session::TurnStatus,
    /// `["context", "reset"]`.
    pub(crate) context_reset: bool,
    /// A priced turn's fresh payment request (`creq`).
    pub(crate) turn_creq: Option<&'a str>,
}

impl ResultTags<'_> {
    /// A one-shot result, or a session's turn 1 that ended `done`: no session tags.
    pub(crate) const NONE: ResultTags<'static> = ResultTags {
        followup_id: None,
        status: crate::session::TurnStatus::Done,
        context_reset: false,
        turn_creq: None,
    };
}

/// Whether THIS inline result (its answer and session tags), with this offer's task, fits one
/// delivery review request.
///
/// [`seller_delivery_budget`] reserves the largest answer the protocol admits (the full envelope
/// ceiling), which a long private task can push past the seal limit although a real answer would
/// fit. A Git-capable agent is then steered to Git before it runs; a hook has no Git route, so it
/// runs and its actual answer is measured here before anything is delivered (dot-bot, rc3 swarm
/// round 2: the hook never ran). A session turn n ≥ 2 is measured here too: its RESULT carries
/// more tags than turn 1's reservation covered. `Ok(false)` is permanent: this result cannot be
/// reviewed. `Ok(true)` for a job with no private delivery review.
pub(crate) async fn seller_inline_answer_fits(
    home: &MaxplayerHome,
    signer: &crate::seller_node::signer::SignerHandle,
    job: &str,
    answer: &str,
    tags: ResultTags<'_>,
) -> Result<bool, String> {
    let Some(probe) = probe(home, signer, job)? else {
        return Ok(true);
    };
    let Probe {
        request,
        claim,
        award,
        policy,
        reviewer,
    } = &probe;
    // Building the request only fails on this answer's own content (its serialized body over the
    // limit, say): the same inputs already passed the worst-case probe. Retrying cannot help.
    let inline = budget_request(request, claim, award, policy, Reserve::Inline(answer, tags))
        .map_err(|error| format!("review: permanent input error (answer): {error}"))?;
    fits(&Identity::Seller(signer), &inline, reviewer).await
}

/// Seal a probe request locally (never sent): `Ok(false)` when it is too large to review.
async fn fits(
    identity: &Identity<'_>,
    request: &Request,
    reviewer: &PublicKey,
) -> Result<bool, String> {
    // The request guard is derived from the seal limit (#1349), so a request too large to seal can
    // fail in `draft` instead of `wrap`: both are permanent, both mean "does not fit" (the worst-case
    // probe then selects Git; a measured answer is not delivered).
    let sent = match request.draft(&reviewer.to_hex()) {
        Ok(draft) => identity.request(draft, *reviewer).await,
        Err(error) => Err(error),
    };
    match sent {
        Ok(_) => Ok(true),
        Err(error) if permanent_error(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// What a probe sizes a delivery review for.
#[derive(Clone, Copy)]
enum Reserve<'a> {
    /// A Git result (fixed-size commit note).
    Git,
    /// The largest inline answer the protocol admits, before the agent has run.
    InlineCeiling,
    /// An inline answer that exists, with the session tags its result carries.
    Inline(&'a str, ResultTags<'a>),
}

struct Probe {
    request: Request,
    claim: Event,
    award: Event,
    policy: pc::runtime::Policy,
    reviewer: PublicKey,
}

/// The private offer's review inputs, or `None` when the job is not a private one (no review).
fn probe(
    home: &MaxplayerHome,
    signer: &crate::seller_node::signer::SignerHandle,
    job: &str,
) -> Result<Option<Probe>, String> {
    let Some((mut ctx, offer)) =
        crate::seller_node::privacy::known(home, signer.public_key_hex(), job)
            .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    if !is_private(&offer) {
        return Ok(None);
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
    Ok(Some(Probe {
        request,
        claim,
        award,
        policy: ctx.policy,
        reviewer,
    }))
}

fn budget_request(
    request: &Request,
    claim: &Event,
    award: &Event,
    policy: &pc::runtime::Policy,
    reserve: Reserve<'_>,
) -> Result<Request, String> {
    use crate::gateway;
    let inline = !matches!(reserve, Reserve::Git);
    let answer = match reserve {
        Reserve::Inline(answer, _) => answer,
        _ => "x",
    };
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
        if inline { answer.into() } else { note.clone() },
        None,
        &policy.host,
    )
    .map_err(|e| e.to_string())?;
    let mut local = if inline {
        gateway::inline_result_draft(
            &id,
            &buyer,
            "text/plain",
            amount,
            &hex64,
            &"f".repeat(128),
            answer,
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
    // The session tags the delivered result carries, as `deliver_inline_with` and
    // `publish_turn_result` append them before projecting (`project` keeps all of them on 3403).
    if let Reserve::Inline(_, tags) = reserve {
        crate::session::append_result_tags(
            &mut local,
            tags.followup_id,
            tags.status,
            tags.context_reset,
            tags.turn_creq,
        );
    }
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
    let answer_envelope = if matches!(reserve, Reserve::InlineCeiling) {
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
        // #1349: the request guard now sits at the seal limit, so too large fails in `draft`.
        let Ok(draft) = r.draft(&service.to_hex()) else {
            return false;
        };
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
                    let git =
                        budget_request(&r, &e.claim, &e.award, &policy, Reserve::Git).unwrap();
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
    /// dot-bot, rc3 swarm round 2: a private inline-only offer whose task is long (the other
    /// members' votes and evidence) fails the WORST-CASE inline reservation, so the seller fell
    /// back to Git, which a hook cannot deliver, and the hook never ran. The answer it would have
    /// given still fits: [`seller_inline_answer_fits`] measures that answer, and still refuses one
    /// that cannot be reviewed. RED ON REVERT: size a real answer at the envelope ceiling.
    #[tokio::test]
    async fn a_long_task_fails_the_ceiling_but_a_real_answer_still_fits() {
        let mints = vec!["https://testnut.cashu.space".to_owned()];
        let fixture = |task: &str| {
            let (e, _, _, policy) = pc::evidence::inline_fixture_with_text(
                true,
                false,
                2_000_000_000,
                mints.clone(),
                task,
                "x",
            );
            let (dir, home) = home(&e, &policy);
            let signer = crate::seller_node::signer::spawn(&home).unwrap();
            (e, dir, home, signer)
        };
        let mut short = None;
        for size in (1_000..=16_000).step_by(1_000) {
            let (e, dir, home, signer) = fixture(&"x".repeat(size));
            if !seller_delivery_budget(&home, &signer, &e.offer.id.to_hex(), true)
                .await
                .unwrap()
            {
                short = Some((size, e, dir, home, signer));
                break;
            }
        }
        let (size, e, _dir, home, signer) =
            short.expect("harness check: some task length must exhaust the ceiling reservation");
        eprintln!("the inline ceiling reservation no longer fits from a {size}-byte task");
        let job = e.offer.id.to_hex();
        let answer = "A short chart: Muse A, Grok Bot A, Dots B, Instinct B.";
        assert!(
            seller_inline_answer_fits(&home, &signer, &job, answer, ResultTags::NONE)
                .await
                .unwrap(),
            "a real answer to a {size}-byte task fits its delivery review"
        );
        // The measurement is real, not a pass-through: an answer whose envelope nears the ceiling
        // (control bytes escape sixfold) is refused beside a long task while the short one fits.
        let largest = format!("x{}", "\u{1}".repeat(2_600));
        let mut refused = None;
        for task in (size..=pc::MAX_BODY_BYTES - 512).step_by(500) {
            let (e, _dir, home, signer) = fixture(&"x".repeat(task));
            let job = e.offer.id.to_hex();
            match seller_inline_answer_fits(&home, &signer, &job, &largest, ResultTags::NONE).await
            {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => panic!("a {task}-byte task: {error}"),
            }
            assert!(
                seller_inline_answer_fits(&home, &signer, &job, answer, ResultTags::NONE)
                    .await
                    .unwrap(),
                "at a {task}-byte task the short answer still fits"
            );
            refused = Some(task);
            break;
        }
        let refused = refused.expect("some task length must refuse the largest answer");
        let (e, _dir, home, signer) = fixture("x");
        let error = seller_inline_answer_fits(
            &home,
            &signer,
            &e.offer.id.to_hex(),
            &"\u{1}".repeat(4_000),
            ResultTags::NONE,
        )
        .await
        .unwrap_err();
        assert!(
            permanent_error(&error),
            "an answer body over the limit is refused, not retried: {error}"
        );
        eprintln!("the largest inline answer is refused from a {refused}-byte task");
    }

    /// An answer whose delivery body is exactly `n` bytes: a control byte escapes to six body bytes
    /// (`\u0001`), so `n / 6` of them and `n % 6` letters. Each step adds one body byte, and the raw
    /// answer stays far under the inline answer cap.
    fn answer_of(n: usize) -> String {
        format!("{}{}", "\u{1}".repeat(n / 6), "x".repeat(n % 6))
    }

    /// The buyer's delivery review request for the RESULT a seller (key 2, the fixtures' seat)
    /// publishes for `answer` with `tags`: built and signed as `deliver_inline_with` /
    /// `publish_turn_result` build it, with a real timestamp, independently of the probe.
    fn real_request(
        e: &pc::evidence::PrivateEvidence,
        policy: &pc::runtime::Policy,
        answer: &str,
        tags: ResultTags<'_>,
    ) -> Request {
        let seller = Keys::parse(&format!("{:064x}", 2)).unwrap();
        let id = e.offer.id.to_hex();
        let o = pc::wire::validate_private(&e.offer, &policy.host).unwrap();
        let amount = pc::wire::decimal(o.required("amount").unwrap()).unwrap();
        let content = pc::carriers::prepare_content(
            &e.offer,
            Some(&e.claim),
            Some(&e.award),
            &seller.public_key().to_hex(),
            &policy.service,
            pc::ContentType::Answer,
            answer.into(),
            None,
            &policy.host,
        )
        .unwrap();
        let job_hash = pc::job_hash(&id).unwrap();
        let sig = seller
            .sign_schnorr(&nostr_sdk::secp256k1::Message::from_digest([7; 32]))
            .to_string();
        let mut local = crate::gateway::inline_result_draft(
            &id,
            &e.offer.pubkey.to_hex(),
            "text/markdown",
            amount,
            &job_hash,
            &sig,
            answer,
            &[],
        );
        crate::session::append_result_tags(
            &mut local,
            tags.followup_id,
            tags.status,
            tags.context_reset,
            tags.turn_creq,
        );
        let result = pc::builders::sign(
            &seller,
            pc::carriers::project(
                &e.offer,
                &local,
                Some(&e.award),
                Some(&content),
                &policy.host,
            )
            .unwrap(),
        )
        .unwrap();
        let mut evidence = e.clone();
        evidence.result = result;
        evidence.answer_envelope = Some(content.envelope().to_owned());
        request(&evidence)
    }

    /// The largest `n` in `0..hi` for which `fits(answer_of(n))`; `fits` must hold at 0 and fail at `hi`.
    async fn largest<F, Fut>(hi: usize, mut fits: F) -> usize
    where
        F: FnMut(usize) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        assert!(fits(0).await, "harness check: an empty answer fits");
        assert!(
            !fits(hi).await,
            "harness check: a {hi}-byte body does not fit"
        );
        let (mut lo, mut hi) = (0, hi);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if fits(mid).await {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// #1441 advisor r1 (Findings 1 and 3): the answer probe sizes the RESULT that is published,
    /// session tags included: turn 1's `turn_status`, and turn n ≥ 2's `reply` and `creq`. For
    /// each, the probe's largest answer is checked against the buyer's real delivery review request
    /// for a result carrying those tags: the real request still seals at the probe's limit (never
    /// optimistic) and stops within a few bytes of it (the timestamp margin). The tags cost real
    /// bytes, so each limit is below the one-shot result's.
    /// RED ON REVERT: size the answer without the session tags.
    #[tokio::test]
    async fn the_answer_probe_sizes_the_session_tags_of_the_result_it_measures() {
        let mints = vec!["https://testnut.cashu.space".to_owned()];
        let fid = "f".repeat(64);
        let creq = crate::gateway::creq::build_seller_creq(
            &fid,
            10,
            "sat",
            &mints,
            &Keys::parse(&format!("{:064x}", 2))
                .unwrap()
                .public_key()
                .to_hex(),
        )
        .unwrap();
        let cases = [
            ("one-shot", ResultTags::NONE),
            (
                "turn 1, needs_input",
                ResultTags {
                    status: crate::session::TurnStatus::NeedsInput,
                    ..ResultTags::NONE
                },
            ),
            (
                "turn 2, priced, out_of_time",
                ResultTags {
                    followup_id: Some(&fid),
                    status: crate::session::TurnStatus::OutOfTime,
                    turn_creq: Some(&creq),
                    ..ResultTags::NONE
                },
            ),
        ];
        // A long private task (the ceiling reservation fails) with room for a few KiB of answer.
        let mut chosen = None;
        for task in (4_000..=pc::MAX_BODY_BYTES - 600).step_by(1_000) {
            let (e, _, buyer, policy) = pc::evidence::inline_fixture_with_text(
                true,
                true,
                2_000_000_000,
                mints.clone(),
                &"t".repeat(task),
                "x",
            );
            let (dir, home) = home(&e, &policy);
            let signer = crate::seller_node::signer::spawn(&home).unwrap();
            let job = e.offer.id.to_hex();
            if seller_delivery_budget(&home, &signer, &job, true)
                .await
                .unwrap()
            {
                continue;
            }
            let fits_max = seller_inline_answer_fits(
                &home,
                &signer,
                &job,
                &answer_of(15_000),
                ResultTags::NONE,
            )
            .await
            .unwrap();
            if !fits_max {
                chosen = Some((task, e, buyer, policy, dir, home, signer));
                break;
            }
        }
        let (task, e, buyer, policy, _dir, home, signer) =
            chosen.expect("harness check: some long task leaves less than 15000 answer bytes");
        let job = e.offer.id.to_hex();
        let service = PublicKey::from_hex(&policy.service).unwrap();
        let mut one_shot = None;
        for (name, tags) in cases {
            let probed = largest(15_000, |n| {
                let (home, signer, job) = (&home, &signer, &job);
                async move {
                    seller_inline_answer_fits(home, signer, job, &answer_of(n), tags)
                        .await
                        .unwrap_or_else(|error| panic!("{name}: {n}: {error}"))
                }
            })
            .await;
            let real = largest(15_000, |n| {
                let r = real_request(&e, &policy, &answer_of(n), tags);
                let buyer = buyer.clone();
                async move { wraps(&r, &buyer, service).await }
            })
            .await;
            eprintln!("{task}-byte task, {name}: probe {probed}, real {real}");
            assert!(
                real >= probed,
                "{name}: the probe accepts a {probed}-byte answer whose real review request does not seal (real limit {real})"
            );
            assert!(
                real - probed <= 16,
                "{name}: the probe refuses {} bytes the real request still holds",
                real - probed
            );
            match one_shot {
                None => one_shot = Some(probed),
                Some(limit) => assert!(
                    probed < limit,
                    "{name}: the session tags must cost answer bytes ({probed} vs one-shot {limit})"
                ),
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
