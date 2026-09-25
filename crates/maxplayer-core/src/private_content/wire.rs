//! Checked content binding to the existing signed trade lifecycle.
use super::{Binding, ContentType, Error, PreparedContent, Result};
pub use maxplayer_private_protocol::wire::*;
use nostr_sdk::prelude::Event;
use std::collections::BTreeSet;

/// Construct expected content binding only from authenticated, checked lifecycle events.
/// `seller` is target for task, claimant before award, selected seller after award.
pub fn bind_content(
    content: &PreparedContent,
    carrier: &Event,
    offer: &Event,
    award: Option<&Event>,
    claim: Option<&Event>,
    result: Option<&Event>,
    service: &str,
    host: &HostPolicy,
) -> Result<()> {
    let o = validate_private(offer, host)?;
    let c = validate_private(carrier, host)?;
    if offer.kind.as_u16() != 3401 || c.required("job")? != o.required("job")? {
        return Err(Error("wrong root offer"));
    }
    let kind = carrier.kind.as_u16();
    // Claim details validate their own signed invoice, even without a separate context argument.
    let claim = if kind == 3402 { Some(carrier) } else { claim };
    let author = carrier.pubkey.to_hex();
    let buyer = offer.pubkey.to_hex();
    let offer_id = offer.id.to_hex();
    let mut seller = author.clone();
    if kind == 3401 {
        if carrier.id != offer.id {
            return Err(Error("wrong task carrier"));
        }
        seller = o
            .participants
            .iter()
            .next()
            .ok_or(Error("task is not targeted"))?
            .clone();
    } else if c.get("root") != Some(offer_id.as_str()) {
        return Err(Error("wrong content root"));
    }
    if kind != 3401 && kind != 3407 && !c.participants.contains(&buyer) {
        return Err(Error("wrong buyer recipient"));
    }
    if award.is_none()
        && kind != 3401
        && !o.participants.is_empty()
        && !o.participants.contains(&author)
    {
        return Err(Error("wrong targeted candidate"));
    }
    let award_id = award.map(|e| e.id.to_hex());
    if let Some(a) = award {
        let a_tags = validate_private(a, host)?;
        let claim = claim.ok_or(Error("award requires signed claim"))?;
        let claim_tags = validate_private(claim, host)?;
        seller = claim.pubkey.to_hex();
        let parties: BTreeSet<_> = [buyer.clone(), seller.clone()].into_iter().collect();
        if a.kind.as_u16() != 3405
            || a.pubkey != offer.pubkey
            || a_tags.get("root") != Some(offer_id.as_str())
            || a_tags.get("claim") != Some(claim.id.to_hex().as_str())
            || a_tags.get("job") != o.get("job")
            || a_tags.participants != parties
            || claim.kind.as_u16() != 3402
            || claim_tags.get("root") != Some(offer_id.as_str())
            || claim_tags.get("job") != o.get("job")
            || (!o.participants.is_empty() && !o.participants.contains(&seller))
        {
            return Err(Error("invalid award chain"));
        }
    }
    if let Some(claim_event) = claim {
        let ct = validate_private(claim_event, host)?;
        if claim_event.kind.as_u16() != 3402
            || ct.get("root") != Some(offer_id.as_str())
            || ct.get("job") != o.get("job")
            || !ct.participants.contains(&buyer)
            || ct
                .participants
                .iter()
                .any(|p| p != &buyer && p != &claim_event.pubkey.to_hex())
        {
            return Err(Error("wrong claim participants or root"));
        }
        if o.get("param:payment") == Some("none") {
            if ct.get("payment") != Some("none") {
                return Err(Error("claim payment mismatch"));
            }
        } else {
            let invoice = ct.required("creq")?;
            #[cfg(feature = "wallet")]
            super::invoice::validate(
                invoice,
                &offer_id,
                decimal(o.required("amount")?)?,
                &claim_event.pubkey.to_hex(),
                host,
            )?;
            #[cfg(not(feature = "wallet"))]
            {
                let _ = invoice;
                return Err(Error("paid invoice validation unavailable"));
            }
        }
    }
    if kind == 3403 {
        if let Some(repo) = c.get("repo") {
            if repo.strip_suffix(".git").unwrap_or(repo)
                != host.job_repo(&buyer, o.required("job")?)?
            {
                return Err(Error("result repo belongs to another job"));
            }
        }
        if o.has("repo") && (c.get("repo") != o.get("repo") || c.get("branch") != o.get("branch")) {
            return Err(Error("result differs from offered delivery binding"));
        }
        if c.get("amount") != o.get("amount")
            || c.get("output") != o.get("output")
            || c.get("job-hash") != Some(super::job_hash(&offer_id)?.as_str())
        {
            return Err(Error("result trade binding mismatch"));
        }
        if c.has("sig:seller-contribution") != o.has("job-class") {
            return Err(Error("contribution result mismatch"));
        }
        if c.get("delivery") == Some("inline")
            && (o.has("job-class")
                || !o
                    .values("param:accepts-delivery")
                    .is_some_and(|v| v.iter().any(|v| v == "inline")))
        {
            return Err(Error("inline delivery not offered"));
        }
    }
    if kind == 3407 {
        let rejected = result.ok_or(Error("rejection requires exact result"))?;
        let rt = validate_private(rejected, host)?;
        if rejected.kind.as_u16() != 3403
            || rejected.pubkey.to_hex() != seller
            || rt.get("root") != Some(offer_id.as_str())
            || rt.get("award") != award_id.as_deref()
            || rt.get("job") != o.get("job")
            || rt.get("delivery") != Some("git")
            || c.get("reply") != Some(rejected.id.to_hex().as_str())
            || c.get("commit") != rt.get("commit")
            || !c.participants.contains(&seller)
        {
            return Err(Error("rejection result mismatch"));
        }
    }
    if kind != 3401 && c.participants.iter().any(|p| p != &buyer && p != &seller) {
        return Err(Error("extra carrier participant"));
    }
    let allowed = match kind {
        3401 => content.body().kind == ContentType::Task && author == buyer && award.is_none(),
        3402 => content.body().kind == ContentType::ClaimDetails && award.is_none(),
        3403 => {
            content.body().kind == ContentType::Answer
                && award.is_some()
                && author == seller
                && c.get("award") == award_id.as_deref()
        }
        3404 => {
            matches!(
                content.body().kind,
                ContentType::Progress | ContentType::Feedback
            ) && author == seller
                && c.get("award") == award_id.as_deref()
                && (award.is_some() || c.get("status") != Some("progress"))
        }
        3407 => content.body().kind == ContentType::Rejection && award.is_some() && author == buyer,
        _ => false,
    };
    if !allowed {
        return Err(Error("invalid content author, carrier or phase"));
    }
    if kind == 3401 {
        if content.body().contribution.is_some() != o.has("job-class") {
            return Err(Error("contribution marker mismatch"));
        }
        check_dispatch(
            &o,
            content
                .body()
                .dispatch
                .as_ref()
                .ok_or(Error("missing task dispatch"))?,
        )?;
    }
    content.validate_binding(&Binding {
        buyer: &buyer,
        seller: &seller,
        service,
        author: &author,
        job_id: o.required("job")?,
        offer_id: if kind == 3401 { None } else { Some(&offer_id) },
        award_id: award_id.as_deref(),
        message_id: c.required("content-id")?,
        commitment: c.required("content-commitment")?,
        kind: content.body().kind,
    })
}
