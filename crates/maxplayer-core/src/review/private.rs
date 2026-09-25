//! Private execution reviews: signed inner messages inside recipient-only NIP-59
//! wrappers. The public transport is never used for a private job, including errors.
use super::*;
use crate::{home::MaxplayerHome, private_content as pc};
use nostr_sdk::prelude::*;

const DOMAIN: &str = "maxplayer-private-review-v1";
const MAX_MESSAGE: usize = 60 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub subject: Subject,
    pub offer: Event,
    pub task_envelope: Option<String>,
    pub delivery: Option<pc::evidence::PrivateEvidence>,
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
    if body.len() > MAX_MESSAGE {
        return Err("review: encrypted message too large".into());
    }
    pc::transport::wrap(keys, recipient, body)
        .await
        .map_err(|e| e.to_string())
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
        self.subject.validate()?;
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
        if self.subject.kind == JOB_OFFER_KIND {
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
        if draft.content.len() > MAX_MESSAGE / 2 {
            return Err("review: private input too large".into());
        }
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
    }))
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
                        Timestamp::now()
                            .as_secs()
                            .saturating_sub(pc::transport::TIMESTAMP_TWEAK_SECS + 300),
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
        self.client
            .send_event_to([self.relay], &outer)
            .await
            .map_err(|_| "review: private request publication failed")?;
        Ok(())
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
    request.validate(&identity.public_key()?, &policy)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    fn key(n: u8) -> Keys {
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
}
