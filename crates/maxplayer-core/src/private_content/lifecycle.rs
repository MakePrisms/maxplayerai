//! Adapt authenticated v2 content to the existing classifier/executor contracts.
//! Public metadata is never substituted for missing task or dispatch content.
use super::{
    Attachment, Contribution, Error, PreparedContent, Result,
    wire::{self, HostPolicy, PublicTask},
};
use crate::gateway::{ParsedOffer, PaymentMode};
use nostr_sdk::prelude::Event;

pub struct ResolvedOffer {
    pub offer: ParsedOffer,
    pub attachments: Vec<Attachment>,
    pub contribution: Option<Contribution>,
}
/// An encrypted task must already have passed recipient authentication. Check it again
/// against the exact signed carrier before constructing executable input. Open discovery
/// is the deliberately public exception, with the same dispatch semantics.
pub fn resolve_offer(
    event: &Event,
    content: Option<&PreparedContent>,
    recipient: &str,
    service: &str,
    host: &HostPolicy,
) -> Result<ResolvedOffer> {
    let tags = wire::validate_private(event, host)?;
    if event.kind.as_u16() != 3401 {
        return Err(Error("not an offer"));
    }
    let (text, output, dispatch, attachments, contribution) = match tags.required("discovery")? {
        "targeted" => {
            let task = content.ok_or(Error("private task is not yet available"))?;
            wire::bind_content(task, event, event, None, None, None, service, host)?;
            if !task.body().recipients.iter().any(|key| key == recipient) {
                return Err(Error("not an offer recipient"));
            }
            let body = task.body();
            (
                body.text.clone(),
                body.requested_output
                    .clone()
                    .ok_or(Error("missing original output"))?,
                body.dispatch
                    .clone()
                    .ok_or(Error("missing task dispatch"))?,
                body.attachments.clone(),
                body.contribution.clone(),
            )
        }
        "open" => {
            if content.is_some() {
                return Err(Error("open task cannot have a private replacement"));
            }
            let task = PublicTask::parse(tags.required("i")?, tags.required("job")?)?;
            wire::check_dispatch(&tags, &task.dispatch)?;
            (
                task.text,
                task.requested_output,
                task.dispatch,
                Vec::new(),
                task.contribution,
            )
        }
        _ => return Err(Error("invalid discovery mode")),
    };
    Ok(ResolvedOffer {
        offer: ParsedOffer {
            task: text,
            output,
            amount: wire::decimal(tags.required("amount")?)?,
            unit: "sat".into(),
            deadline_unix: wire::decimal(tags.required("param:deadline")?)?,
            seller_pubkey: tags.participants.iter().next().cloned(),
            requested_agent: dispatch.agent,
            requested_harness_family: dispatch.harness_family,
            requested_model: dispatch.harness_model,
            required_capabilities: dispatch.capabilities.unwrap_or_default(),
            payment_mode: if tags.get("param:payment") == Some("none") {
                PaymentMode::None
            } else {
                PaymentMode::Sat
            },
            accepts_delivery: tags
                .values("param:accepts-delivery")
                .unwrap_or_default()
                .to_vec(),
        },
        attachments,
        contribution,
    })
}

/// A selected execution chain, authenticated independently of delivery decryption.
/// Do not cache an award merely because its signature or its `p` tag is valid.
pub fn validate_selection(
    offer: &Event,
    claim: &Event,
    award: &Event,
    host: &HostPolicy,
) -> Result<()> {
    let o = wire::validate_private(offer, host)?;
    let c = wire::validate_private(claim, host)?;
    let a = wire::validate_private(award, host)?;
    let buyer = offer.pubkey.to_hex();
    let seller = claim.pubkey.to_hex();
    let parties = std::collections::BTreeSet::from([buyer.clone(), seller.clone()]);
    if offer.kind.as_u16() != 3401
        || claim.kind.as_u16() != 3402
        || award.kind.as_u16() != 3405
        || award.pubkey != offer.pubkey
        || a.get("root") != Some(offer.id.to_hex().as_str())
        || c.get("root") != a.get("root")
        || c.get("job") != o.get("job")
        || a.get("job") != o.get("job")
        || a.get("claim") != Some(claim.id.to_hex().as_str())
        || a.participants != parties
        || !c.participants.contains(&buyer)
        || c.participants.iter().any(|p| !parties.contains(p))
        || (!o.participants.is_empty() && !o.participants.contains(&seller))
    {
        return Err(Error("invalid selected execution chain"));
    }
    if o.get("param:payment") == Some("none") {
        if c.get("payment") != Some("none") {
            return Err(Error("claim payment mismatch"));
        }
    } else {
        #[cfg(feature = "wallet")]
        super::invoice::validate(
            c.required("creq")?,
            &offer.id.to_hex(),
            wire::decimal(o.required("amount")?)?,
            &seller,
            host,
        )?;
        #[cfg(not(feature = "wallet"))]
        return Err(Error("paid invoice validation unavailable"));
    }
    Ok(())
}

/// Public result evidence, checked before existing Git/inline artifact and money
/// verification. This does not replace any signature, sentinel or budget gate.
pub fn validate_result(
    offer: &Event,
    claim: &Event,
    award: &Event,
    result: &Event,
    host: &HostPolicy,
) -> Result<()> {
    validate_selection(offer, claim, award, host)?;
    let o = wire::validate_private(offer, host)?;
    let r = wire::validate_private(result, host)?;
    let buyer = offer.pubkey.to_hex();
    if result.kind.as_u16() != 3403
        || result.pubkey != claim.pubkey
        || r.get("root") != Some(offer.id.to_hex().as_str())
        || r.get("award") != Some(award.id.to_hex().as_str())
        || r.get("job") != o.get("job")
        || r.get("amount") != o.get("amount")
        || r.get("output") != o.get("output")
        || r.get("job-hash") != Some(super::job_hash(&offer.id.to_hex())?.as_str())
        || r.participants != std::collections::BTreeSet::from([buyer.clone()])
        || r.has("sig:seller-contribution") != o.has("job-class")
    {
        return Err(Error("result differs from selected trade"));
    }
    if r.get("delivery") == Some("inline") {
        if o.has("job-class")
            || !o
                .values("param:accepts-delivery")
                .is_some_and(|modes| modes.iter().any(|mode| mode == "inline"))
        {
            return Err(Error("inline delivery was not offered"));
        }
    } else {
        let repo = r.required("repo")?;
        if repo.strip_suffix(".git").unwrap_or(repo) != host.job_repo(&buyer, o.required("job")?)? {
            return Err(Error("result repository is outside this job"));
        }
        if o.has("repo") && (r.get("repo") != o.get("repo") || r.get("branch") != o.get("branch")) {
            return Err(Error("result differs from pinned delivery route"));
        }
    }
    Ok(())
}
