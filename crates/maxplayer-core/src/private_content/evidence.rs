//! Immutable v2 evidence retained in the existing pre-ACCEPT/pay bind. Rehydration
//! uses exact signed IDs and envelope bytes, never a latest-result query.
use super::{Error, PreparedContent, Result, runtime::Policy, wire};
use nostr_sdk::prelude::Event;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrivateEvidence {
    pub offer: Event,
    pub claim: Event,
    pub award: Event,
    pub result: Event,
    pub task_envelope: Option<String>,
    pub answer_envelope: Option<String>,
}
impl std::fmt::Debug for PrivateEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateEvidence")
            .field("offer", &self.offer.id)
            .field("result", &self.result.id)
            .finish_non_exhaustive()
    }
}
pub struct VerifiedEvidence {
    pub routing_job_id: String,
    pub integrity: String,
    pub answer: Option<String>,
    pub preimage: crate::receipt::ReceiptPreimage,
    pub contribution: Option<super::Contribution>,
}
impl PrivateEvidence {
    pub fn validate(&self, buyer: &str, policy: &Policy) -> Result<VerifiedEvidence> {
        #[cfg(feature = "wallet")]
        if super::public_v2::is_public(&self.offer) {
            return super::public_v2::validate_evidence(self, buyer);
        }
        if self.offer.pubkey.to_hex() != buyer {
            return Err(Error("wrong private evidence buyer"));
        }
        super::lifecycle::validate_result(
            &self.offer,
            &self.claim,
            &self.award,
            &self.result,
            &policy.host,
        )?;
        let o = wire::validate_private(&self.offer, &policy.host)?;
        let c = wire::validate_private(&self.claim, &policy.host)?;
        let r = wire::validate_private(&self.result, &policy.host)?;
        let task = self
            .task_envelope
            .as_deref()
            .map(PreparedContent::decode)
            .transpose()?;
        let resolved = super::lifecycle::resolve_offer(
            &self.offer,
            task.as_ref(),
            buyer,
            &policy.service,
            &policy.host,
        )?;
        let content = self
            .answer_envelope
            .as_deref()
            .map(PreparedContent::decode)
            .transpose()?;
        if r.has("content-id") != content.is_some() {
            return Err(Error(
                "private result content evidence missing or extraneous",
            ));
        }
        if let Some(content) = &content {
            wire::bind_content(
                content,
                &self.result,
                &self.offer,
                Some(&self.award),
                Some(&self.claim),
                None,
                &policy.service,
                &policy.host,
            )?;
        }
        let (kind, integrity, answer): (&str, String, Option<String>) =
            if r.get("delivery") == Some("inline") {
                let content = content.ok_or(Error("inline result has no immutable envelope"))?;
                if content.body().text.trim().is_empty() || !content.body().attachments.is_empty() {
                    return Err(Error("invalid inline answer artifact"));
                }
                (
                    "inline",
                    content.commitment().into(),
                    Some(content.body().text.clone()),
                )
            } else {
                ("fork", r.required("commit")?.into(), None)
            };
        let preimage = crate::receipt::ReceiptPreimage {
            protocol: crate::receipt::ReceiptProtocol::V2,
            job_hash: super::job_hash(&self.offer.id.to_hex())?,
            offer_id: self.offer.id.to_hex(),
            amount: wire::decimal(o.required("amount")?)?,
            unit: "sat".into(),
            buyer_pubkey: buyer.into(),
            seller_pubkey: self.claim.pubkey.to_hex(),
            delivery_integrity_hash: integrity.clone(),
            delivery_kind: kind.into(),
            exec_metadata_commitment: "none".into(),
            creq_hash: c
                .get("creq")
                .map(|creq| hex::encode(Sha256::digest(creq.as_bytes()))),
        };
        // Pin the exact canonical v2 shape before any signing/payment caller uses it.
        super::settlement::canonical_json(&preimage)?;
        Ok(VerifiedEvidence {
            routing_job_id: o.required("job")?.into(),
            integrity,
            answer,
            preimage,
            contribution: resolved.contribution,
        })
    }
}

#[cfg(feature = "wallet")]
impl PrivateEvidence {
    /// The sealed bind is a projection, not a second source of authority. Check
    /// every spend/artifact field against the original signed chain on each use.
    pub fn validate_request(
        &self,
        request: &crate::authorize_pay::AuthorizePayRequest,
        buyer: &str,
        policy: &Policy,
    ) -> Result<VerifiedEvidence> {
        if super::public_v2::is_public(&self.offer) {
            return super::public_v2::validate_request(self, request, buyer);
        }
        use crate::authorize_pay::JobClass;
        let verified = self.validate(buyer, policy)?;
        let p = &verified.preimage;
        let o = wire::validate_private(&self.offer, &policy.host)?;
        let c = wire::validate_private(&self.claim, &policy.host)?;
        let r = wire::validate_private(&self.result, &policy.host)?;
        if request.job_id != self.offer.id.to_hex()
            || request.result_id != self.result.id.to_hex()
            || request.seller_pubkey != p.seller_pubkey
            || request.job_hash != p.job_hash
            || request.amount_sats != p.amount
            || request.delivery_integrity_hash != p.delivery_integrity_hash
            || request.commit_oid != p.delivery_integrity_hash
            || request.inline_answer != verified.answer
            || request.creq_hash != p.creq_hash
            || request.seller_signature != r.required("sig:seller")?
            || request.payment_mode.is_free() != (o.get("param:payment") == Some("none"))
            || request.repo != r.get("repo").unwrap_or("")
            || request.branch != r.get("branch").unwrap_or("")
        {
            return Err(Error("accepted bind differs from signed private result"));
        }
        let mints = if let Some(raw) = c.get("creq") {
            super::invoice::validate(raw, &p.offer_id, p.amount, &p.seller_pubkey, &policy.host)?
                .mints
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut expected = mints;
        let mut actual = request.accepted_mints.clone();
        expected.sort();
        actual.sort();
        if actual != expected {
            return Err(Error("accepted mint set differs from signed claim"));
        }
        match (
            &verified.contribution,
            &request.contribution,
            request.job_class,
        ) {
            (None, None, JobClass::FromScratch) => {}
            (Some(pin), Some(bind), JobClass::Contribution)
                if pin.target_owner_pubkey == bind.target_owner_pubkey
                    && pin.target_clone_url == bind.target_clone_url
                    && pin.base_branch == bind.base_branch
                    && pin.base_oid == bind.base_oid
                    && r.get("sig:seller-contribution") == Some(bind.tuple_signature.as_str()) => {}
            _ => {
                return Err(Error(
                    "private contribution bind differs from offered inputs",
                ));
            }
        }
        Ok(verified)
    }
}

#[cfg(all(test, feature = "wallet"))]
pub(crate) fn inline_fixture() -> (
    PrivateEvidence,
    crate::authorize_pay::AuthorizePayRequest,
    nostr_sdk::Keys,
    Policy,
) {
    inline_fixture_for(true)
}
#[cfg(all(test, feature = "wallet"))]
pub(crate) fn inline_fixture_for(
    targeted: bool,
) -> (
    PrivateEvidence,
    crate::authorize_pay::AuthorizePayRequest,
    nostr_sdk::Keys,
    Policy,
) {
    inline_fixture_with_payment(targeted, false)
}
#[cfg(all(test, feature = "wallet"))]
pub(crate) fn inline_fixture_with_payment(
    targeted: bool,
    paid: bool,
) -> (
    PrivateEvidence,
    crate::authorize_pay::AuthorizePayRequest,
    nostr_sdk::Keys,
    Policy,
) {
    inline_fixture_with_deadline(targeted, paid, 2_000_000_000)
}
#[cfg(all(test, feature = "wallet"))]
pub(crate) fn inline_fixture_with_deadline(
    targeted: bool,
    paid: bool,
    deadline: u64,
) -> (
    PrivateEvidence,
    crate::authorize_pay::AuthorizePayRequest,
    nostr_sdk::Keys,
    Policy,
) {
    let amount = if paid { 10 } else { 0 };
    let payment_mode = if paid {
        crate::gateway::PaymentMode::Sat
    } else {
        crate::gateway::PaymentMode::None
    };
    use super::{ContentType, builders, carriers};
    use crate::{gateway, receipt};
    use nostr_sdk::prelude::*;
    use nostr_sdk::secp256k1::Message;
    let buyer = Keys::parse(&format!("{:064x}", 1)).unwrap();
    let seller = Keys::parse(&format!("{:064x}", 2)).unwrap();
    let service = Keys::parse(&format!("{:064x}", 3))
        .unwrap()
        .public_key()
        .to_hex();
    let policy = Policy {
        service: service.clone(),
        host: wire::HostPolicy {
            git_prefix: "https://git.example/git/".into(),
            accepted_mints: vec!["https://testnut.cashu.space".into()],
        },
    };
    let prepared = builders::prepare_offer(
        &buyer,
        &(if targeted {
            gateway::OfferDraft::new(
                "private task",
                "text/markdown",
                amount,
                deadline,
                seller.public_key().to_hex(),
            )
        } else {
            gateway::OfferDraft::untargeted(
                "deliberately public discovery",
                "text/markdown",
                amount,
                deadline,
            )
        })
        .with_payment_mode(payment_mode)
        .accepting_delivery(["inline"]),
        builders::OfferOptions {
            visibility: wire::Visibility::Private,
            category: wire::Output::Text,
            service: &service,
            job_id: &"33".repeat(32),
            attachments: Vec::new(),
            contribution: None,
        },
        &policy.host,
    )
    .unwrap();
    let offer = prepared.event;
    let creq = paid.then(|| {
        gateway::creq::build_seller_creq(
            &offer.id.to_hex(),
            amount,
            "sat",
            &policy.host.accepted_mints,
            &seller.public_key().to_hex(),
        )
        .unwrap()
    });
    let claim = builders::sign(
        &seller,
        carriers::project(
            &offer,
            &gateway::claim_draft(
                &offer.id.to_hex(),
                &buyer.public_key().to_hex(),
                &seller.public_key().to_hex(),
                creq.as_deref()
                    .map(gateway::ClaimPayment::Sat)
                    .unwrap_or(gateway::ClaimPayment::None),
                &["codex".into()],
                &crate::heartbeat::SeatCapability::default(),
            ),
            None,
            None,
            &policy.host,
        )
        .unwrap(),
    )
    .unwrap();
    let award = builders::sign(
        &buyer,
        carriers::project(
            &offer,
            &gateway::award_draft(
                &offer.id.to_hex(),
                &claim.id.to_hex(),
                &buyer.public_key().to_hex(),
                &seller.public_key().to_hex(),
            ),
            None,
            None,
            &policy.host,
        )
        .unwrap(),
    )
    .unwrap();
    let answer = carriers::prepare_content(
        &offer,
        Some(&claim),
        Some(&award),
        &seller.public_key().to_hex(),
        &service,
        ContentType::Answer,
        "exact private answer".into(),
        None,
        &policy.host,
    )
    .unwrap();
    let preimage = receipt::ReceiptPreimage {
        protocol: receipt::ReceiptProtocol::V2,
        job_hash: super::job_hash(&offer.id.to_hex()).unwrap(),
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
        .sign_schnorr(&Message::from_digest(preimage.digest_bytes()))
        .to_string();
    let result = builders::sign(
        &seller,
        carriers::project(
            &offer,
            &gateway::inline_result_draft(
                &offer.id.to_hex(),
                &buyer.public_key().to_hex(),
                "text/markdown",
                amount,
                &preimage.job_hash,
                &sig,
                &answer.body().text,
                &[],
            ),
            Some(&award),
            Some(&answer),
            &policy.host,
        )
        .unwrap(),
    )
    .unwrap();
    let evidence = PrivateEvidence {
        offer,
        claim,
        award,
        result,
        task_envelope: prepared.task.map(|t| t.envelope().to_owned()),
        answer_envelope: Some(answer.envelope().to_owned()),
    };
    let request = crate::authorize_pay::AuthorizePayRequest {
        private_evidence: Some(evidence.clone()),
        job_id: preimage.offer_id,
        result_id: evidence.result.id.to_hex(),
        job_class: crate::authorize_pay::JobClass::FromScratch,
        delivery_integrity_hash: preimage.delivery_integrity_hash.clone(),
        job_hash: preimage.job_hash,
        seller_pubkey: preimage.seller_pubkey,
        amount_sats: amount,
        repo: String::new(),
        branch: String::new(),
        commit_oid: preimage.delivery_integrity_hash,
        inline_answer: Some(answer.body().text.clone()),
        seller_signature: sig,
        creq_hash: creq.as_deref().map(gateway::creq_hash_hex),
        accepted_mints: if paid {
            policy.host.accepted_mints.clone()
        } else {
            Vec::new()
        },
        realized_mint: None,
        contribution: None,
        payment_mode,
    };
    (evidence, request, buyer, policy)
}

#[cfg(all(test, feature = "wallet"))]
mod tests {
    use super::*;
    #[test]
    fn immutable_private_bind_roundtrip_refuses_result_answer_amount_and_mode_substitution() {
        let (evidence, request, buyer, policy) = inline_fixture();
        let serialized = serde_json::to_vec(&evidence).unwrap();
        let restored: PrivateEvidence = serde_json::from_slice(&serialized).unwrap();
        let verified = restored
            .validate_request(&request, &buyer.public_key().to_hex(), &policy)
            .unwrap();
        assert_eq!(
            verified.preimage.protocol,
            crate::receipt::ReceiptProtocol::V2
        );
        assert_eq!(
            verified.preimage.canonical_json(),
            super::super::settlement::canonical_json(&verified.preimage).unwrap()
        );
        let authority = crate::payment::ReceiptAuthority {
            buyer: buyer.public_key(),
            seller: evidence.claim.pubkey,
        };
        authority
            .verify_seller_prepay_cosig(&verified.preimage, &request.seller_signature, None)
            .unwrap();
        let mut legacy = verified.preimage.clone();
        legacy.protocol = crate::receipt::ReceiptProtocol::V1;
        assert!(
            authority
                .verify_seller_prepay_cosig(&legacy, &request.seller_signature, None)
                .is_err()
        );
        for alter in [
            |r: &mut crate::authorize_pay::AuthorizePayRequest| r.result_id = "44".repeat(32),
            |r: &mut crate::authorize_pay::AuthorizePayRequest| {
                r.inline_answer = Some("replacement".into())
            },
            |r: &mut crate::authorize_pay::AuthorizePayRequest| r.amount_sats = 20,
            |r: &mut crate::authorize_pay::AuthorizePayRequest| {
                r.payment_mode = crate::gateway::PaymentMode::Sat
            },
            |r: &mut crate::authorize_pay::AuthorizePayRequest| {
                r.accepted_mints.push("https://mint.attacker".into())
            },
            |r: &mut crate::authorize_pay::AuthorizePayRequest| {
                r.delivery_integrity_hash =
                    crate::receipt::result_content_hash_hex(r.inline_answer.as_ref().unwrap())
            },
        ] {
            let mut changed = request.clone();
            alter(&mut changed);
            assert!(
                restored
                    .validate_request(&changed, &buyer.public_key().to_hex(), &policy)
                    .is_err()
            );
        }
        let mut changed = restored;
        changed.answer_envelope = None;
        assert!(
            changed
                .validate_request(&request, &buyer.public_key().to_hex(), &policy)
                .is_err()
        );
    }
}
