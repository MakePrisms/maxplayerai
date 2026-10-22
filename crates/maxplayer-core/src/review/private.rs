//! Private execution reviews: signed inner messages inside recipient-only NIP-59
//! wrappers. The public transport is never used for a private job, including errors.
use super::*;
use crate::{home::MaxplayerHome, private_content as pc};
use nostr_sdk::prelude::*;

#[path = "delivery_budget.rs"]
mod delivery_budget;
#[path = "request_wire.rs"]
mod request_wire;
pub(crate) use delivery_budget::seller_delivery_budget;

const DOMAIN: &str = "maxplayer-private-review-v1";
const MAX_MESSAGE: usize = 60 * 1024;
#[derive(Clone, Deserialize)]
#[serde(try_from = "request_wire::RequestWire")]
pub struct Request {
    pub subject: Subject,
    pub offer: Event,
    pub task_envelope: Option<String>,
    pub delivery: Option<pc::evidence::PrivateEvidence>,
    /// Session follow-up subject (session-jobs §9): present iff `subject.kind` is 3412.
    pub followup: Option<FollowUpEvidence>,
}

/// A private follow-up review's evidence: the selected claim/award and the buyer's signed
/// follow-up carriers with their encrypted bodies, oldest first; the LAST is the subject. The
/// reviewer re-authenticates every carrier against the signed offer/claim/award chain, so neither
/// party can put words in the other's mouth.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowUpEvidence {
    pub claim: Event,
    pub award: Event,
    pub followups: Vec<FollowUpItem>,
    /// Earlier deliveries, oldest first: the selected seller's signed RESULT carriers, each reduced
    /// by the reviewer to one line (turn, status, commit) from its signed public tags; the reviewer
    /// never reads their content. Absent (empty) ⇒ no `deliveries` key on the wire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<DeliveryItem>,
}
/// One earlier delivery: the signed RESULT and, for turn n >= 2, the buyer's signed follow-up
/// carrier it replies to (carrier only: it is where the turn number is authenticated).
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryItem {
    pub result: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followup: Option<Event>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowUpItem {
    pub carrier: Event,
    pub envelope: String,
}
impl FollowUpEvidence {
    /// The follow-up review context (§9) built from authenticated evidence only: the original task
    /// and every carried follow-up in full, each carried earlier delivery as one line. Same
    /// renderer and size bound as the public lane.
    pub fn context(
        &self,
        offer: &Event,
        task: &str,
        policy: &pc::runtime::Policy,
    ) -> Result<String, String> {
        let followups = self.verify(offer, policy)?;
        let deliveries = self.delivery_lines(offer, policy)?;
        Ok(crate::session::followup_review_context_numbered(
            task,
            &followups,
            &deliveries,
        ))
    }

    /// One line per carried delivery, oldest first, after authenticating each RESULT against the
    /// selected chain (seller-signed, this award) and, for turn n >= 2, its buyer-signed follow-up.
    /// Every delivery precedes the follow-up under review; turn numbers strictly increase.
    pub fn delivery_lines(
        &self,
        offer: &Event,
        policy: &pc::runtime::Policy,
    ) -> Result<Vec<crate::session::DeliveryLine>, String> {
        let bad = |_| String::from("invalid_subject");
        let current = self
            .followups
            .last()
            .and_then(|i| pc::wire::validate_private(&i.carrier, &policy.host).ok())
            .and_then(|t| t.get("turn").and_then(|n| n.parse::<u32>().ok()))
            .ok_or("invalid_subject")?;
        if self.deliveries.len() > 64 {
            return Err("invalid_subject".into());
        }
        let mut out: Vec<crate::session::DeliveryLine> = Vec::new();
        for item in &self.deliveries {
            pc::lifecycle::validate_result(
                offer,
                &self.claim,
                &self.award,
                &item.result,
                &policy.host,
            )
            .map_err(bad)?;
            let r = pc::wire::validate_private(&item.result, &policy.host).map_err(bad)?;
            let turn = match (r.get("reply"), &item.followup) {
                (None, None) => 1,
                (Some(reply), Some(f)) => {
                    let t = pc::wire::validate_private(f, &policy.host).map_err(bad)?;
                    if f.kind.as_u16() != SESSION_FOLLOWUP_KIND
                        || f.pubkey != offer.pubkey
                        || f.id.to_hex() != reply
                        || t.get("root") != Some(offer.id.to_hex().as_str())
                        || t.get("job") != r.get("job")
                        || t.get("status") != Some("turn")
                    {
                        return Err("invalid_subject".into());
                    }
                    t.get("turn")
                        .and_then(|n| n.parse::<u32>().ok())
                        .filter(|n| *n >= 2)
                        .ok_or("invalid_subject")?
                }
                _ => return Err("invalid_subject".into()),
            };
            let status = match r.get("turn_status") {
                None => crate::session::TurnStatus::Done,
                Some(s) => crate::session::TurnStatus::parse(s).ok_or("invalid_subject")?,
            };
            if turn >= current || out.last().is_some_and(|d| d.turn >= turn) {
                return Err("invalid_subject".into());
            }
            out.push(crate::session::DeliveryLine {
                turn,
                status,
                commit: r.get("commit").map(str::to_owned),
            });
        }
        Ok(out)
    }

    /// `(turn, text)` per follow-up, oldest first, after authenticating each one.
    pub fn verify(
        &self,
        offer: &Event,
        policy: &pc::runtime::Policy,
    ) -> Result<Vec<(u32, String)>, String> {
        pc::lifecycle::validate_selection(offer, &self.claim, &self.award, &policy.host)
            .map_err(|_| "invalid_subject")?;
        if self.followups.is_empty() || self.followups.len() > 64 {
            return Err("invalid_subject".into());
        }
        let mut out: Vec<(u32, String)> = Vec::new();
        for item in &self.followups {
            let tags = pc::wire::validate_private(&item.carrier, &policy.host)
                .map_err(|_| "invalid_subject")?;
            let turn: u32 = tags
                .get("turn")
                .and_then(|t| t.parse().ok())
                .ok_or("invalid_subject")?;
            if item.carrier.kind.as_u16() != SESSION_FOLLOWUP_KIND
                || item.carrier.pubkey != offer.pubkey
                || tags.get("status") != Some("turn")
                || out.last().is_some_and(|(n, _)| *n >= turn)
            {
                return Err("invalid_subject".into());
            }
            let content =
                pc::PreparedContent::decode(&item.envelope).map_err(|_| "invalid_subject")?;
            pc::wire::bind_content(
                &content,
                &item.carrier,
                offer,
                Some(&self.award),
                Some(&self.claim),
                None,
                &policy.service,
                &policy.host,
            )
            .map_err(|_| "invalid_subject")?;
            out.push((turn, content.body().text.clone()));
        }
        Ok(out)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    domain: String,
    event: Event,
}

pub fn is_private(event: &Event) -> bool {
    event
        .tags
        .iter()
        .any(|t| t.as_slice() == ["visibility", "private"])
}

fn private_marker(event: &Event) -> bool {
    let tags: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("visibility"))
        .collect();
    tags.len() == 1 && tags[0].as_slice() == ["visibility", "private"]
}

pub async fn wrap(keys: &Keys, recipient: PublicKey, event: &Event) -> Result<Event, String> {
    event
        .verify()
        .map_err(|_| "review: invalid inner signature")?;
    if event.pubkey != keys.public_key()
        || !private_marker(event)
        || !matches!(event.kind.as_u16(), REVIEW_REQUEST_KIND | REVIEW_KIND)
    {
        return Err("review: invalid private message".into());
    }
    let body = serde_json::to_string(&Message {
        domain: DOMAIN.into(),
        event: event.clone(),
    })
    .map_err(|_| "review: encoding failed")?;
    check_size("message", body.len(), MAX_MESSAGE)?;
    pc::transport::wrap_checked(keys, recipient, body)
        .await
        .map_err(|error| match error {
            pc::transport::WrapError::Size { layer, actual, allowed } =>
                format!("review: permanent input error: private {layer} is {actual} bytes; limit {allowed} bytes; retrying unchanged input cannot help"),
            pc::transport::WrapError::Other(message) => format!("review: {message}"),
        })
}
fn check_size(layer: &str, actual: usize, allowed: usize) -> Result<(), String> {
    if actual > allowed {
        return Err(format!(
            "review: permanent input error: private {layer} is {actual} bytes; limit {allowed} bytes; retrying unchanged input cannot help"
        ));
    }
    Ok(())
}

pub async fn unwrap(keys: &Keys, outer: &Event) -> Result<Event, String> {
    let (author, plaintext) = pc::transport::unwrap_message(keys, outer)
        .await
        .map_err(|e| e.to_string())?;
    if plaintext.len() > MAX_MESSAGE {
        return Err("review: encrypted message too large".into());
    }
    pc::strict_json::validate(plaintext.as_bytes()).map_err(|e| e.to_string())?;
    let message: Message =
        serde_json::from_str(&plaintext).map_err(|_| "review: unrelated private message")?;
    let event = message.event;
    event
        .verify()
        .map_err(|_| "review: invalid inner signature")?;
    if message.domain != DOMAIN
        || event.pubkey != author
        || !private_marker(&event)
        || !matches!(event.kind.as_u16(), REVIEW_REQUEST_KIND | REVIEW_KIND)
    {
        return Err("review: invalid private message".into());
    }
    Ok(event)
}

impl Request {
    /// Revalidate the signed input and requester before classifier work or recipients
    /// are selected. Caller supplies deployment policy, never policy from the request.
    pub fn validate(
        &self,
        requester: &PublicKey,
        policy: &pc::runtime::Policy,
    ) -> Result<Vec<String>, String> {
        self.subject.validate().map_err(|_| "invalid_subject")?;
        if self.subject.offer != self.offer.id.to_hex() || !is_private(&self.offer) {
            return Err("invalid_subject".into());
        }
        let tags =
            pc::wire::validate_private(&self.offer, &policy.host).map_err(|_| "invalid_subject")?;
        let task = self
            .task_envelope
            .as_deref()
            .map(pc::PreparedContent::decode)
            .transpose()
            .map_err(|_| "invalid_subject")?;
        pc::lifecycle::resolve_offer(
            &self.offer,
            task.as_ref(),
            &policy.service,
            &policy.service,
            &policy.host,
        )
        .map_err(|_| "invalid_subject")?;
        let buyer = self.offer.pubkey;
        let mut recipients = std::collections::BTreeSet::from([
            buyer.to_hex(),
            policy.service.clone(),
            requester.to_hex(),
        ]);
        if self.subject.kind == SESSION_FOLLOWUP_KIND {
            let f = self.followup.as_ref().ok_or("invalid_subject")?;
            if self.delivery.is_some()
                || f.followups.last().map(|i| i.carrier.id.to_hex()).as_deref()
                    != Some(self.subject.event.as_str())
            {
                return Err("invalid_subject".into());
            }
            f.verify(&self.offer, policy)?;
            f.delivery_lines(&self.offer, policy)?;
            if *requester != buyer && *requester != f.claim.pubkey {
                return Err("unauthorized_request".into());
            }
            recipients.insert(f.claim.pubkey.to_hex());
        } else if self.followup.is_some() {
            return Err("invalid_subject".into());
        } else if self.subject.kind == JOB_OFFER_KIND {
            if self.delivery.is_some()
                || (!tags.participants.is_empty()
                    && *requester != buyer
                    && !tags.participants.contains(&requester.to_hex()))
            {
                return Err("unauthorized_request".into());
            }
            recipients.extend(tags.participants);
        } else {
            let evidence = self.delivery.as_ref().ok_or("invalid_subject")?;
            if evidence.offer != self.offer
                || evidence.task_envelope != self.task_envelope
                || evidence.result.id.to_hex() != self.subject.event
            {
                return Err("invalid_subject".into());
            }
            evidence
                .validate(&buyer.to_hex(), policy)
                .map_err(|_| "invalid_subject")?;
            let rt = pc::wire::validate_private(&evidence.result, &policy.host)
                .map_err(|_| "invalid_subject")?;
            if rt.get("commit") != self.subject.commit.as_deref() {
                return Err("invalid_subject".into());
            }
            if *requester != buyer && *requester != evidence.result.pubkey {
                return Err("unauthorized_request".into());
            }
            recipients.insert(evidence.result.pubkey.to_hex());
        }
        Ok(recipients.into_iter().collect())
    }
    pub fn draft(&self, reviewer: &str) -> Result<EventDraft, String> {
        let mut draft = request_draft(&self.subject, reviewer)?;
        draft.tags.push(TagSpec::new(["visibility", "private"]));
        draft.content = serde_json::to_string(self).map_err(|_| "review: request encoding")?;
        check_size("request", draft.content.len(), MAX_MESSAGE)?;
        Ok(draft)
    }
}

pub fn offer_request(
    home: &MaxplayerHome,
    seller: &str,
    job: &str,
) -> Result<Option<Request>, String> {
    let Some((mut ctx, offer)) =
        crate::seller_node::privacy::known(home, seller, job).map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let tags = pc::wire::validate_private(&offer, &ctx.policy.host).map_err(|e| e.to_string())?;
    let task_envelope = if tags.get("discovery") == Some("targeted") {
        Some(
            ctx.accept_content(&offer, &offer, None, None, None, Timestamp::now().as_secs())
                .map_err(|e| e.to_string())?
                .envelope()
                .to_owned(),
        )
    } else {
        None
    };
    Ok(Some(Request {
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
    }))
}

/// The seller's private follow-up review request (session-jobs §9): the selected claim/award, the
/// current follow-up, then as many `earlier` follow-ups (oldest first) as fit the message bound,
/// newest kept first, then as many earlier `deliveries` (`(result id, follow-up id it answers)`,
/// oldest first; `None` for turn 1) as still fit, newest kept first. Buyer messages take the
/// bound before deliveries. Everything comes from the seller's authenticated content store; the
/// reviewer re-authenticates it and reduces each delivery to one line.
pub fn followup_request(
    home: &MaxplayerHome,
    seller: &str,
    job: &str,
    earlier: &[String],
    followup: &str,
    deliveries: &[(String, Option<String>)],
) -> Result<Request, String> {
    let (mut ctx, offer) = crate::seller_node::privacy::known(home, seller, job)
        .map_err(|e| e.to_string())?
        .ok_or("review: private offer context unavailable")?;
    let (claim, award) = ctx
        .store
        .selection(job)
        .map_err(|e| e.to_string())?
        .ok_or("review: private selection unavailable")?;
    let tags = pc::wire::validate_private(&offer, &ctx.policy.host).map_err(|e| e.to_string())?;
    let task_envelope = if tags.get("discovery") == Some("targeted") {
        Some(
            ctx.accept_content(&offer, &offer, None, None, None, Timestamp::now().as_secs())
                .map_err(|e| e.to_string())?
                .envelope()
                .to_owned(),
        )
    } else {
        None
    };
    let item = |id: &str| -> Result<FollowUpItem, String> {
        let carrier = ctx
            .store
            .event(id)
            .map_err(|e| e.to_string())?
            .ok_or("review: follow-up carrier unavailable")?;
        let t =
            pc::wire::validate_private(&carrier, &ctx.policy.host).map_err(|e| e.to_string())?;
        let content = ctx
            .store
            .get(
                t.required("job").map_err(|e| e.to_string())?,
                t.required("content-id").map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?
            .ok_or("review: follow-up content unavailable")?;
        Ok(FollowUpItem {
            carrier,
            envelope: content.envelope().to_owned(),
        })
    };
    let mut request = Request {
        subject: Subject {
            offer: job.into(),
            event: followup.into(),
            kind: SESSION_FOLLOWUP_KIND,
            commit: None,
        },
        offer,
        task_envelope,
        delivery: None,
        followup: Some(FollowUpEvidence {
            claim,
            award,
            followups: vec![item(followup)?],
            deliveries: Vec::new(),
        }),
    };
    let earlier: Vec<FollowUpItem> = earlier.iter().filter_map(|id| item(id).ok()).collect();
    let deliveries: Vec<DeliveryItem> = deliveries
        .iter()
        .filter_map(|(result, answers)| {
            let result = ctx.store.event(result).ok().flatten()?;
            let followup = match answers {
                None => None,
                Some(id) => Some(ctx.store.event(id).ok().flatten()?),
            };
            Some(DeliveryItem { result, followup })
        })
        .collect();
    fill_followup_evidence(&mut request, earlier, deliveries, MAX_MESSAGE - 2048);
    Ok(request)
}

/// Fill a follow-up request under the message bound (§9): earlier buyer follow-ups first (newest
/// kept first), then earlier deliveries with whatever room is left (newest kept first). Both lists
/// arrive oldest first; whatever is kept stays oldest first. The current follow-up is never cut.
fn fill_followup_evidence(
    request: &mut Request,
    earlier: Vec<FollowUpItem>,
    deliveries: Vec<DeliveryItem>,
    bound: usize,
) {
    let fits =
        |request: &Request| serde_json::to_string(request).map_or(usize::MAX, |s| s.len()) <= bound;
    for item in earlier.into_iter().rev() {
        request.followup.as_mut().expect("follow-up request").followups.insert(0, item);
        if !fits(request) {
            request.followup.as_mut().expect("follow-up request").followups.remove(0);
            break;
        }
    }
    for item in deliveries.into_iter().rev() {
        request.followup.as_mut().expect("follow-up request").deliveries.insert(0, item);
        if !fits(request) {
            request.followup.as_mut().expect("follow-up request").deliveries.remove(0);
            break;
        }
    }
}

/// Keys remain in their existing custody site. Seller calls cross only the signer actor.
pub enum Identity<'a> {
    Buyer(&'a Keys),
    Seller(&'a crate::seller_node::signer::SignerHandle),
}
impl Identity<'_> {
    fn public_key(&self) -> Result<PublicKey, String> {
        match self {
            Self::Buyer(keys) => Ok(keys.public_key()),
            Self::Seller(s) => {
                PublicKey::from_hex(s.public_key_hex()).map_err(|_| "review: invalid signer".into())
            }
        }
    }
    async fn request(&self, draft: EventDraft, recipient: PublicKey) -> Result<Event, String> {
        match self {
            Self::Buyer(keys) => {
                let event = crate::gateway::nostr::event_builder(&draft)
                    .map_err(|e| e.to_string())?
                    .sign_with_keys(keys)
                    .map_err(|e| e.to_string())?;
                wrap(keys, recipient, &event).await
            }
            Self::Seller(s) => s
                .wrap_review(draft, recipient)
                .await
                .map_err(|e| e.to_string())?,
        }
    }
    async fn decode(&self, outer: &Event) -> Result<Event, String> {
        match self {
            Self::Buyer(keys) => unwrap(keys, outer).await,
            Self::Seller(s) => s
                .unwrap_review(outer.clone())
                .await
                .map_err(|e| e.to_string())?,
        }
    }
}
struct Transport<'a> {
    client: &'a Client,
    relay: &'a str,
    identity: Identity<'a>,
    request: &'a Request,
    reviewer: PublicKey,
}
impl wire::Transport for Transport<'_> {
    async fn fetch(&self, subject: &Subject, reviewer: &PublicKey) -> Result<Vec<Event>, String> {
        let recipient = self.identity.public_key()?;
        let rows = self
            .client
            .fetch_events_from(
                [self.relay],
                Filter::new()
                    .kind(Kind::GiftWrap)
                    .pubkey(recipient)
                    .since(Timestamp::from(
                        // A reply can land after this client's wait: the worker holds
                        // its own processing window (default 600s, configurable) behind
                        // a 300s request-freshness allowance, plus queueing. Cover that
                        // lifetime generously, or a later attempt reads a published
                        // reply as "no response" (#1115). A reply older than this is
                        // still collected by any retry within 30 minutes of it.
                        Timestamp::now()
                            .as_secs()
                            .saturating_sub(pc::transport::TIMESTAMP_TWEAK_SECS + 1800),
                    ))
                    .limit(128),
                std::time::Duration::from_secs(2),
            )
            .await
            .map_err(|_| "review: encrypted response read failed")?;
        if rows.len() >= 128 {
            return Err("review: encrypted response window saturated; retry required".into());
        }
        let mut found = Vec::new();
        for outer in rows {
            if let Ok(event) = self.identity.decode(&outer).await {
                if event.pubkey == *reviewer
                    && event.kind == Kind::Custom(REVIEW_KIND)
                    && event
                        .tags
                        .iter()
                        .any(|t| t.as_slice() == ["e", &subject.event, "", "reply"])
                {
                    found.push(event);
                }
            }
        }
        Ok(found)
    }
    async fn request(&self, _: EventDraft) -> Result<(), String> {
        let outer = self
            .identity
            .request(self.request.draft(&self.reviewer.to_hex())?, self.reviewer)
            .await?;
        let output = self
            .client
            .send_event_to([self.relay], &outer)
            .await
            .map_err(|_| "review: private request publication failed")?;
        // A relay refusal of the wrapper must surface now; otherwise the client
        // waits the full window for a response the reviewer can never send.
        wire::request_refused(&output)
    }
}

pub async fn check(
    home: &MaxplayerHome,
    client: &Client,
    identity: Identity<'_>,
    request: &Request,
    counterparty: &str,
) -> Result<Option<String>, String> {
    let config = &home.config.review;
    if !config.enabled(request.subject.kind, counterparty)? {
        return Ok(None);
    }
    let policy = pc::runtime::Policy::from_home(home).map_err(|e| e.to_string())?;
    let reviewer = config
        .reviewers
        .get(&home.config.relay_url)
        .ok_or("review: configure private service reviewer key")?;
    if reviewer != &policy.service {
        return Err(
            "review: private reviewer must be the configured content-service identity".into(),
        );
    }
    request
        .validate(&identity.public_key()?, &policy)
        .map_err(|code| reviewer_error(&code))?;
    let transport = Transport {
        client,
        relay: &home.config.relay_url,
        identity,
        request,
        reviewer: PublicKey::from_hex(reviewer).map_err(|_| "review: invalid reviewer key")?,
    };
    // Reuse the exact decision/retry contract, but only with the encrypted transport.
    wire::check(
        &transport,
        config,
        &home.config.relay_url,
        &request.subject,
        counterparty,
        false,
    )
    .await
}

/// The seller's offer review, before the claim and again before the agent runs. Both sites
/// route here: a private offer (`request` from [`offer_request`]) goes only over the encrypted
/// lane. The reviewer does not answer a public request for a private offer, so a public
/// fallback would block the job.
pub async fn seller_offer(
    home: &MaxplayerHome,
    client: &Client,
    signer: &crate::seller_node::signer::SignerHandle,
    relay: &str,
    request: Option<&Request>,
    job: &str,
    buyer: &str,
) -> Result<Option<String>, String> {
    if let Some(request) = request {
        return check(home, client, Identity::Seller(signer), request, buyer).await;
    }
    let subject = Subject {
        offer: job.into(),
        event: job.into(),
        kind: JOB_OFFER_KIND,
        commit: None,
    };
    wire::check(
        &wire::RelayTransport { client, relay },
        &home.config.review,
        relay,
        &subject,
        buyer,
        false,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn key(n: u8) -> Keys {
        Keys::parse(&format!("{n:064x}")).unwrap()
    }
    pub(crate) fn fixture(targeted: bool, delivery: bool) -> (Request, pc::runtime::Policy) {
        let (evidence, _, _, policy) = pc::evidence::inline_fixture_for(targeted);
        let subject = Subject {
            offer: evidence.offer.id.to_hex(),
            event: if delivery {
                evidence.result.id.to_hex()
            } else {
                evidence.offer.id.to_hex()
            },
            kind: if delivery {
                JOB_RESULT_KIND
            } else {
                JOB_OFFER_KIND
            },
            commit: None,
        };
        (
            Request {
                subject,
                offer: evidence.offer.clone(),
                task_envelope: evidence.task_envelope.clone(),
                delivery: delivery.then_some(evidence),
                followup: None,
            },
            policy,
        )
    }
    #[tokio::test]
    async fn private_review_wrappers_hide_request_result_and_error_and_reject_other_domains() {
        let buyer = key(1);
        let service = key(3);
        let outsider = key(4);
        let (request, _) = fixture(true, true);
        let inner = crate::gateway::nostr::event_builder(
            &request.draft(&service.public_key().to_hex()).unwrap(),
        )
        .unwrap()
        .sign_with_keys(&buyer)
        .unwrap();
        let outer = wrap(&buyer, service.public_key(), &inner).await.unwrap();
        let visible = outer.as_json();
        for secret in [
            &request.subject.offer,
            &request.subject.event,
            "private task",
            "task_envelope",
            DOMAIN,
        ] {
            assert!(!visible.contains(secret));
        }
        assert_eq!(outer.tags.len(), 1);
        assert_eq!(unwrap(&service, &outer).await.unwrap(), inner);
        assert!(unwrap(&outsider, &outer).await.is_err());
        assert!(
            pc::transport::unwrap_content(&service, &outer)
                .await
                .is_err()
        );
        for error in [false, true] {
            let review = Review {
                schema: 1,
                subject: request.subject.clone(),
                input_sha256: input_digest(b"guessable private task"),
                status: if error { "error" } else { "ok" }.into(),
                results: if error {
                    vec![]
                } else {
                    vec![Classification {
                        classifier: CLASSIFIER.into(),
                        version: CLASSIFIER_VERSION.into(),
                        label: "safe".into(),
                        probabilities: BTreeMap::from([
                            ("safe".into(), 0.9),
                            ("unsafe".into(), 0.1),
                        ]),
                    }]
                },
                error_code: error.then(|| "provider_unavailable".into()),
            };
            let mut draft = review_draft(&review).unwrap();
            draft.tags.push(TagSpec::new(["visibility", "private"]));
            let inner = crate::gateway::nostr::event_builder(&draft)
                .unwrap()
                .sign_with_keys(&service)
                .unwrap();
            for recipient in [key(1), key(2), key(3)] {
                let outer = wrap(&service, recipient.public_key(), &inner)
                    .await
                    .unwrap();
                assert!(!outer.as_json().contains(&review.input_sha256));
                assert!(!outer.as_json().contains("provider_unavailable"));
                let decoded = unwrap(&recipient, &outer).await.unwrap();
                assert_eq!(
                    wire::verify(&decoded, &service.public_key(), &request.subject).unwrap(),
                    review
                );
                assert!(unwrap(&outsider, &outer).await.is_err());
            }
        }
        let unrelated = pc::transport::wrap(
            &buyer,
            service.public_key(),
            "{\"domain\":\"other\"}".into(),
        )
        .await
        .unwrap();
        assert!(unwrap(&service, &unrelated).await.is_err());
        let mut tampered = outer.clone();
        tampered.content.push('x');
        assert!(unwrap(&service, &tampered).await.is_err());
    }
    #[test]
    fn private_review_checks_context_commitments_selection_and_requester() {
        for targeted in [false, true] {
            for delivery in [false, true] {
                let (r, p) = fixture(targeted, delivery);
                r.validate(&key(1).public_key(), &p).unwrap();
                r.validate(&key(2).public_key(), &p).unwrap();
                if targeted || delivery {
                    assert!(r.validate(&key(4).public_key(), &p).is_err());
                }
                let mut changed = r.clone();
                changed.subject.event = "ab".repeat(32);
                assert!(changed.validate(&key(1).public_key(), &p).is_err());
                if let Some(e) = changed.delivery.as_mut() {
                    e.answer_envelope = Some("{}".into());
                    assert!(changed.validate(&key(1).public_key(), &p).is_err());
                }
                let wrong = pc::runtime::Policy {
                    service: key(4).public_key().to_hex(),
                    host: pc::wire::HostPolicy {
                        git_prefix: p.host.git_prefix.clone(),
                        accepted_mints: p.host.accepted_mints.clone(),
                    },
                };
                if targeted || delivery {
                    assert!(r.validate(&key(1).public_key(), &wrong).is_err());
                }
            }
        }
    }
    #[tokio::test]
    async fn private_review_signer_actor_uses_same_encrypted_domain() {
        let dir = tempfile::tempdir().unwrap();
        let home = crate::home::bootstrap(dir.path()).unwrap();
        std::fs::write(&home.key_path, format!("{:064x}", 2)).unwrap();
        let actor = crate::seller_node::signer::spawn(&home).unwrap();
        let (request, _) = fixture(true, false);
        let service = key(3);
        let outer = actor
            .wrap_review(
                request.draft(&service.public_key().to_hex()).unwrap(),
                service.public_key(),
            )
            .await
            .unwrap()
            .unwrap();
        let inner = unwrap(&service, &outer).await.unwrap();
        assert_eq!(inner.pubkey, key(2).public_key());
        let mut draft = request_draft(&request.subject, &key(2).public_key().to_hex()).unwrap();
        draft.tags.push(TagSpec::new(["visibility", "private"]));
        let inner = crate::gateway::nostr::event_builder(&draft)
            .unwrap()
            .sign_with_keys(&service)
            .unwrap();
        let wrapped = wrap(&service, key(2).public_key(), &inner).await.unwrap();
        assert_eq!(actor.unwrap_review(wrapped).await.unwrap().unwrap(), inner);
    }
    #[tokio::test]
    async fn private_review_wrong_service_is_blocked_before_transport() {
        let dir = tempfile::tempdir().unwrap();
        let mut home = crate::home::bootstrap(dir.path()).unwrap();
        let (request, p) = fixture(true, true);
        home.config.privacy.service_pubkey = Some(p.service);
        home.config.privacy.git_base = Some(p.host.git_prefix);
        home.config
            .review
            .reviewers
            .insert(home.config.relay_url.clone(), key(4).public_key().to_hex());
        let keys = key(1);
        let client = Client::new(keys.clone());
        assert!(
            check(
                &home,
                &client,
                Identity::Buyer(&keys),
                &request,
                &key(2).public_key().to_hex()
            )
            .await
            .unwrap_err()
            .contains("content-service")
        );
    }

    /// A private follow-up as the buyer sends it: FollowUp content + projected 3412 carrier.
    fn followup_item(
        evidence: &pc::evidence::PrivateEvidence,
        policy: &pc::runtime::Policy,
        signer: &Keys,
        text: &str,
    ) -> FollowUpItem {
        followup_item_at(evidence, policy, signer, text, 2, &evidence.result.id.to_hex())
    }

    /// A follow-up for turn `turn` replying to result `prev`.
    fn followup_item_at(
        evidence: &pc::evidence::PrivateEvidence,
        policy: &pc::runtime::Policy,
        signer: &Keys,
        text: &str,
        turn: u32,
        prev: &str,
    ) -> FollowUpItem {
        let buyer = key(1);
        let content = pc::carriers::prepare_content(
            &evidence.offer,
            Some(&evidence.claim),
            Some(&evidence.award),
            &signer.public_key().to_hex(),
            &policy.service,
            pc::ContentType::FollowUp,
            text.into(),
            None,
            &policy.host,
        )
        .unwrap();
        let draft = crate::session::followup_draft(&crate::session::FollowUp {
            offer_id: evidence.offer.id.to_hex(),
            seller_pubkey: evidence.claim.pubkey.to_hex(),
            status: crate::session::FollowUpStatus::Turn,
            prev_result_id: Some(prev.to_owned()),
            turn: Some(turn),
            deadline_unix: Some(2_000_000_000),
            text: None,
        })
        .unwrap();
        let draft = pc::carriers::project(
            &evidence.offer,
            &draft,
            Some(&evidence.award),
            Some(&content),
            &policy.host,
        )
        .unwrap();
        let _ = buyer;
        FollowUpItem {
            carrier: pc::builders::sign(signer, draft).unwrap(),
            envelope: content.envelope().to_owned(),
        }
    }

    /// A signed private turn-n RESULT (inline, free job) answering follow-up `followup`, signed by
    /// `signer` (the selected seller in the honest case).
    fn turn_result(
        evidence: &pc::evidence::PrivateEvidence,
        policy: &pc::runtime::Policy,
        signer: &Keys,
        followup: &Event,
        status: crate::session::TurnStatus,
    ) -> Event {
        let answer = pc::carriers::prepare_content(
            &evidence.offer,
            Some(&evidence.claim),
            Some(&evidence.award),
            &signer.public_key().to_hex(),
            &policy.service,
            pc::ContentType::Answer,
            "a delivery body the reviewer never reads".into(),
            None,
            &policy.host,
        )
        .unwrap();
        let mut draft = crate::gateway::inline_result_draft(
            &evidence.offer.id.to_hex(),
            &key(1).public_key().to_hex(),
            "text/markdown",
            0,
            &pc::job_hash(&evidence.offer.id.to_hex()).unwrap(),
            &"cd".repeat(64),
            &answer.body().text,
            &[],
        );
        crate::session::append_result_tags(
            &mut draft,
            Some(&followup.id.to_hex()),
            status,
            false,
            None,
        );
        let draft = pc::carriers::project(
            &evidence.offer,
            &draft,
            Some(&evidence.award),
            Some(&answer),
            &policy.host,
        )
        .unwrap();
        pc::builders::sign(signer, draft).unwrap()
    }

    /// Session-jobs §9, private lane parity with the public lane: earlier deliveries reach the
    /// reviewer as one line each (turn, status, commit), read only from the seller's SIGNED result
    /// carriers and the buyer's signed follow-ups they answer; buyer follow-ups stay in full. A
    /// forged, misattributed, misordered or future delivery is refused, never rendered.
    #[test]
    fn private_followup_review_lists_earlier_deliveries_as_one_liners() {
        use crate::session::TurnStatus;
        let (evidence, _, _, policy) = pc::evidence::inline_fixture_for(true);
        let r1 = evidence.result.id.to_hex();
        let f2 = followup_item_at(&evidence, &policy, &key(1), "second ask", 2, &r1);
        let r2 = turn_result(&evidence, &policy, &key(2), &f2.carrier, TurnStatus::NeedsInput);
        let f3 = followup_item_at(&evidence, &policy, &key(1), "third ask", 3, &r2.id.to_hex());
        let d1 = DeliveryItem { result: evidence.result.clone(), followup: None };
        let d2 = DeliveryItem { result: r2.clone(), followup: Some(f2.carrier.clone()) };
        let request = |deliveries: Vec<DeliveryItem>| Request {
            subject: Subject {
                offer: evidence.offer.id.to_hex(),
                event: f3.carrier.id.to_hex(),
                kind: SESSION_FOLLOWUP_KIND,
                commit: None,
            },
            offer: evidence.offer.clone(),
            task_envelope: evidence.task_envelope.clone(),
            delivery: None,
            followup: Some(FollowUpEvidence {
                claim: evidence.claim.clone(),
                award: evidence.award.clone(),
                followups: vec![f2.clone(), f3.clone()],
                deliveries,
            }),
        };
        let good = request(vec![d1.clone(), d2.clone()]);
        let f = good.followup.as_ref().unwrap();
        assert_eq!(
            f.delivery_lines(&good.offer, &policy).unwrap(),
            vec![
                crate::session::DeliveryLine { turn: 1, status: TurnStatus::Done, commit: None },
                crate::session::DeliveryLine { turn: 2, status: TurnStatus::NeedsInput, commit: None },
            ]
        );
        let context = f.context(&good.offer, "the task", &policy).unwrap();
        assert_eq!(
            context,
            "ORIGINAL TASK:\nthe task\nturn 1: done, commit inline\nturn 2: needs_input, commit inline\n\
             FOLLOW-UP turn 2:\nsecond ask\nFOLLOW-UP turn 3:\nthird ask\n"
        );
        assert!(!context.contains("delivery body"), "delivery content never reaches the context");
        // Same bytes as the public lane renders for the same facts.
        let lines = f.delivery_lines(&good.offer, &policy).unwrap();
        assert_eq!(
            context,
            crate::session::followup_review_context(
                "the task",
                &["second ask".into(), "third ask".into()],
                &lines
            )
        );
        // The wire keeps the deliveries; both parties may still request; a stranger may not.
        let wire: Request = serde_json::from_str(&serde_json::to_string(&good).unwrap()).unwrap();
        assert_eq!(wire.followup.as_ref().unwrap().deliveries.len(), 2);
        for requester in [key(1), key(2)] {
            wire.validate(&requester.public_key(), &policy).unwrap();
        }
        assert!(wire.validate(&key(4).public_key(), &policy).is_err());
        // Without deliveries the context is exactly what the private lane rendered before.
        let none = request(vec![]);
        assert!(!serde_json::to_string(&none).unwrap().contains("deliveries"));
        assert_eq!(
            none.followup.as_ref().unwrap().context(&none.offer, "t", &policy).unwrap(),
            crate::session::followup_review_context("t", &["second ask".into(), "third ask".into()], &[])
        );
        // Refused: a delivery signed by anyone but the selected seller (the buyer cannot put a
        // delivery in the seller's mouth either) ...
        let by_buyer = turn_result(&evidence, &policy, &key(1), &f2.carrier, TurnStatus::Done);
        let refused = |deliveries: Vec<DeliveryItem>| {
            let r = request(deliveries);
            assert!(r.followup.as_ref().unwrap().delivery_lines(&r.offer, &policy).is_err());
            assert!(r.validate(&key(2).public_key(), &policy).is_err());
        };
        refused(vec![DeliveryItem { result: by_buyer, followup: Some(f2.carrier.clone()) }]);
        // ... a turn-n result paired with a follow-up it does not answer, or with none ...
        refused(vec![DeliveryItem { result: r2.clone(), followup: Some(f3.carrier.clone()) }]);
        refused(vec![DeliveryItem { result: r2.clone(), followup: None }]);
        // ... a turn-1 result dressed up with a follow-up ...
        refused(vec![DeliveryItem { result: evidence.result.clone(), followup: Some(f2.carrier.clone()) }]);
        // ... a follow-up forged by the seller to number a delivery ...
        let forged = followup_item_at(&evidence, &policy, &key(2), "x", 2, &r1);
        let r_forged = turn_result(&evidence, &policy, &key(2), &forged.carrier, TurnStatus::Done);
        refused(vec![DeliveryItem { result: r_forged, followup: Some(forged.carrier) }]);
        // ... out of order, duplicated, or not earlier than the follow-up under review.
        refused(vec![d2.clone(), d1.clone()]);
        refused(vec![d1.clone(), d1.clone()]);
        let r3 = turn_result(&evidence, &policy, &key(2), &f3.carrier, TurnStatus::Done);
        refused(vec![d1, d2, DeliveryItem { result: r3, followup: Some(f3.carrier.clone()) }]);
    }

    /// §9 size bound on the private lane: buyer follow-ups take the bound first (newest kept
    /// first), deliveries get what is left (newest kept first); the current follow-up is never cut.
    #[test]
    fn private_followup_request_fills_followups_before_deliveries() {
        use crate::session::TurnStatus;
        let (evidence, _, _, policy) = pc::evidence::inline_fixture_for(true);
        let r1 = evidence.result.id.to_hex();
        let f2 = followup_item_at(&evidence, &policy, &key(1), "second ask", 2, &r1);
        let r2 = turn_result(&evidence, &policy, &key(2), &f2.carrier, TurnStatus::Done);
        let f3 = followup_item_at(&evidence, &policy, &key(1), "third ask", 3, &r2.id.to_hex());
        let d1 = DeliveryItem { result: evidence.result.clone(), followup: None };
        let d2 = DeliveryItem { result: r2, followup: Some(f2.carrier.clone()) };
        let base = || Request {
            subject: Subject {
                offer: evidence.offer.id.to_hex(),
                event: f3.carrier.id.to_hex(),
                kind: SESSION_FOLLOWUP_KIND,
                commit: None,
            },
            offer: evidence.offer.clone(),
            task_envelope: evidence.task_envelope.clone(),
            delivery: None,
            followup: Some(FollowUpEvidence {
                claim: evidence.claim.clone(),
                award: evidence.award.clone(),
                followups: vec![f3.clone()],
                deliveries: vec![],
            }),
        };
        let size = |r: &Request| serde_json::to_string(r).unwrap().len();
        let ids = |r: &Request| {
            let f = r.followup.as_ref().unwrap();
            (
                f.followups.iter().map(|i| i.carrier.id).collect::<Vec<_>>(),
                f.deliveries.iter().map(|d| d.result.id).collect::<Vec<_>>(),
            )
        };
        // Unbounded: everything, oldest first.
        let mut all = base();
        fill_followup_evidence(&mut all, vec![f2.clone()], vec![d1.clone(), d2.clone()], usize::MAX);
        assert_eq!(ids(&all), (vec![f2.carrier.id, f3.carrier.id], vec![d1.result.id, d2.result.id]));
        all.validate(&key(2).public_key(), &policy).unwrap();
        // Room for the follow-up and only the newest delivery: the oldest delivery goes.
        let mut want = base();
        want.followup.as_mut().unwrap().followups.insert(0, f2.clone());
        want.followup.as_mut().unwrap().deliveries.push(d2.clone());
        let mut cut = base();
        fill_followup_evidence(&mut cut, vec![f2.clone()], vec![d1.clone(), d2.clone()], size(&want));
        assert_eq!(ids(&cut), (vec![f2.carrier.id, f3.carrier.id], vec![d2.result.id]));
        // Room for nothing extra: the earlier follow-up wins over any delivery only when it fits;
        // here neither fits and the current follow-up stays.
        let mut bare = base();
        let bound = size(&bare);
        fill_followup_evidence(&mut bare, vec![f2.clone()], vec![d1, d2], bound);
        assert_eq!(ids(&bare), (vec![f3.carrier.id], vec![]));
    }

    /// Session-jobs §9 private lane: a follow-up subject carries the buyer's signed carrier and
    /// its encrypted body; the reviewer authenticates both against the signed chain, the buyer
    /// and the selected seller may ask, nobody else, and a forged or swapped body is refused.
    #[test]
    fn private_followup_review_authenticates_the_buyers_carrier_and_body() {
        let (evidence, _, _, policy) = pc::evidence::inline_fixture_for(true);
        let item = followup_item(&evidence, &policy, &key(1), "now add tests");
        let request = |items: Vec<FollowUpItem>| Request {
            subject: Subject {
                offer: evidence.offer.id.to_hex(),
                event: items.last().unwrap().carrier.id.to_hex(),
                kind: SESSION_FOLLOWUP_KIND,
                commit: None,
            },
            offer: evidence.offer.clone(),
            task_envelope: evidence.task_envelope.clone(),
            delivery: None,
            followup: Some(FollowUpEvidence {
                claim: evidence.claim.clone(),
                award: evidence.award.clone(),
                followups: items,
                deliveries: Vec::new(),
            }),
        };
        let good = request(vec![item.clone()]);
        assert_eq!(
            good.followup
                .as_ref()
                .unwrap()
                .verify(&good.offer, &policy)
                .unwrap(),
            vec![(2, "now add tests".to_owned())]
        );
        // Wire round trip keeps the evidence, and both parties may request the review.
        let wire: Request = serde_json::from_str(&serde_json::to_string(&good).unwrap()).unwrap();
        for requester in [key(1), key(2)] {
            let recipients = wire.validate(&requester.public_key(), &policy).unwrap();
            assert!(recipients.contains(&key(2).public_key().to_hex()));
        }
        assert_eq!(
            wire.validate(&key(4).public_key(), &policy).unwrap_err(),
            "unauthorized_request"
        );
        // A carrier signed by the seller (words put in the buyer's mouth) is refused.
        let forged = followup_item(&evidence, &policy, &key(2), "now add tests");
        assert!(
            request(vec![forged])
                .validate(&key(2).public_key(), &policy)
                .is_err()
        );
        // Another follow-up's body under this carrier is refused.
        let other = followup_item(&evidence, &policy, &key(1), "something else");
        let mut swapped = item.clone();
        swapped.envelope = other.envelope;
        assert!(
            request(vec![swapped])
                .validate(&key(1).public_key(), &policy)
                .is_err()
        );
        // Follow-up evidence on a non-follow-up subject, or a subject that is not the last item.
        let mut stray = good.clone();
        stray.subject.event = evidence.offer.id.to_hex();
        stray.subject.kind = JOB_OFFER_KIND;
        assert!(stray.validate(&key(1).public_key(), &policy).is_err());
        let mut wrong = good;
        wrong.subject.event = evidence.result.id.to_hex();
        assert!(wrong.validate(&key(1).public_key(), &policy).is_err());
    }
}

#[cfg(test)]
mod local_error_tests {
    use super::*;
    #[tokio::test]
    async fn private_local_validation_failures_are_permanent_in_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut home = crate::home::bootstrap(dir.path()).unwrap();
        let (mut request, policy) = super::tests::fixture(true, true);
        home.config.privacy.service_pubkey = Some(policy.service.clone());
        home.config.privacy.git_base = Some(policy.host.git_prefix);
        home.config
            .review
            .reviewers
            .insert(home.config.relay_url.clone(), policy.service);
        let buyer = super::tests::key(1);
        let client = Client::new(buyer.clone());
        for unauthorized in [false, true] {
            let identity = if unauthorized {
                super::tests::key(4)
            } else {
                buyer.clone()
            };
            if !unauthorized {
                request.subject.event = "ab".repeat(32);
            } else {
                request.subject.event = request.delivery.as_ref().unwrap().result.id.to_hex();
            }
            let result = check(
                &home,
                &client,
                Identity::Buyer(&identity),
                &request,
                &super::tests::key(2).public_key().to_hex(),
            )
            .await;
            assert!(permanent_error(result.as_ref().unwrap_err()));
            state::completed(&home.root, &request.subject, &result).unwrap();
            let status = state::read(&home.root, &request.subject.event).unwrap();
            assert_eq!(status.state, "input_refused");
            assert!(!status.retry.contains("Retry the same"));
        }
    }
}
