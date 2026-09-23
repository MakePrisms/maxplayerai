//! Closed public projection of existing lifecycle drafts. Content is prepared before
//! signatures (especially inline receipt signatures) and is immutable on retry.
use super::{
    BODY_SCHEMA, ContentBody, ContentType, Dispatch, Error, PreparedContent, Result,
    wire::{self, HostPolicy},
};
use crate::gateway::{EventDraft, TagSpec};
use nostr_sdk::prelude::Event;
use std::collections::BTreeSet;

pub fn prepare_content(
    offer: &Event,
    claim: Option<&Event>,
    award: Option<&Event>,
    author: &str,
    service: &str,
    kind: ContentType,
    text: String,
    dispatch: Option<Dispatch>,
    host: &HostPolicy,
) -> Result<PreparedContent> {
    let o = wire::validate_private(offer, host)?;
    if offer.kind.as_u16() != 3401 || kind == ContentType::Task {
        return Err(Error("wrong lifecycle content builder"));
    }
    let buyer = offer.pubkey.to_hex();
    let seller = if let Some(claim) = claim {
        claim.pubkey.to_hex()
    } else if author != buyer {
        author.into()
    } else {
        o.participants
            .iter()
            .next()
            .cloned()
            .ok_or(Error("content seller unavailable"))?
    };
    if let Some(award) = award {
        super::lifecycle::validate_selection(
            offer,
            claim.ok_or(Error("missing selected claim"))?,
            award,
            host,
        )?;
    }
    let recipients: BTreeSet<_> = [buyer, seller, service.into()].into_iter().collect();
    PreparedContent::new(ContentBody {
        schema: BODY_SCHEMA.into(),
        job_id: o.required("job")?.into(),
        offer_id: Some(offer.id.to_hex()),
        award_id: award.map(|event| event.id.to_hex()),
        message_id: super::random_id()?,
        kind,
        revision: 0,
        supersedes: None,
        author: author.into(),
        recipients: recipients.into_iter().collect(),
        text,
        requested_output: None,
        dispatch,
        attachments: Vec::new(),
        contribution: None,
    })
}

/// Construct a private carrier from an existing local draft. This is NOT a parser
/// for remote events: remote tags are rejected, never sanitized into validity.
/// Signature/context validation runs on the resulting signed event before enqueue.
pub fn project(
    offer: &Event,
    local: &EventDraft,
    award: Option<&Event>,
    content: Option<&PreparedContent>,
    host: &HostPolicy,
) -> Result<EventDraft> {
    #[cfg(feature = "wallet")]
    if super::public_v2::is_public(offer) {
        return super::public_v2::project(offer, local.clone(), award, content);
    }
    let o = wire::validate_private(offer, host)?;
    let kind = local.kind;
    if offer.kind.as_u16() != 3401
        || !matches!(kind, 3400 | 3402 | 3403 | 3404 | 3405 | 3406 | 3407)
    {
        return Err(Error("unsupported lifecycle projection"));
    }
    let mut tags = vec![
        TagSpec::new(["t", "maxplayer"]),
        TagSpec::new(["v", "2"]),
        TagSpec::new(["job", o.required("job")?]),
    ];
    let allowed: &[&str] = match kind {
        3400 => &[
            "e",
            "p",
            "job-hash",
            "amount",
            "mint",
            "sig",
            "creq-hash",
            "delivery_kind",
            "delivery_integrity_hash",
        ],
        3402 => &[
            "e",
            "p",
            "status",
            "creq",
            "payment",
            "agents",
            "harness_family",
            "capabilities",
        ],
        3403 => &[
            "e", "p", "amount", "job-hash", "sig", "delivery", "repo", "branch", "commit",
        ],
        3404 => &["e", "p", "status", "reason_code"],
        3405 | 3406 => &["e", "p", "status"],
        3407 => &["e", "p", "status", "reason_code", "commit"],
        _ => unreachable!(),
    };
    let mut participants = BTreeSet::new();
    for tag in &local.tags {
        let Some(name) = tag.first() else {
            return Err(Error("empty local lifecycle tag"));
        };
        if !allowed.contains(&name) {
            continue;
        }
        let mut row = tag.0.clone();
        match name {
            "p" => {
                if row.len() != 2 {
                    return Err(Error("invalid local participant"));
                }
                if !participants.insert(row[1].clone()) {
                    continue;
                }
            }
            "status" => {
                row.truncate(2);
            }
            "agents" | "harness_family" | "capabilities" => {
                let enums: &[&str] = match name {
                    "agents" => &["claude", "codex", "cursor"],
                    "harness_family" => &["claude-code", "codex", "cursor", "goose"],
                    _ => &["node", "python", "rust"],
                };
                let mut values: Vec<_> = row
                    .into_iter()
                    .skip(1)
                    .filter(|v| enums.contains(&v.as_str()))
                    .collect();
                values.sort();
                values.dedup();
                if values.is_empty() {
                    continue;
                }
                row = vec![name.into()];
                row.extend(values);
            }
            "reason_code" => {
                let value = row.get(1).map(String::as_str).unwrap_or("");
                let allowed = if kind == 3407 {
                    &[
                        "verify_not_descendant",
                        "verify_tip_mismatch",
                        "verify_content_refused",
                        "verify_no_sentinel",
                        "verify_reserved_path",
                        "verify_attestation_missing",
                        "verify_attestation_mismatch",
                        "checks_failed",
                        "other",
                    ][..]
                } else {
                    &[
                        "below_rate",
                        "unsupported_version",
                        "mint_incompatible",
                        "at_capacity",
                        "execution_failed",
                        "delivery_failed",
                        "no_sentinel",
                        "other",
                    ][..]
                };
                row = vec![
                    name.into(),
                    if allowed.contains(&value) {
                        value.into()
                    } else {
                        "other".into()
                    },
                ];
            }
            _ => {}
        }
        tags.push(TagSpec(row));
    }
    if kind == 3403 {
        tags.push(TagSpec::new(["output", o.required("output")?]));
    }
    if matches!(kind, 3403 | 3404) {
        if let Some(award) = award {
            tags.push(TagSpec::new(["award", &award.id.to_hex()]));
        } else if kind == 3403 {
            return Err(Error("result requires award"));
        }
    }
    if matches!(kind, 3404 | 3407) && !tags.iter().any(|t| t.first() == Some("reason_code")) {
        tags.push(TagSpec::new(["reason_code", "other"]));
    }
    if let Some(content) = content {
        if content.body().offer_id.as_deref() != Some(offer.id.to_hex().as_str())
            || content.body().job_id != o.required("job")?
        {
            return Err(Error("wrong content projection root"));
        }
        tags.extend([
            TagSpec::new(["content-id", &content.body().message_id]),
            TagSpec::new(["content-commitment", content.commitment()]),
        ]);
    } else if !local.content.is_empty()
        || local
            .tags
            .iter()
            .any(|tag| tag.first() == Some("status") && tag.0.len() > 2)
    {
        return Err(Error("local text requires encrypted content"));
    }
    // Numeric usage echoes are optional, seller-claimed and not receipt cosigned.
    // Omit the whole block until the caller can prove exact RESULT echo parity.
    Ok(EventDraft::new(kind, tags, ""))
}
