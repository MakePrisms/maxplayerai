//! Community challenges v1 (docs/challenges-v1.md). Pure drafts and verified readers.
use crate::gateway::{EventDraft, TagSpec};
pub use crate::kinds::{CHALLENGE_APPLICATION_KIND, CHALLENGE_DECISION_KIND, CHALLENGE_KIND};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChallengeError(pub &'static str);
impl std::fmt::Display for ChallengeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ChallengeError {}
type Result<T> = std::result::Result<T, ChallengeError>;
fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ChallengeError(message))
    }
}
fn tag<'a>(draft: &'a EventDraft, name: &str) -> Result<&'a [String]> {
    let mut found = draft.tags.iter().filter(|t| t.first() == Some(name));
    let value = found.next().ok_or(ChallengeError("missing required tag"))?;
    require(found.next().is_none(), "duplicate tag")?;
    Ok(&value.0)
}
fn value<'a>(draft: &'a EventDraft, name: &str) -> Result<&'a str> {
    let t = tag(draft, name)?;
    require(t.len() == 2, "invalid tag arity")?;
    Ok(&t[1])
}
fn optional(draft: &EventDraft, name: &str) -> Result<Option<String>> {
    if draft.tags.iter().any(|t| t.first() == Some(name)) {
        Ok(Some(value(draft, name)?.into()))
    } else {
        Ok(None)
    }
}
fn number(s: &str) -> Result<u64> {
    require(
        !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()),
        "invalid unsigned integer",
    )?;
    s.parse()
        .map_err(|_| ChallengeError("integer out of range"))
}
fn valid_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
fn valid_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn base(d: &EventDraft, kind: u16) -> Result<()> {
    require(d.kind == kind, "wrong kind")?;
    require(value(d, "t")? == "maxplayer", "wrong namespace")?;
    require(value(d, "v")? == "1", "wrong version")
}
fn tags(d: &str) -> Vec<TagSpec> {
    vec![
        TagSpec::new(["t", "maxplayer"]),
        TagSpec::new(["v", "1"]),
        TagSpec::new(["d", d]),
    ]
}
fn add(tags: &mut Vec<TagSpec>, k: &str, v: impl ToString) {
    tags.push(TagSpec::new([k, &v.to_string()]));
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Challenge {
    pub buyer: String,
    pub id: String,
    pub title: String,
    pub goal: String,
    pub join_opens: u64,
    pub join_closes: u64,
    pub starts: u64,
    pub ends: u64,
    pub budget_sats: u64,
    pub status: String,
    pub update: Option<String>,
    pub result: Option<String>,
}
impl Challenge {
    pub fn coordinate(&self) -> String {
        format!("{CHALLENGE_KIND}:{}:{}", self.buyer, self.id)
    }
    pub fn to_event_draft(&self) -> Result<EventDraft> {
        let mut t = tags(&self.id);
        for (k, v) in [
            ("title", self.title.clone()),
            ("join_opens", self.join_opens.to_string()),
            ("join_closes", self.join_closes.to_string()),
            ("starts", self.starts.to_string()),
            ("ends", self.ends.to_string()),
            ("status", self.status.clone()),
        ] {
            add(&mut t, k, v);
        }
        t.push(TagSpec::new([
            "budget",
            &self.budget_sats.to_string(),
            "sat",
        ]));
        if let Some(v) = &self.update {
            add(&mut t, "update", v);
        }
        if let Some(v) = &self.result {
            add(&mut t, "result", v);
        }
        let d = EventDraft::new(CHALLENGE_KIND, t, &self.goal);
        parse_challenge_draft(&d, &self.buyer, &self.buyer)?;
        Ok(d)
    }
    /// CLI admission is stricter than reader timestamp drift tolerance.
    pub fn check_join(&self, now: u64) -> Result<()> {
        require(self.status != "cancelled", "challenge is cancelled")?;
        require(
            now >= self.join_opens && now <= self.join_closes,
            "challenge join window is closed",
        )
    }
}
fn parse_challenge_draft(d: &EventDraft, author: &str, buyer: &str) -> Result<Challenge> {
    base(d, CHALLENGE_KIND)?;
    require(
        valid_hex(buyer) && author == buyer,
        "wrong challenge author",
    )?;
    let id = value(d, "d")?;
    require(valid_id(id), "bad challenge d")?;
    let join_opens = number(value(d, "join_opens")?)?;
    let join_closes = number(value(d, "join_closes")?)?;
    let starts = number(value(d, "starts")?)?;
    let ends = number(value(d, "ends")?)?;
    require(
        join_opens.checked_add(3600) == Some(join_closes) && starts >= join_closes && ends > starts,
        "invalid challenge times",
    )?;
    let b = tag(d, "budget")?;
    require(b.len() == 3 && b[2] == "sat", "invalid budget unit")?;
    let budget_sats = number(&b[1])?;
    let status = value(d, "status")?;
    require(
        matches!(
            status,
            "scheduled" | "in_progress" | "completed" | "cancelled"
        ),
        "unknown challenge status",
    )?;
    let title = value(d, "title")?;
    require(!title.trim().is_empty(), "empty title")?;
    Ok(Challenge {
        buyer: buyer.into(),
        id: id.into(),
        title: title.into(),
        goal: d.content.clone(),
        join_opens,
        join_closes,
        starts,
        ends,
        budget_sats,
        status: status.into(),
        update: optional(d, "update")?,
        result: optional(d, "result")?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Application {
    pub seller: String,
    pub minutes: u64,
    pub status: String,
    pub note: String,
}
impl Application {
    pub fn to_event_draft(&self, c: &Challenge, created_at: u64) -> Result<EventDraft> {
        let mut t = tags(&format!("{}:{}", c.buyer, c.id));
        add(&mut t, "a", c.coordinate());
        add(&mut t, "p", &c.buyer);
        add(&mut t, "minutes", self.minutes);
        add(&mut t, "status", &self.status);
        let d = EventDraft::new(CHALLENGE_APPLICATION_KIND, t, &self.note);
        parse_application_draft(&d, &self.seller, created_at, c)?;
        Ok(d)
    }
}
fn parse_application_draft(
    d: &EventDraft,
    author: &str,
    created_at: u64,
    c: &Challenge,
) -> Result<Application> {
    base(d, CHALLENGE_APPLICATION_KIND)?;
    require(valid_hex(author), "invalid seller key")?;
    require(
        value(d, "a")? == c.coordinate() && value(d, "p")? == c.buyer,
        "unknown challenge or wrong buyer",
    )?;
    require(
        value(d, "d")? == format!("{}:{}", c.buyer, c.id),
        "bad application d",
    )?;
    require(
        created_at <= c.join_closes.saturating_add(900),
        "late application",
    )?;
    let minutes = number(value(d, "minutes")?)?;
    require((1..=480).contains(&minutes), "minutes must be 1..=480")?;
    let status = value(d, "status")?;
    require(
        matches!(status, "applied" | "withdrawn"),
        "unknown application status",
    )?;
    Ok(Application {
        seller: author.into(),
        minutes,
        status: status.into(),
        note: d.content.clone(),
    })
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub seller: String,
    pub decision: String,
    pub application_event_id: Option<String>,
    pub reason: String,
}
impl Decision {
    pub fn to_event_draft(&self, c: &Challenge) -> Result<EventDraft> {
        let mut t = tags(&format!("{}:{}", c.id, self.seller));
        add(&mut t, "a", c.coordinate());
        add(&mut t, "p", &self.seller);
        add(&mut t, "decision", &self.decision);
        if let Some(e) = &self.application_event_id {
            add(&mut t, "e", e);
        }
        let d = EventDraft::new(CHALLENGE_DECISION_KIND, t, &self.reason);
        parse_decision_draft(&d, &c.buyer, c)?;
        Ok(d)
    }
}
fn parse_decision_draft(d: &EventDraft, author: &str, c: &Challenge) -> Result<Decision> {
    base(d, CHALLENGE_DECISION_KIND)?;
    require(author == c.buyer, "wrong decision author")?;
    require(value(d, "a")? == c.coordinate(), "unknown challenge")?;
    let seller = value(d, "p")?;
    require(valid_hex(seller), "invalid seller key")?;
    require(
        value(d, "d")? == format!("{}:{}", c.id, seller),
        "bad decision d",
    )?;
    let decision = value(d, "decision")?;
    require(
        matches!(decision, "accepted" | "denied"),
        "unknown decision",
    )?;
    let application_event_id = optional(d, "e")?;
    require(
        application_event_id.as_deref().is_none_or(valid_hex),
        "invalid application event id",
    )?;
    Ok(Decision {
        seller: seller.into(),
        decision: decision.into(),
        application_event_id,
        reason: d.content.clone(),
    })
}

#[cfg(feature = "gateway")]
mod signed {
    use super::*;
    use nostr_sdk::Event;
    use std::collections::BTreeMap;
    fn verified(e: &Event) -> Result<EventDraft> {
        e.verify()
            .map_err(|_| ChallengeError("invalid event id or signature"))?;
        Ok(EventDraft::new(
            e.kind.as_u16(),
            e.tags
                .iter()
                .map(|t| TagSpec(t.as_slice().to_vec()))
                .collect(),
            &e.content,
        ))
    }
    pub fn parse_challenge(e: &Event, buyer: &str) -> Result<Challenge> {
        parse_challenge_draft(&verified(e)?, &e.pubkey.to_hex(), buyer)
    }
    pub fn parse_application(e: &Event, c: &Challenge) -> Result<Application> {
        parse_application_draft(&verified(e)?, &e.pubkey.to_hex(), e.created_at.as_secs(), c)
    }
    pub fn parse_decision(e: &Event, c: &Challenge) -> Result<Decision> {
        parse_decision_draft(&verified(e)?, &e.pubkey.to_hex(), c)
    }
    /// Verify before replacement. Protocol resolvers additionally filter invalid payloads
    /// before calling this primitive, as required by the v1 ignore-invalid rule.
    pub fn latest_per_coordinate<'a>(
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Vec<&'a Event> {
        let mut latest: BTreeMap<(String, u16, String), &Event> = BTreeMap::new();
        for e in events {
            let Ok(draft) = verified(e) else { continue };
            let Ok(d) = value(&draft, "d") else { continue };
            let key = (e.pubkey.to_hex(), e.kind.as_u16(), d.to_owned());
            let replace = latest.get(&key).is_none_or(|old| {
                e.created_at > old.created_at || (e.created_at == old.created_at && e.id < old.id)
            });
            if replace {
                latest.insert(key, e);
            }
        }
        latest.into_values().collect()
    }
    pub fn eligible_sellers<'a>(
        c: &Challenge,
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Vec<Application> {
        let latest = latest_per_coordinate(
            events
                .into_iter()
                .filter(|e| parse_application(e, c).is_ok() || parse_decision(e, c).is_ok()),
        );
        let accepted: std::collections::BTreeSet<_> = latest
            .iter()
            .filter_map(|e| parse_decision(e, c).ok())
            .filter(|d| d.decision == "accepted")
            .map(|d| d.seller)
            .collect();
        latest
            .iter()
            .filter_map(|e| parse_application(e, c).ok())
            .filter(|a| a.status == "applied" && accepted.contains(&a.seller))
            .collect()
    }
}
#[cfg(feature = "gateway")]
pub use signed::*;

#[cfg(test)]
mod tests {
    use super::*;
    fn challenge() -> Challenge {
        Challenge {
            buyer: "ab".repeat(32),
            id: "today".into(),
            title: "Build".into(),
            goal: "Goal".into(),
            join_opens: 100,
            join_closes: 3700,
            starts: 3700,
            ends: 7300,
            budget_sats: 100,
            status: "scheduled".into(),
            update: None,
            result: None,
        }
    }
    fn application() -> Application {
        Application {
            seller: "cd".repeat(32),
            minutes: 60,
            status: "applied".into(),
            note: String::new(),
        }
    }
    fn decision() -> Decision {
        Decision {
            seller: application().seller,
            decision: "accepted".into(),
            application_event_id: None,
            reason: String::new(),
        }
    }
    fn set(d: &mut EventDraft, k: &str, v: &str) {
        d.tags.iter_mut().find(|t| t.first() == Some(k)).unwrap().0[1] = v.into();
    }
    #[test]
    fn drafts_roundtrip() {
        let c = challenge();
        assert_eq!(
            parse_challenge_draft(&c.to_event_draft().unwrap(), &c.buyer, &c.buyer).unwrap(),
            c
        );
        assert_eq!(
            parse_application_draft(
                &application().to_event_draft(&c, 100).unwrap(),
                &application().seller,
                100,
                &c
            )
            .unwrap(),
            application()
        );
        assert_eq!(
            parse_decision_draft(&decision().to_event_draft(&c).unwrap(), &c.buyer, &c).unwrap(),
            decision()
        );
    }
    #[test]
    fn every_required_tag_and_duplicates_refused() {
        let c = challenge();
        let a = application();
        for original in [
            c.to_event_draft().unwrap(),
            a.to_event_draft(&c, 100).unwrap(),
            decision().to_event_draft(&c).unwrap(),
        ] {
            let parse = |d: &EventDraft| match original.kind {
                CHALLENGE_KIND => parse_challenge_draft(d, &c.buyer, &c.buyer).map(|_| ()),
                CHALLENGE_APPLICATION_KIND => {
                    parse_application_draft(d, &a.seller, 100, &c).map(|_| ())
                }
                _ => parse_decision_draft(d, &c.buyer, &c).map(|_| ()),
            };
            for i in 0..original.tags.len() {
                let mut d = original.clone();
                d.tags.remove(i);
                assert!(parse(&d).is_err());
                let mut d = original.clone();
                d.tags.push(d.tags[i].clone());
                assert!(parse(&d).is_err());
            }
            for (k, v) in [("t", "other"), ("v", "2"), ("d", "bad:d")] {
                let mut d = original.clone();
                set(&mut d, k, v);
                assert!(parse(&d).is_err());
            }
            let mut d = original.clone();
            d.kind = 30344;
            assert!(parse(&d).is_err());
        }
    }
    #[test]
    fn wrong_authors_coordinates_and_keys_refused() {
        let c = challenge();
        let a = application();
        assert!(parse_challenge_draft(&c.to_event_draft().unwrap(), &a.seller, &c.buyer).is_err());
        assert!(
            parse_decision_draft(&decision().to_event_draft(&c).unwrap(), &a.seller, &c).is_err()
        );
        for (k, v) in [("a", "30341:unknown:today"), ("p", &a.seller)] {
            let mut d = a.to_event_draft(&c, 100).unwrap();
            set(&mut d, k, v);
            assert!(parse_application_draft(&d, &a.seller, 100, &c).is_err());
        }
        let mut d = decision().to_event_draft(&c).unwrap();
        set(&mut d, "p", "NOT-HEX");
        assert!(parse_decision_draft(&d, &c.buyer, &c).is_err());
        let mut d = decision().to_event_draft(&c).unwrap();
        set(&mut d, "a", "wrong");
        assert!(parse_decision_draft(&d, &c.buyer, &c).is_err());
        let mut d = decision();
        d.application_event_id = Some("bad".into());
        assert!(d.to_event_draft(&c).is_err());
    }
    #[test]
    fn numbers_times_statuses_and_ids_refused() {
        let c = challenge();
        let a = application();
        for bad in ["0", "481", "-1", "+1", "1.5", "18446744073709551616", ""] {
            let mut d = a.to_event_draft(&c, 100).unwrap();
            set(&mut d, "minutes", bad);
            assert!(
                parse_application_draft(&d, &a.seller, 100, &c).is_err(),
                "{bad}"
            );
        }
        for n in [1, 480] {
            let mut a = a.clone();
            a.minutes = n;
            assert!(a.to_event_draft(&c, 100).is_ok());
        }
        for (k, v) in [
            ("join_opens", "-1"),
            ("join_closes", "100"),
            ("join_closes", "3701"),
            ("starts", "3699"),
            ("ends", "3700"),
            ("budget", "-1"),
            ("status", "unknown"),
            ("title", " "),
            ("d", ""),
            ("d", "ABC"),
            ("d", &"x".repeat(65)),
        ] {
            let mut d = c.to_event_draft().unwrap();
            set(&mut d, k, v);
            assert!(
                parse_challenge_draft(&d, &c.buyer, &c.buyer).is_err(),
                "{k}: {v}"
            );
        }
        let mut d = c.to_event_draft().unwrap();
        d.tags
            .iter_mut()
            .find(|t| t.first() == Some("budget"))
            .unwrap()
            .0[2] = "usd".into();
        assert!(parse_challenge_draft(&d, &c.buyer, &c.buyer).is_err());
        let mut a = a;
        a.status = "unknown".into();
        assert!(a.to_event_draft(&c, 100).is_err());
        let mut d = decision();
        d.decision = "maybe".into();
        assert!(d.to_event_draft(&c).is_err());
    }
    #[test]
    fn late_application_and_closed_join_refused() {
        let mut c = challenge();
        assert!(application().to_event_draft(&c, 4600).is_ok());
        assert!(application().to_event_draft(&c, 4601).is_err());
        assert!(c.check_join(99).is_err());
        assert!(c.check_join(100).is_ok());
        assert!(c.check_join(3700).is_ok());
        assert!(c.check_join(3701).is_err());
        c.status = "cancelled".into();
        assert!(c.check_join(100).is_err());
    }
    #[cfg(feature = "gateway")]
    mod verified {
        use super::*;
        use nostr_sdk::prelude::*;
        fn sign(d: EventDraft, k: &Keys, t: u64) -> Event {
            EventBuilder::new(Kind::from(d.kind), d.content)
                .tags(d.tags.into_iter().map(|t| Tag::parse(t.0).unwrap()))
                .custom_created_at(Timestamp::from(t))
                .sign_with_keys(k)
                .unwrap()
        }
        #[test]
        fn signatures_ids_authors_and_known_challenge_required() {
            let buyer = Keys::generate();
            let seller = Keys::generate();
            let mut c = challenge();
            c.buyer = buyer.public_key().to_hex();
            let mut e = sign(c.to_event_draft().unwrap(), &buyer, 100);
            assert!(parse_challenge(&e, &c.buyer).is_ok());
            assert!(parse_challenge(&e, &seller.public_key().to_hex()).is_err());
            e.content.push('x');
            assert!(parse_challenge(&e, &c.buyer).is_err());
            let mut a = application();
            a.seller = seller.public_key().to_hex();
            let mut e = sign(a.to_event_draft(&c, 100).unwrap(), &seller, 100);
            assert!(parse_application(&e, &c).is_ok());
            let mut other = c.clone();
            other.id = "other".into();
            assert!(parse_application(&e, &other).is_err());
            e.sig = sign(c.to_event_draft().unwrap(), &buyer, 100).sig;
            assert!(parse_application(&e, &c).is_err());
        }
        #[test]
        fn latest_tie_break_and_eligible_sellers() {
            let buyer = Keys::generate();
            let seller = Keys::generate();
            let mut c = challenge();
            c.buyer = buyer.public_key().to_hex();
            let mut a = application();
            a.seller = seller.public_key().to_hex();
            let applied = sign(a.to_event_draft(&c, 100).unwrap(), &seller, 100);
            let mut d = decision();
            d.seller = a.seller.clone();
            let accepted = sign(d.to_event_draft(&c).unwrap(), &buyer, 100);
            assert_eq!(eligible_sellers(&c, [&applied, &accepted]), vec![a.clone()]);
            a.status = "withdrawn".into();
            let withdrawn = sign(a.to_event_draft(&c, 101).unwrap(), &seller, 101);
            assert!(eligible_sellers(&c, [&applied, &accepted, &withdrawn]).is_empty());
            d.decision = "denied".into();
            let denied = sign(d.to_event_draft(&c).unwrap(), &buyer, 101);
            assert!(eligible_sellers(&c, [&applied, &accepted, &denied]).is_empty());
            let tied = sign(d.to_event_draft(&c).unwrap(), &buyer, 100);
            let winner = if tied.id < accepted.id {
                &tied
            } else {
                &accepted
            };
            assert_eq!(latest_per_coordinate([&tied, &accepted]), vec![winner]);
            assert_eq!(latest_per_coordinate([&accepted, &tied]), vec![winner]);
            let wrong = sign(d.to_event_draft(&c).unwrap(), &seller, 200);
            assert_eq!(eligible_sellers(&c, [&applied, &accepted, &wrong]).len(), 1);
            let mut invalid = withdrawn.clone();
            invalid.content.push('x');
            assert_eq!(
                eligible_sellers(&c, [&applied, &accepted, &invalid]).len(),
                1
            );
        }
    }
}

/// Relay effects are separate from the pure protocol, and never touch jobs or money.
#[cfg(feature = "wallet")]
pub mod relay {
    use super::*;
    use crate::home::{self, MaxplayerHome};
    use nostr_sdk::prelude::{Event, EventBuilder, Filter, Keys, Kind, Tag, Timestamp};
    use std::time::Duration;
    fn keys(home: &MaxplayerHome) -> Result<Keys> {
        let secret = home::read_secret_key_hex(home)
            .map_err(|_| ChallengeError("could not read home key"))?;
        Keys::parse(&secret).map_err(|_| ChallengeError("invalid home key"))
    }
    pub async fn fetch(home: &MaxplayerHome, filter: Filter) -> Result<Vec<Event>> {
        let client = crate::relay_auth::connect_authenticated(&keys(home)?, &home.config.relay_url)
            .await
            .map_err(|_| ChallengeError("relay authentication failed"))?;
        let fetched = client.fetch_events(filter, Duration::from_secs(10)).await;
        client.disconnect().await;
        fetched
            .map(|e| e.into_iter().collect())
            .map_err(|_| ChallengeError("challenge relay fetch failed"))
    }
    pub async fn publish(
        home: &MaxplayerHome,
        draft: EventDraft,
        created_at: u64,
    ) -> Result<String> {
        let keys = keys(home)?;
        let tags = draft
            .tags
            .into_iter()
            .map(|t| Tag::parse(t.0))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| ChallengeError("invalid event tags"))?;
        let event = EventBuilder::new(Kind::from(draft.kind), draft.content)
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(&keys)
            .map_err(|_| ChallengeError("challenge signing failed"))?;
        crate::profile::send_signed_event(home, &keys, &event, "challenge")
            .await
            .map_err(|_| ChallengeError("no relay accepted challenge event"))
    }
}
