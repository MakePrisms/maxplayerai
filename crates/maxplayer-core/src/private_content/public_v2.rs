//! Public trades retain public task/Git content, but share v2 artifact and receipt
//! bindings. Visibility is read from the signed root, never guessed from a child.
use super::{ContentBody, ContentType, Error, PreparedContent, Result};
use crate::{
    gateway::{self, EventDraft, TagSpec},
    home::MaxplayerHome,
};
use nostr_sdk::Event;
use std::collections::BTreeSet;

pub fn value<'a>(event: &'a Event, key: &str) -> Result<Option<&'a str>> {
    let mut found = None;
    for tag in event.tags.iter() {
        let row = tag.as_slice();
        let at = if key == "root" || key == "reply" {
            if row.first().map(String::as_str) == Some("e")
                && row.get(3).map(String::as_str) == Some(key)
            {
                Some(1)
            } else {
                None
            }
        } else if let Some(role) = key.strip_prefix("sig:") {
            if row.first().map(String::as_str) == Some("sig")
                && row.get(1).map(String::as_str) == Some(role)
            {
                Some(2)
            } else {
                None
            }
        } else if row.first().map(String::as_str) == Some(key) {
            Some(1)
        } else {
            None
        };
        if let Some(at) = at {
            let v = row.get(at).ok_or(Error("incomplete public v2 tag"))?;
            if found.replace(v.as_str()).is_some() {
                return Err(Error("duplicate public v2 binding"));
            }
        }
    }
    Ok(found)
}
pub fn required<'a>(event: &'a Event, key: &str) -> Result<&'a str> {
    value(event, key)?.ok_or(Error("public v2 binding missing"))
}
pub fn is_public(event: &Event) -> bool {
    value(event, "v") == Ok(Some("2")) && value(event, "visibility") == Ok(Some("public"))
}
fn participants(event: &Event) -> Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for t in event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("p"))
    {
        let row = t.as_slice();
        let p = row.get(1).ok_or(Error("missing public participant"))?;
        super::require_hex(p, 32)?;
        if !keys.insert(p.clone()) {
            return Err(Error("duplicate public participant"));
        }
    }
    Ok(keys)
}
pub fn validate_offer(event: &Event) -> Result<gateway::ParsedOffer> {
    event
        .verify()
        .map_err(|_| Error("invalid public offer signature"))?;
    if event.kind.as_u16() != 3401 || !is_public(event) || required(event, "t")? != "maxplayer" {
        return Err(Error("not a public v2 offer"));
    }
    super::require_hex(required(event, "job")?, 32)?;
    for key in ["i", "output", "amount"] {
        required(event, key)?;
    }
    let targets = participants(event)?;
    if targets.len() > 1 {
        return Err(Error("ambiguous public target"));
    }
    let parsed = gateway::parse_offer(&crate::job_lifecycle::event_to_draft(event))
        .map_err(|_| Error("invalid public offer"))?;
    if parsed.payment_mode.is_free() && parsed.amount != 0 {
        return Err(Error("invalid free public offer"));
    }
    Ok(parsed)
}
pub fn validate_child(offer: &Event, event: &Event) -> Result<()> {
    validate_offer(offer)?;
    event
        .verify()
        .map_err(|_| Error("invalid public child signature"))?;
    if !is_public(event)
        || required(event, "root")? != offer.id.to_hex()
        || required(event, "job")? != required(offer, "job")?
        || required(event, "t")? != "maxplayer"
    {
        return Err(Error("public child root mismatch"));
    }
    Ok(())
}
pub fn validate_selection(offer: &Event, claim: &Event, award: &Event) -> Result<()> {
    let parsed = validate_offer(offer)?;
    validate_child(offer, claim)?;
    validate_child(offer, award)?;
    let selected = gateway::parse_award(&crate::job_lifecycle::event_to_draft(award))
        .ok_or(Error("invalid public award"))?;
    let parties: BTreeSet<_> = [offer.pubkey.to_hex(), claim.pubkey.to_hex()]
        .into_iter()
        .collect();
    if claim.kind.as_u16() != 3402
        || award.pubkey != offer.pubkey
        || selected.claim_id != claim.id.to_hex()
        || selected.offer_id != offer.id.to_hex()
        || required(claim, "status")? != "processing"
        || participants(claim)? != parties
        || participants(award)? != parties
        || parsed
            .seller_pubkey
            .as_deref()
            .is_some_and(|target| target != claim.pubkey.to_hex())
    {
        return Err(Error("public selection mismatch"));
    }
    if parsed.payment_mode.is_free() {
        if value(claim, "payment")? != Some("none") || value(claim, "creq")?.is_some() {
            return Err(Error("public free claim mismatch"));
        }
    } else {
        if value(claim, "payment")? == Some("none") {
            return Err(Error("public payment mismatch"));
        }
        crate::job_lifecycle::verify_accepted_claim_creq(
            Some(required(claim, "creq")?),
            &offer.id.to_hex(),
            parsed.amount,
        )
        .map_err(|_| Error("invalid public invoice"))?;
    }
    Ok(())
}
pub fn project(
    offer: &Event,
    mut draft: EventDraft,
    award: Option<&Event>,
    answer: Option<&PreparedContent>,
) -> Result<EventDraft> {
    validate_offer(offer)?;
    // Existing self-trades can have buyer == seller. Collapse locally generated
    // role tags before signing; remote duplicates remain invalid/ambiguous.
    let mut participant_keys = BTreeSet::new();
    draft.tags.retain(|t| {
        t.first() != Some("p") || participant_keys.insert(t.value().unwrap_or("").to_owned())
    });
    draft.tags.retain(|t| {
        !matches!(
            t.first(),
            Some("v" | "job" | "visibility" | "award" | "content-id" | "content-commitment")
        )
    });
    draft.tags.extend([
        TagSpec::new(["v", "2"]),
        TagSpec::new(["visibility", "public"]),
        TagSpec::new(["job", required(offer, "job")?]),
    ]);
    if draft.kind == 3403 {
        let award = award.ok_or(Error("public result needs exact award"))?;
        draft.tags.push(TagSpec::new(["award", &award.id.to_hex()]));
        draft.tags.retain(|t| t.first() != Some("job-hash"));
        draft.tags.push(TagSpec::new([
            "job-hash",
            &super::job_hash(&offer.id.to_hex())?,
        ]));
        if draft
            .tags
            .iter()
            .any(|t| t.0.as_slice() == ["delivery", "inline"])
        {
            let answer = answer.ok_or(Error("public inline envelope missing"))?;
            draft.content = answer.envelope().into();
            draft.tags.extend([
                TagSpec::new(["content-id", &answer.body().message_id]),
                TagSpec::new(["content-commitment", answer.commitment()]),
            ]);
        }
    }
    Ok(draft)
}
pub fn offer_draft(mut draft: EventDraft) -> Result<EventDraft> {
    draft
        .tags
        .retain(|t| !matches!(t.first(), Some("v" | "job" | "visibility")));
    draft.tags.extend([
        TagSpec::new(["v", "2"]),
        TagSpec::new(["visibility", "public"]),
        TagSpec::new(["job", &super::random_id()?]),
    ]);
    Ok(draft)
}

// Separate signed-evidence namespace; private-content reads cannot accidentally
// treat public envelopes as recipient-authenticated private messages.
pub struct Context {
    pub store: super::store::ContentStore,
}
impl Context {
    pub fn open(home: &MaxplayerHome) -> Result<Self> {
        Ok(Self {
            store: super::store::ContentStore::open(&home.root.join("public-v2.sqlite"))?,
        })
    }
    pub fn remember(&mut self, offer: &Event, event: &Event) -> Result<()> {
        validate_offer(offer)?;
        if offer.id != event.id {
            validate_child(offer, event)?;
        }
        self.store.remember_public(offer, event)
    }
    pub fn selection(&self, id: &str) -> Result<(Event, Event)> {
        self.store
            .selection(id)?
            .ok_or(Error("public selection missing"))
    }
    pub fn select(&mut self, offer: &Event, claim: &Event, award: &Event) -> Result<()> {
        validate_selection(offer, claim, award)?;
        self.remember(offer, claim)?;
        self.remember(offer, award)?;
        self.store.select_public(offer, claim, award)
    }
    pub fn answer(
        &mut self,
        offer: &Event,
        claim: &Event,
        award: &Event,
        text: &str,
    ) -> Result<PreparedContent> {
        validate_selection(offer, claim, award)?;
        self.store.prepare_once(
            &format!("answer:{}", offer.id),
            ContentBody {
                schema: super::BODY_SCHEMA.into(),
                job_id: required(offer, "job")?.into(),
                offer_id: Some(offer.id.to_hex()),
                award_id: Some(award.id.to_hex()),
                message_id: super::random_id()?,
                kind: ContentType::Answer,
                revision: 0,
                supersedes: None,
                author: claim.pubkey.to_hex(),
                recipients: [offer.pubkey.to_hex(), claim.pubkey.to_hex()]
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                text: text.into(),
                requested_output: None,
                dispatch: None,
                attachments: vec![],
                contribution: None,
            },
        )
    }
}
pub fn known(home: &MaxplayerHome, id: &str) -> Result<Option<(Context, Event)>> {
    let context = if home.root.join("public-v2.sqlite").exists() {
        Some(Context::open(home)?)
    } else {
        None
    };
    let offer = context
        .as_ref()
        .map(|ctx| ctx.store.event(id))
        .transpose()?
        .flatten();
    let Some(offer) = offer else {
        let lifecycle = home.root.join(crate::seller_node::STATE_DB_FILE);
        if lifecycle.exists() {
            use rusqlite::OptionalExtension;
            let db = rusqlite::Connection::open_with_flags(
                lifecycle,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(|_| Error("public protocol classification unavailable"))?;
            let marked: Option<String> = db
                .query_row(
                    "SELECT value FROM seller_meta WHERE key=?1",
                    [format!("public-v2-offer:{id}")],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| Error("public protocol classification unavailable"))?;
            if marked.is_some() {
                return Err(Error("public v2 context missing; refusing downgrade"));
            }
        }
        return Ok(None);
    };
    let context = context.ok_or(Error("public context unavailable"))?;
    validate_offer(&offer)?;
    Ok(Some((context, offer)))
}

pub fn validate_evidence(
    e: &super::evidence::PrivateEvidence,
    buyer: &str,
) -> Result<super::evidence::VerifiedEvidence> {
    let offer = validate_offer(&e.offer)?;
    validate_selection(&e.offer, &e.claim, &e.award)?;
    validate_child(&e.offer, &e.result)?;
    let result = &e.result;
    if e.offer.pubkey.to_hex() != buyer
        || result.kind.as_u16() != 3403
        || result.pubkey != e.claim.pubkey
        || required(result, "award")? != e.award.id.to_hex()
        || required(result, "job-hash")? != super::job_hash(&e.offer.id.to_hex())?
        || required(result, "output")? != offer.output
        || required(result, "amount")? != offer.amount.to_string()
        || e.task_envelope.is_some()
        || participants(result)? != [buyer.to_owned()].into_iter().collect()
    {
        return Err(Error("public result trade mismatch"));
    }
    // Validate exact unit, not just the numeric projection.
    if !result
        .tags
        .iter()
        .any(|t| t.as_slice() == ["amount", &offer.amount.to_string(), "sat"])
    {
        return Err(Error("public result unit mismatch"));
    }
    super::require_hex(required(result, "sig:seller")?, 64)?;
    let contribution = match crate::contribution::parse_contribution_offer(
        &crate::job_lifecycle::event_to_draft(&e.offer).tags,
    ) {
        Ok(Some(c)) => Some(super::Contribution {
            target_owner_pubkey: c.target.owner_pubkey().into(),
            target_clone_url: c.target.clone_url().into(),
            base_branch: c.base.branch().into(),
            base_oid: c.base.oid().into(),
            accepts: c.accepts,
            input: None,
        }),
        Ok(None) => None,
        Err(_) => return Err(Error("invalid public contribution")),
    };
    let (kind, integrity, answer) = if value(result, "delivery")? == Some("inline") {
        if contribution.is_some()
            || !offer.accepts_delivery.iter().any(|m| m == "inline")
            || ["repo", "branch", "commit"]
                .iter()
                .any(|k| value(result, k) != Ok(None))
        {
            return Err(Error("public inline delivery not eligible"));
        }
        let envelope = e
            .answer_envelope
            .as_deref()
            .ok_or(Error("public inline evidence missing"))?;
        if envelope != result.content {
            return Err(Error("public inline evidence substituted"));
        }
        let content = PreparedContent::decode(envelope)?;
        let b = content.body();
        if b.kind != ContentType::Answer
            || b.job_id != required(&e.offer, "job")?
            || b.offer_id.as_deref() != Some(e.offer.id.to_hex().as_str())
            || b.award_id.as_deref() != Some(e.award.id.to_hex().as_str())
            || b.author != result.pubkey.to_hex()
            || b.message_id != required(result, "content-id")?
            || content.commitment() != required(result, "content-commitment")?
            || b.recipients.iter().cloned().collect::<BTreeSet<_>>()
                != [buyer.to_owned(), result.pubkey.to_hex()]
                    .into_iter()
                    .collect()
            || b.text.trim().is_empty()
            || !b.attachments.is_empty()
        {
            return Err(Error("public inline content binding mismatch"));
        }
        (
            "inline",
            content.commitment().to_owned(),
            Some(b.text.clone()),
        )
    } else {
        if e.answer_envelope.is_some()
            || value(result, "content-id")?.is_some()
            || value(result, "content-commitment")?.is_some()
        {
            return Err(Error("extraneous public Git envelope"));
        }
        let delivery =
            gateway::parse_git_result_delivery(&crate::job_lifecycle::event_to_draft(result))
                .map_err(|_| Error("invalid public Git delivery"))?;
        ("fork", delivery.commit_oid().as_str().to_owned(), None)
    };
    let preimage = crate::receipt::ReceiptPreimage {
        protocol: crate::receipt::ReceiptProtocol::V2,
        job_hash: super::job_hash(&e.offer.id.to_hex())?,
        offer_id: e.offer.id.to_hex(),
        amount: offer.amount,
        unit: "sat".into(),
        buyer_pubkey: buyer.into(),
        seller_pubkey: e.claim.pubkey.to_hex(),
        delivery_integrity_hash: integrity.clone(),
        delivery_kind: kind.into(),
        exec_metadata_commitment: "none".into(),
        creq_hash: value(&e.claim, "creq")?.map(gateway::creq_hash_hex),
    };
    super::settlement::canonical_json(&preimage)?;
    Ok(super::evidence::VerifiedEvidence {
        routing_job_id: required(&e.offer, "job")?.into(),
        integrity,
        answer,
        preimage,
        contribution,
    })
}

pub fn validate_request(
    e: &super::evidence::PrivateEvidence,
    request: &crate::authorize_pay::AuthorizePayRequest,
    buyer: &str,
) -> Result<super::evidence::VerifiedEvidence> {
    let verified = validate_evidence(e, buyer)?;
    let p = &verified.preimage;
    let result = &e.result;
    let offer = validate_offer(&e.offer)?;
    if request.job_id != p.offer_id
        || request.result_id != result.id.to_hex()
        || request.seller_pubkey != p.seller_pubkey
        || request.job_hash != p.job_hash
        || request.amount_sats != p.amount
        || request.delivery_integrity_hash != p.delivery_integrity_hash
        || request.commit_oid != p.delivery_integrity_hash
        || request.inline_answer != verified.answer
        || request.creq_hash != p.creq_hash
        || request.seller_signature != required(result, "sig:seller")?
        || request.payment_mode != offer.payment_mode
        || request.repo != value(result, "repo")?.unwrap_or("")
        || request.branch != value(result, "branch")?.unwrap_or("")
    {
        return Err(Error(
            "accepted bind differs from signed public v2 evidence",
        ));
    }
    let mut expected = if offer.payment_mode.is_free() {
        vec![]
    } else {
        crate::job_lifecycle::verify_accepted_claim_creq(
            Some(required(&e.claim, "creq")?),
            &p.offer_id,
            p.amount,
        )
        .map_err(|_| Error("public invoice mismatch"))?
    };
    let mut actual = request.accepted_mints.clone();
    expected.sort();
    actual.sort();
    if expected != actual {
        return Err(Error("accepted public mint set changed"));
    }
    match (
        &verified.contribution,
        &request.contribution,
        request.job_class,
    ) {
        (None, None, crate::authorize_pay::JobClass::FromScratch) => {}
        (Some(pin), Some(bind), crate::authorize_pay::JobClass::Contribution)
            if pin.target_owner_pubkey == bind.target_owner_pubkey
                && pin.target_clone_url == bind.target_clone_url
                && pin.base_branch == bind.base_branch
                && pin.base_oid == bind.base_oid
                && value(result, "sig:seller-contribution")?
                    == Some(bind.tuple_signature.as_str()) => {}
        _ => return Err(Error("public contribution bind mismatch")),
    }
    Ok(verified)
}

pub fn project_local(home: &MaxplayerHome, id: &str, draft: EventDraft) -> Result<EventDraft> {
    let Some((mut ctx, offer)) = known(home, id)? else {
        return Ok(draft);
    };
    let selected = ctx.store.selection(id)?;
    let answer = if draft.kind == 3403
        && draft
            .tags
            .iter()
            .any(|t| t.0.as_slice() == ["delivery", "inline"])
    {
        let (claim, award) = selected
            .as_ref()
            .ok_or(Error("public inline selection unavailable"))?;
        Some(ctx.answer(&offer, claim, award, &draft.content)?)
    } else {
        None
    };
    project(
        &offer,
        draft,
        selected.as_ref().map(|(_, a)| a),
        answer.as_ref(),
    )
}
pub fn bind_receipt(
    home: &MaxplayerHome,
    seller: &str,
    id: &str,
    p: &mut crate::receipt::ReceiptPreimage,
    answer: Option<&str>,
) -> Result<()> {
    let Some((mut ctx, offer)) = known(home, id)? else {
        return Ok(());
    };
    let (claim, award) = ctx.selection(id)?;
    validate_selection(&offer, &claim, &award)?;
    if claim.pubkey.to_hex() != seller {
        return Err(Error("wrong public selected seller"));
    }
    p.protocol = crate::receipt::ReceiptProtocol::V2;
    p.job_hash = super::job_hash(id)?;
    if let Some(text) = answer {
        p.delivery_integrity_hash = ctx
            .answer(&offer, &claim, &award, text)?
            .commitment()
            .into();
    }
    super::settlement::canonical_json(p)?;
    Ok(())
}
pub fn publication(home: &MaxplayerHome, event: &Event) -> Result<()> {
    let id = required(event, "root")?;
    let (mut ctx, offer) = known(home, id)?.ok_or(Error("public publication context missing"))?;
    ctx.remember(&offer, event)
}
pub fn execution_hash(home: &MaxplayerHome, id: &str, task: &str, amount: u64) -> Result<String> {
    if known(home, id)?.is_some() {
        super::job_hash(id)
    } else {
        Ok(crate::job_lifecycle::job_hash_for_offer(id, task, amount))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::private_content::{builders::sign, evidence::PrivateEvidence};
    use nostr_sdk::{Keys, secp256k1::Message};
    fn keys(n: u8) -> Keys {
        Keys::parse(&format!("{n:064x}")).unwrap()
    }
    pub(crate) fn fixture(paid: bool, self_trade: bool) -> (PrivateEvidence, Keys) {
        let amount = if paid { 10 } else { 0 };
        let mode = if paid {
            gateway::PaymentMode::Sat
        } else {
            gateway::PaymentMode::None
        };
        let buyer = keys(1);
        let seller = if self_trade { keys(1) } else { keys(2) };
        let offer = sign(
            &buyer,
            offer_draft(
                gateway::OfferDraft::new(
                    "public task",
                    "text/plain",
                    amount,
                    2_000_000_000,
                    seller.public_key().to_hex(),
                )
                .with_payment_mode(mode)
                .accepting_delivery(["inline"])
                .to_event_draft(),
            )
            .unwrap(),
        )
        .unwrap();
        let creq = paid.then(|| {
            gateway::creq::build_seller_creq(
                &offer.id.to_hex(),
                amount,
                "sat",
                &["https://testnut.cashu.space".into()],
                &seller.public_key().to_hex(),
            )
            .unwrap()
        });
        let claim = sign(
            &seller,
            project(
                &offer,
                gateway::claim_draft(
                    &offer.id.to_hex(),
                    &buyer.public_key().to_hex(),
                    &seller.public_key().to_hex(),
                    creq.as_deref()
                        .map(gateway::ClaimPayment::Sat)
                        .unwrap_or(gateway::ClaimPayment::None),
                    &[],
                    &Default::default(),
                ),
                None,
                None,
            )
            .unwrap(),
        )
        .unwrap();
        let award = sign(
            &buyer,
            project(
                &offer,
                gateway::award_draft(
                    &offer.id.to_hex(),
                    &claim.id.to_hex(),
                    &buyer.public_key().to_hex(),
                    &seller.public_key().to_hex(),
                ),
                None,
                None,
            )
            .unwrap(),
        )
        .unwrap();
        let mut ctx = Context {
            store: super::super::store::ContentStore::in_memory().unwrap(),
        };
        ctx.select(&offer, &claim, &award).unwrap();
        let answer = ctx
            .answer(&offer, &claim, &award, "public answer\n")
            .unwrap();
        let p = crate::receipt::ReceiptPreimage {
            protocol: crate::receipt::ReceiptProtocol::V2,
            job_hash: super::super::job_hash(&offer.id.to_hex()).unwrap(),
            offer_id: offer.id.to_hex(),
            amount,
            unit: "sat".into(),
            buyer_pubkey: buyer.public_key().to_hex(),
            seller_pubkey: seller.public_key().to_hex(),
            delivery_integrity_hash: answer.commitment().into(),
            delivery_kind: "inline".into(),
            exec_metadata_commitment: "none".into(),
            creq_hash: creq.as_deref().map(gateway::creq_hash_hex),
        };
        let sig = seller
            .sign_schnorr(&Message::from_digest(p.digest_bytes()))
            .to_string();
        let result = sign(
            &seller,
            project(
                &offer,
                gateway::inline_result_draft(
                    &offer.id.to_hex(),
                    &buyer.public_key().to_hex(),
                    "text/plain",
                    amount,
                    &p.job_hash,
                    &sig,
                    "public answer\n",
                    &[],
                ),
                Some(&award),
                Some(&answer),
            )
            .unwrap(),
        )
        .unwrap();
        (
            PrivateEvidence {
                offer,
                claim,
                award,
                result,
                task_envelope: None,
                answer_envelope: Some(answer.envelope().into()),
            },
            buyer,
        )
    }
    #[test]
    fn public_v2_inline_binds_exact_public_envelope_and_signed_selection() {
        let (e, buyer) = fixture(false, false);
        let verified = validate_evidence(&e, &buyer.public_key().to_hex()).unwrap();
        assert_eq!(verified.answer.as_deref(), Some("public answer\n"));
        assert_ne!(
            verified.integrity,
            crate::receipt::result_content_hash_hex("public answer\n")
        );
        let authority = crate::payment::ReceiptAuthority {
            buyer: buyer.public_key(),
            seller: e.claim.pubkey,
        };
        authority
            .verify_seller_prepay_cosig(
                &verified.preimage,
                required(&e.result, "sig:seller").unwrap(),
                None,
            )
            .unwrap();
        let mut legacy = verified.preimage.clone();
        legacy.protocol = crate::receipt::ReceiptProtocol::V1;
        assert!(
            authority
                .verify_seller_prepay_cosig(
                    &legacy,
                    required(&e.result, "sig:seller").unwrap(),
                    None
                )
                .is_err()
        );
        assert_eq!(e.result.content, e.answer_envelope.clone().unwrap());
        for field in [
            "award",
            "job",
            "job-hash",
            "amount",
            "output",
            "content-id",
            "content-commitment",
        ] {
            let mut changed = e.clone();
            let mut draft = crate::job_lifecycle::event_to_draft(&e.result);
            let tag = draft
                .tags
                .iter_mut()
                .find(|t| t.first() == Some(field))
                .unwrap();
            tag.0[1] = if field == "output" {
                "different".into()
            } else if field == "amount" {
                "9".into()
            } else {
                "44".repeat(32)
            };
            changed.result = sign(&keys(2), draft).unwrap();
            assert!(
                validate_evidence(&changed, &buyer.public_key().to_hex()).is_err(),
                "{field}"
            );
        }
        let mut replaced = e.clone();
        replaced.answer_envelope = Some(e.answer_envelope.clone().unwrap() + " ");
        assert!(validate_evidence(&replaced, &buyer.public_key().to_hex()).is_err());
        let mut wrong_seller = e.clone();
        wrong_seller.result =
            sign(&keys(4), crate::job_lifecycle::event_to_draft(&e.result)).unwrap();
        assert!(validate_evidence(&wrong_seller, &buyer.public_key().to_hex()).is_err());
    }
    #[test]
    fn public_v2_preserves_existing_same_identity_trades() {
        for paid in [false, true] {
            let (e, buyer) = fixture(paid, true);
            assert_eq!(e.offer.pubkey, e.claim.pubkey);
            assert_eq!(participants(&e.claim).unwrap().len(), 1);
            assert_eq!(participants(&e.award).unwrap().len(), 1);
            let verified = validate_evidence(&e, &buyer.public_key().to_hex()).unwrap();
            crate::payment::ReceiptAuthority {
                buyer: buyer.public_key(),
                seller: buyer.public_key(),
            }
            .verify_seller_prepay_cosig(
                &verified.preimage,
                required(&e.result, "sig:seller").unwrap(),
                None,
            )
            .unwrap();
        }
    }
    #[test]
    fn public_v2_paid_bind_pins_invoice_mints_and_result_after_restart() {
        let (e, buyer) = fixture(true, false);
        let verified = validate_evidence(&e, &buyer.public_key().to_hex()).unwrap();
        let p = verified.preimage;
        let request = crate::authorize_pay::AuthorizePayRequest {
            private_evidence: Some(e.clone()),
            job_id: e.offer.id.to_hex(),
            result_id: e.result.id.to_hex(),
            job_class: crate::authorize_pay::JobClass::FromScratch,
            delivery_integrity_hash: p.delivery_integrity_hash.clone(),
            commit_oid: p.delivery_integrity_hash.clone(),
            job_hash: p.job_hash.clone(),
            seller_pubkey: e.claim.pubkey.to_hex(),
            amount_sats: p.amount,
            repo: String::new(),
            branch: String::new(),
            inline_answer: verified.answer,
            seller_signature: required(&e.result, "sig:seller").unwrap().into(),
            creq_hash: p.creq_hash.clone(),
            accepted_mints: vec!["https://testnut.cashu.space".into()],
            realized_mint: Some("https://testnut.cashu.space".into()),
            contribution: None,
            payment_mode: gateway::PaymentMode::Sat,
        };
        let restored: PrivateEvidence =
            serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        validate_request(&restored, &request, &buyer.public_key().to_hex()).unwrap();
        crate::payment::ReceiptAuthority {
            buyer: buyer.public_key(),
            seller: e.claim.pubkey,
        }
        .verify_seller_prepay_cosig(&p, &request.seller_signature, None)
        .unwrap();
        for field in ["mint", "invoice", "result", "amount"] {
            let mut changed = request.clone();
            match field {
                "mint" => changed.accepted_mints = vec!["https://other.example".into()],
                "invoice" => changed.creq_hash = Some("aa".repeat(32)),
                "result" => changed.result_id = "aa".repeat(32),
                _ => changed.amount_sats += 1,
            }
            assert!(
                validate_request(&restored, &changed, &buyer.public_key().to_hex()).is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn public_v2_restart_keeps_envelope_nonce_and_selected_claim() {
        let (e, _) = fixture(false, false);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("public.sqlite");
        let initial = {
            let mut ctx = Context {
                store: super::super::store::ContentStore::open(&path).unwrap(),
            };
            ctx.select(&e.offer, &e.claim, &e.award).unwrap();
            ctx.answer(&e.offer, &e.claim, &e.award, "stable").unwrap()
        };
        let mut ctx = Context {
            store: super::super::store::ContentStore::open(&path).unwrap(),
        };
        let (claim, award) = ctx.selection(&e.offer.id.to_hex()).unwrap();
        assert_eq!(claim.id, e.claim.id);
        assert_eq!(award.id, e.award.id);
        assert_eq!(
            ctx.answer(&e.offer, &claim, &award, "stable")
                .unwrap()
                .envelope(),
            initial.envelope()
        );
        assert!(ctx.answer(&e.offer, &claim, &award, "changed").is_err());
        let alternate = sign(
            &keys(3),
            project(
                &e.offer,
                gateway::claim_draft(
                    &e.offer.id.to_hex(),
                    &e.offer.pubkey.to_hex(),
                    &keys(3).public_key().to_hex(),
                    gateway::ClaimPayment::None,
                    &[],
                    &Default::default(),
                ),
                None,
                None,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(ctx.select(&e.offer, &alternate, &e.award).is_err());
    }
}
