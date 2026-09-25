//! Signed offer construction from the existing dispatch request. The original output
//! and custom model/preset filters stay in the task; public tags use explicit categories.
use super::{
    Attachment, BODY_SCHEMA, ContentBody, ContentType, Contribution, Dispatch, Error,
    PreparedContent, Result,
    wire::{HostPolicy, Output, PublicTask, Visibility},
};
use crate::gateway::{EventDraft, OfferDraft, TagSpec};
use nostr_sdk::prelude::{Event, EventBuilder, Keys, Kind, Tag};
pub struct PreparedOffer {
    pub event: Event,
    pub task: Option<PreparedContent>,
}
pub struct OfferOptions<'a> {
    pub visibility: Visibility,
    pub category: Output,
    pub service: &'a str,
    pub job_id: &'a str,
    pub attachments: Vec<Attachment>,
    pub contribution: Option<Contribution>,
}
pub fn prepare_offer(
    keys: &Keys,
    request: &OfferDraft,
    options: OfferOptions<'_>,
    host: &HostPolicy,
) -> Result<PreparedOffer> {
    super::require_hex(options.job_id, 32)?;
    super::require_hex(options.service, 32)?;
    let targeted = request.seller_pubkey.is_some();
    if let Some(target) = &request.seller_pubkey {
        super::require_hex(target, 32)?;
    }
    let mut capabilities = request.required_capabilities.clone();
    capabilities.sort();
    capabilities.dedup();
    let dispatch = Dispatch {
        agent: request.requested_agent.clone().filter(|a| a != "any"),
        harness_family: request.requested_harness_family.clone(),
        harness_model: request.requested_model.clone(),
        capabilities: (!capabilities.is_empty()).then_some(capabilities),
    };
    dispatch.validate()?;
    if dispatch.harness_model.is_some() && dispatch.agent.is_none() {
        return Err(Error("model requires an agent preset"));
    }
    let discovery = if targeted { "targeted" } else { "open" };
    if options.visibility == Visibility::Public {
        if !options.attachments.is_empty() || options.contribution.is_some() {
            return Err(Error("public input upload is not part of private hosting"));
        }
        let mut draft = request.to_event_draft();
        for tag in &mut draft.tags {
            if tag.first() == Some("v") {
                tag.0[1] = "2".into();
            }
        }
        draft.tags.extend([
            TagSpec::new(["job", options.job_id]),
            TagSpec::new(["visibility", "public"]),
            TagSpec::new(["discovery", discovery]),
        ]);
        return Ok(PreparedOffer {
            event: sign(keys, draft)?,
            task: None,
        });
    }
    let mut tags = vec![
        TagSpec::new(["t", "maxplayer"]),
        TagSpec::new(["v", "2"]),
        TagSpec::new(["job", options.job_id]),
        TagSpec::new(["visibility", "private"]),
        TagSpec::new(["discovery", discovery]),
        TagSpec::new(["output", options.category.as_str()]),
        TagSpec::new(["amount", &request.amount_sats.to_string(), "sat"]),
        TagSpec::new(["param", "deadline", &request.deadline_unix.to_string()]),
    ];
    if request.payment_mode.is_free() {
        tags.push(TagSpec::new(["param", "payment", "none"]));
    }
    if !request.accepts_delivery.is_empty() {
        let mut modes = request.accepts_delivery.clone();
        modes.sort();
        modes.dedup();
        let mut row = vec!["param".into(), "accepts-delivery".into()];
        row.extend(modes);
        tags.push(TagSpec(row));
    }
    if let Some(agent) = &dispatch.agent {
        if ["claude", "codex", "cursor"].contains(&agent.as_str()) {
            tags.push(TagSpec::new(["param", "agent", agent]));
        }
    }
    if let Some(family) = &dispatch.harness_family {
        tags.push(TagSpec::new(["param", "harness_family", family]));
    }
    if let Some(capabilities) = &dispatch.capabilities {
        let mut row = vec!["param".into(), "capability".into()];
        row.extend(capabilities.iter().cloned());
        tags.push(TagSpec(row));
    }
    if options.contribution.is_some() {
        tags.push(TagSpec::new(["job-class", "contribution"]));
    }
    let task = if let Some(seller) = &request.seller_pubkey {
        tags.push(TagSpec::new(["p", seller]));
        let mut recipients = vec![
            keys.public_key().to_hex(),
            seller.clone(),
            options.service.into(),
        ];
        recipients.sort();
        recipients.dedup();
        let body = ContentBody {
            schema: BODY_SCHEMA.into(),
            job_id: options.job_id.into(),
            offer_id: None,
            award_id: None,
            message_id: super::random_id()?,
            kind: ContentType::Task,
            revision: 0,
            supersedes: None,
            author: keys.public_key().to_hex(),
            recipients,
            text: request.task.clone(),
            requested_output: Some(request.output.clone()),
            dispatch: Some(dispatch),
            attachments: options.attachments,
            contribution: options.contribution,
        };
        let content = PreparedContent::new(body)?;
        tags.extend([
            TagSpec::new(["content-id", &content.body().message_id]),
            TagSpec::new(["content-commitment", content.commitment()]),
        ]);
        Some(content)
    } else {
        if !options.attachments.is_empty() {
            return Err(Error("confidential inputs require a targeted offer"));
        }
        let task = PublicTask {
            schema: "maxplayer.public-task.v2".into(),
            text: request.task.clone(),
            requested_output: request.output.clone(),
            dispatch,
            contribution: options.contribution,
        };
        let json =
            serde_json::to_string(&task).map_err(|_| Error("public task serialization failed"))?;
        PublicTask::parse(&json, options.job_id)?;
        tags.push(TagSpec::new(["i", &json]));
        None
    };
    let event = sign(keys, EventDraft::new(3401, tags, ""))?;
    super::wire::validate_private(&event, host)?;
    if let Some(task) = &task {
        super::wire::bind_content(
            task,
            &event,
            &event,
            None,
            None,
            None,
            options.service,
            host,
        )?;
    }
    Ok(PreparedOffer { event, task })
}
pub fn sign(keys: &Keys, draft: EventDraft) -> Result<Event> {
    let tags: std::result::Result<Vec<_>, _> =
        draft.tags.into_iter().map(|t| Tag::parse(t.0)).collect();
    EventBuilder::new(Kind::from(draft.kind), draft.content)
        .allow_self_tagging()
        .tags(tags.map_err(|_| Error("invalid event tag"))?)
        .sign_with_keys(keys)
        .map_err(|_| Error("could not sign content carrier"))
}
