//! Maxplayer Team screening (docs/specs/maxplayer-team.md §5): a reader asks the content
//! service to classify one team chat message or one team-repo commit before using it. Same
//! encrypted lane, classifier, threshold and override as private job reviews; the verdict goes
//! only to the requester, and no roster check is made.
use super::*;
use nostr_sdk::prelude::*;

/// Largest team chat event, as JSON (§5 client rules).
pub const MAX_MESSAGE_EVENT_BYTES: usize = 32 * 1024;
/// Chat text bound, the same as `maxplayer-chat`.
pub const MAX_MESSAGE_CHARS: usize = 8000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub subject: Subject,
    /// The kind-9 message, inline: the reviewer fetches nothing for it.
    pub message: Option<Event>,
    /// The channel creator, who owns the repo path (`/git/<creator>/<repo id>`).
    pub creator: Option<String>,
}

/// The repo id for a channel UUID: its 32 hex digits, then 32 zeros (§3).
pub fn repo_id(channel: &str) -> Option<String> {
    let id = uuid::Uuid::parse_str(channel).ok()?;
    (id.hyphenated().to_string() == channel).then(|| format!("{}{}", id.simple(), "0".repeat(32)))
}

/// A team chat message's repo id. Kind 9, valid signature, bounded size and text, and exactly
/// one two-element `h` tag holding a canonical lowercase UUID: the relay routes by the first
/// parseable `h` while fan-out matches any, so a second `h` could reach another team.
pub fn message_repo(event: &Event) -> Result<String, String> {
    let chars = event.content.chars().count();
    if event.kind != Kind::Custom(TEAM_MESSAGE_KIND)
        || event.verify().is_err()
        || event.as_json().len() > MAX_MESSAGE_EVENT_BYTES
        || chars == 0
        || chars > MAX_MESSAGE_CHARS
    {
        return Err("invalid_subject".into());
    }
    let h: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("h"))
        .collect();
    match h.as_slice() {
        [tag] if tag.as_slice().len() == 2 => {
            repo_id(&tag.as_slice()[1]).ok_or_else(|| "invalid_subject".into())
        }
        _ => Err("invalid_subject".into()),
    }
}

impl Request {
    pub fn message(event: Event) -> Result<Self, String> {
        let repo = message_repo(&event)?;
        Ok(Self {
            subject: Subject {
                offer: repo,
                event: event.id.to_hex(),
                kind: TEAM_MESSAGE_KIND,
                commit: None,
            },
            message: Some(event),
            creator: None,
        })
    }
    pub fn commit(repo: &str, creator: &str, commit: &str) -> Result<Self, String> {
        let request = Self {
            subject: Subject {
                offer: repo.into(),
                event: team_commit_event(repo, commit),
                kind: TEAM_COMMIT_SUBJECT_KIND,
                commit: Some(commit.into()),
            },
            message: None,
            creator: Some(creator.into()),
        };
        request.check()?;
        Ok(request)
    }
    fn check(&self) -> Result<(), String> {
        self.subject.validate().map_err(|_| "invalid_subject")?;
        match (self.subject.kind, &self.message, &self.creator) {
            (TEAM_MESSAGE_KIND, Some(m), None)
                if message_repo(m)? == self.subject.offer && m.id.to_hex() == self.subject.event =>
            {
                Ok(())
            }
            (TEAM_COMMIT_SUBJECT_KIND, None, Some(c)) if hex_id(c) => Ok(()),
            _ => Err("invalid_subject".into()),
        }
    }
    /// Revalidate before any fetch or classifier work. Recipients: the requester only.
    pub fn validate(&self, requester: &PublicKey) -> Result<Vec<String>, String> {
        self.check()?;
        Ok(vec![requester.to_hex()])
    }
    pub fn draft(&self, reviewer: &str) -> Result<EventDraft, String> {
        let mut draft = request_draft(&self.subject, reviewer)?;
        draft.tags.push(TagSpec::new(["visibility", "private"]));
        draft.content = serde_json::to_string(self).map_err(|_| "review: request encoding")?;
        super::private::check_size("request", draft.content.len(), super::private::MAX_MESSAGE)?;
        Ok(draft)
    }
}

/// The content-service reviewer key for `relay`, which must be the configured service key.
fn reviewer(config: &crate::home::MaxplayerConfig, relay: &str) -> Result<PublicKey, String> {
    config.review.validate()?;
    let reviewer = config
        .review
        .reviewers
        .get(relay)
        .ok_or("review: configure the content-service reviewer key for this relay")?;
    if config.privacy.service_pubkey.as_deref() != Some(reviewer.as_str()) {
        return Err("review: team reviewer must be the configured content-service identity".into());
    }
    PublicKey::from_hex(reviewer).map_err(|_| "review: invalid reviewer key".into())
}

/// Ask for (or reuse) the signed verdict on one team subject on `relay`. `Ok(None)`: review is
/// switched off for it. A refused verdict is returned too: the reader stores the signed event and
/// re-decides it with [`decide`] against its current config (spec §5). Private jobs need not be
/// enabled.
pub async fn check(
    config: &crate::home::MaxplayerConfig,
    relay: &str,
    client: &Client,
    keys: &Keys,
    request: &Request,
    counterparty: &str,
) -> Result<Option<Event>, String> {
    use super::wire::Transport as _;
    if !config.review.enabled(request.subject.kind, counterparty)? {
        return Ok(None);
    }
    let reviewer = reviewer(config, relay)?;
    request
        .validate(&keys.public_key())
        .map_err(|code| reviewer_error(&code))?;
    let draft = |reviewer: &str| request.draft(reviewer);
    let transport = super::private::Transport {
        client,
        relay,
        identity: super::private::Identity::Buyer(keys),
        draft: &draft,
        reviewer,
    };
    let subject = &request.subject;
    let id = match wire::check_detailed(&transport, &config.review, relay, subject, counterparty, false, None).await {
        Ok(Some(pass)) => pass.event_id,
        Ok(None) => return Ok(None),
        Err(e) if e.starts_with(VERDICT_REFUSED_PREFIX) => e
            .split("verdict event ")
            .nth(1)
            .and_then(|rest| rest.get(..64))
            .filter(|id| hex_id(id))
            .ok_or("review: refused verdict without an event id")?
            .to_owned(),
        Err(e) => return Err(e),
    };
    transport
        .fetch(subject, &reviewer)
        .await?
        .into_iter()
        .find(|e| e.id.to_hex() == id)
        .map(Some)
        .ok_or_else(|| "review: verdict event not found".into())
}

/// Re-decide a stored verdict offline against the current config (threshold, reviewer key).
/// `accepted` is a verdict id the reader accepted with `accept-verdict`: it overrides `Refused`
/// for exactly that verdict, never an error.
pub fn decide(
    config: &crate::home::MaxplayerConfig,
    relay: &str,
    verdict: &Event,
    subject: &Subject,
    accepted: Option<&str>,
) -> Result<Decision, String> {
    let review = wire::verify(verdict, &reviewer(config, relay)?, subject)?;
    Ok(match review.decision(subject, config.review.reject_at_or_above_ppm)? {
        Decision::Refused { .. } if accepted == Some(verdict.id.to_hex().as_str()) => Decision::Passed,
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const CHANNEL: &str = "0b0e5f1c-9d4a-4c3e-8f21-6a7b8c9d0e1f";
    fn message(tags: Vec<Vec<&str>>, text: &str) -> Event {
        let tags: Vec<Tag> = tags.into_iter().map(|t| Tag::parse(t).unwrap()).collect();
        EventBuilder::new(Kind::Custom(9), text)
            .tags(tags)
            .sign_with_keys(&Keys::generate())
            .unwrap()
    }
    #[test]
    fn subjects_bind_team_ids_and_refuse_job_shaped_offers() {
        let repo = repo_id(CHANNEL).unwrap();
        assert_eq!(repo, format!("0b0e5f1c9d4a4c3e8f216a7b8c9d0e1f{}", "0".repeat(32)));
        assert!(repo_id(&CHANNEL.to_uppercase()).is_none());
        let commit = "c".repeat(40);
        let ok = Request::commit(&repo, &"a".repeat(64), &commit).unwrap();
        assert!(ok.validate(&Keys::generate().public_key()).is_ok());
        // A job id in a team request is refused before anything is fetched.
        assert!(Request::commit(&"e".repeat(64), &"a".repeat(64), &commit).is_err());
        // The commit subject's `event` is bound to repo and commit.
        let mut moved = ok.clone();
        moved.subject.commit = Some("d".repeat(40));
        assert!(moved.check().is_err());
        assert_ne!(team_commit_event(&repo, &commit), team_commit_event(&repo, &"d".repeat(40)));
        assert!(Request::commit(&repo, "not-hex", &commit).is_err());
    }
    #[test]
    fn messages_need_one_canonical_h_tag_and_bounded_text() {
        let ok = Request::message(message(vec![vec!["h", CHANNEL]], "hi")).unwrap();
        assert_eq!(ok.subject.offer, repo_id(CHANNEL).unwrap());
        assert!(ok.validate(&Keys::generate().public_key()).is_ok());
        let other = "1b0e5f1c-9d4a-4c3e-8f21-6a7b8c9d0e1f";
        for bad in [
            message(vec![vec!["h", CHANNEL], vec!["h", other]], "hi"),
            message(vec![vec!["h", CHANNEL, "extra"]], "hi"),
            message(vec![vec!["h", &CHANNEL.to_uppercase()]], "hi"),
            message(vec![], "hi"),
            message(vec![vec!["h", CHANNEL]], ""),
            message(vec![vec!["h", CHANNEL]], &"x".repeat(MAX_MESSAGE_CHARS + 1)),
        ] {
            assert!(Request::message(bad).is_err());
        }
        // A message moved to another team's subject is refused.
        let mut moved = ok.clone();
        moved.subject.offer = repo_id(other).unwrap();
        assert!(moved.check().is_err());
        let mut wrong = ok;
        wrong.subject.event = "f".repeat(64);
        assert!(wrong.check().is_err());
    }
}
