//! Swarms (docs/specs/swarm.md §4). Pure drafts, validators and verified readers for the four
//! swarm kinds. Relay I/O lives in [`crate::swarm_relay`].
//!
//! | Kind | Object | Author | `d` |
//! |---|---|---|---|
//! | `30344` | SWARM | buyer | slug |
//! | `30341` | SWARM JOB (`a` = its swarm) | buyer | slug |
//! | `30342` | SWARM MEMBERSHIP (`a` = the swarm) | seller | `<buyer>:<swarm d>` |
//! | `30343` | SWARM DECISION | buyer | `<seller>` (buyer-wide) or `<seller>:<swarm d>` (per swarm) |
use crate::gateway::{EventDraft, TagSpec};
pub use crate::kinds::{SWARM_DECISION_KIND, SWARM_JOB_KIND, SWARM_KIND, SWARM_MEMBERSHIP_KIND};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwarmError(pub &'static str);
impl std::fmt::Display for SwarmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for SwarmError {}
type Result<T> = std::result::Result<T, SwarmError>;

/// Minutes a seller may offer one swarm per day.
pub const MINUTES_MAX: u64 = 480;
/// Longest swarm name, in characters.
pub const NAME_MAX: usize = 128;
/// A membership note longer than this (UTF-16 units) is read back empty.
const NOTE_MAX: usize = 4096;
/// A membership whose tags serialize longer than this (UTF-16 units) has its note dropped.
const TAGS_MAX: usize = 16384;

fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok { Ok(()) } else { Err(SwarmError(message)) }
}
fn tag<'a>(draft: &'a EventDraft, name: &str) -> Result<&'a [String]> {
    let mut found = draft.tags.iter().filter(|t| t.first() == Some(name));
    let value = found.next().ok_or(SwarmError("missing required tag"))?;
    require(found.next().is_none(), "duplicate tag")?;
    Ok(&value.0)
}
fn value<'a>(draft: &'a EventDraft, name: &str) -> Result<&'a str> {
    let t = tag(draft, name)?;
    require(t.len() == 2, "invalid tag arity")?;
    Ok(&t[1])
}
fn has(draft: &EventDraft, name: &str) -> bool {
    draft.tags.iter().any(|t| t.first() == Some(name))
}
fn optional<'a>(draft: &'a EventDraft, name: &str) -> Result<Option<&'a str>> {
    if has(draft, name) {
        Ok(Some(value(draft, name)?))
    } else {
        Ok(None)
    }
}
fn number(s: &str) -> Result<u64> {
    require(
        !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()),
        "invalid unsigned integer",
    )?;
    s.parse().map_err(|_| SwarmError("integer out of range"))
}
/// A swarm or swarm job `d`: 1–64 of `a-z`, `0-9`, `-`. Never contains `:`, so it can be the
/// suffix of a membership or decision `d`.
pub fn valid_slug(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
pub fn valid_hex(s: &str) -> bool {
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

/// `30344:<buyer>:<swarm d>`, the `a` value naming a swarm.
pub fn swarm_address(buyer: &str, swarm: &str) -> String {
    format!("{SWARM_KIND}:{buyer}:{swarm}")
}
/// Split a swarm `a` value into `(buyer, swarm d)`.
pub fn parse_swarm_address(a: &str) -> Result<(&str, &str)> {
    let mut parts = a.splitn(3, ':');
    let (Some(kind), Some(buyer), Some(swarm)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(SwarmError("invalid swarm address"));
    };
    require(kind == SWARM_KIND.to_string(), "address names another kind")?;
    require(
        valid_hex(buyer) && valid_slug(swarm),
        "invalid swarm address",
    )?;
    Ok((buyer, swarm))
}

/// A named group of sellers a buyer creates (kind `30344`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Swarm {
    pub buyer: String,
    pub id: String,
    pub name: String,
    /// Markdown description; the event content.
    pub purpose: String,
    /// `open` | `closed`.
    pub status: String,
    /// `public` | `private` (§7.5). Absent on the wire means `public`.
    pub tasks: String,
}
impl Swarm {
    pub fn address(&self) -> String {
        swarm_address(&self.buyer, &self.id)
    }
    pub fn to_event_draft(&self) -> Result<EventDraft> {
        let mut t = tags(&self.id);
        add(&mut t, "name", &self.name);
        add(&mut t, "status", &self.status);
        add(&mut t, "tasks", &self.tasks);
        let d = EventDraft::new(SWARM_KIND, t, &self.purpose);
        parse_swarm_draft(&d, &self.buyer)?;
        Ok(d)
    }
}
fn parse_swarm_draft(d: &EventDraft, author: &str) -> Result<Swarm> {
    base(d, SWARM_KIND)?;
    require(valid_hex(author), "invalid buyer key")?;
    let id = value(d, "d")?;
    require(valid_slug(id), "bad swarm d")?;
    let name = value(d, "name")?;
    require(
        !name.trim().is_empty() && name.chars().count() <= NAME_MAX,
        "swarm name must be 1..=128 characters",
    )?;
    let status = value(d, "status")?;
    require(matches!(status, "open" | "closed"), "unknown swarm status")?;
    let tasks = optional(d, "tasks")?.unwrap_or("public");
    require(matches!(tasks, "public" | "private"), "unknown swarm tasks")?;
    Ok(Swarm {
        buyer: author.into(),
        id: id.into(),
        name: name.into(),
        purpose: d.content.clone(),
        status: status.into(),
        tasks: tasks.into(),
    })
}

/// A goal the buyer gives one of its swarms (kind `30341`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SwarmJob {
    pub buyer: String,
    /// The swarm's `d`; the swarm is by the same buyer.
    pub swarm: String,
    pub id: String,
    pub title: String,
    pub goal: String,
    pub starts: u64,
    pub ends: u64,
    pub budget_sats: u64,
    /// `scheduled` | `in_progress` | `completed` | `cancelled`.
    pub status: String,
    pub update: Option<String>,
    pub result: Option<String>,
}
impl SwarmJob {
    pub fn address(&self) -> String {
        format!("{SWARM_JOB_KIND}:{}:{}", self.buyer, self.id)
    }
    pub fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "scheduled" | "in_progress")
    }
    pub fn to_event_draft(&self) -> Result<EventDraft> {
        let mut t = tags(&self.id);
        add(&mut t, "a", swarm_address(&self.buyer, &self.swarm));
        add(&mut t, "title", &self.title);
        add(&mut t, "starts", self.starts);
        add(&mut t, "ends", self.ends);
        add(&mut t, "status", &self.status);
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
        let d = EventDraft::new(SWARM_JOB_KIND, t, &self.goal);
        parse_swarm_job_draft(&d, &self.buyer)?;
        Ok(d)
    }
}
fn parse_swarm_job_draft(d: &EventDraft, author: &str) -> Result<SwarmJob> {
    base(d, SWARM_JOB_KIND)?;
    require(valid_hex(author), "invalid buyer key")?;
    let id = value(d, "d")?;
    require(valid_slug(id), "bad swarm job d")?;
    let (buyer, swarm) = parse_swarm_address(value(d, "a")?)?;
    require(buyer == author, "swarm job names another buyer's swarm")?;
    let starts = number(value(d, "starts")?)?;
    let ends = number(value(d, "ends")?)?;
    require(ends > starts, "invalid swarm job times")?;
    let b = tag(d, "budget")?;
    require(b.len() == 3 && b[2] == "sat", "invalid budget unit")?;
    let budget_sats = number(&b[1])?;
    let status = value(d, "status")?;
    require(
        matches!(
            status,
            "scheduled" | "in_progress" | "completed" | "cancelled"
        ),
        "unknown swarm job status",
    )?;
    let title = value(d, "title")?;
    require(!title.trim().is_empty(), "empty title")?;
    Ok(SwarmJob {
        buyer: author.into(),
        swarm: swarm.into(),
        id: id.into(),
        title: title.into(),
        goal: d.content.clone(),
        starts,
        ends,
        budget_sats,
        status: status.into(),
        update: optional(d, "update")?.map(str::to_owned),
        result: optional(d, "result")?.map(str::to_owned),
    })
}

/// A seller's membership of one swarm (kind `30342`, `d = <buyer>:<swarm d>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Membership {
    pub buyer: String,
    pub swarm: String,
    pub seller: String,
    pub since: u64,
    /// Minutes per day offered to this swarm, 1..=480.
    pub minutes: u64,
    /// `joined` | `left`.
    pub status: String,
    pub note: String,
}
impl Membership {
    pub fn to_event_draft(&self) -> Result<EventDraft> {
        let mut t = tags(&format!("{}:{}", self.buyer, self.swarm));
        add(&mut t, "a", swarm_address(&self.buyer, &self.swarm));
        add(&mut t, "p", &self.buyer);
        add(&mut t, "since", self.since);
        add(&mut t, "minutes", self.minutes);
        add(&mut t, "status", &self.status);
        let d = EventDraft::new(SWARM_MEMBERSHIP_KIND, t, &self.note);
        parse_membership_draft(&d, &self.seller)?;
        Ok(d)
    }
}
fn parse_membership_draft(d: &EventDraft, author: &str) -> Result<Membership> {
    base(d, SWARM_MEMBERSHIP_KIND)?;
    require(valid_hex(author), "invalid seller key")?;
    let (buyer, swarm) = parse_swarm_address(value(d, "a")?)?;
    require(value(d, "p")? == buyer, "membership p and a disagree")?;
    require(
        value(d, "d")? == format!("{buyer}:{swarm}"),
        "bad membership d",
    )?;
    let since = number(value(d, "since")?)?;
    let minutes = number(value(d, "minutes")?)?;
    require(
        (1..=MINUTES_MAX).contains(&minutes),
        "minutes must be 1..=480",
    )?;
    let status = value(d, "status")?;
    require(
        matches!(status, "joined" | "left"),
        "unknown membership status",
    )?;
    let oversized = d.content.encode_utf16().count() > NOTE_MAX
        || serde_json::to_string(&d.tags.iter().map(|t| &t.0).collect::<Vec<_>>())
            .expect("string tags")
            .encode_utf16()
            .count()
            > TAGS_MAX;
    Ok(Membership {
        buyer: buyer.into(),
        swarm: swarm.into(),
        seller: author.into(),
        since,
        minutes,
        status: status.into(),
        note: if oversized {
            String::new()
        } else {
            d.content.clone()
        },
    })
}

/// A buyer's decision on a seller (kind `30343`): buyer-wide (`swarm = None`, `d = <seller>`) or
/// for one swarm (`d = <seller>:<swarm d>`, `a` = that swarm).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub buyer: String,
    pub seller: String,
    pub swarm: Option<String>,
    /// `accepted` | `denied`.
    pub decision: String,
    pub membership_event_id: Option<String>,
    pub reason: String,
}
impl Decision {
    pub fn to_event_draft(&self) -> Result<EventDraft> {
        let d = match &self.swarm {
            Some(swarm) => format!("{}:{swarm}", self.seller),
            None => self.seller.clone(),
        };
        let mut t = tags(&d);
        if let Some(swarm) = &self.swarm {
            add(&mut t, "a", swarm_address(&self.buyer, swarm));
        }
        add(&mut t, "p", &self.seller);
        add(&mut t, "decision", &self.decision);
        if let Some(e) = &self.membership_event_id {
            add(&mut t, "e", e);
        }
        let d = EventDraft::new(SWARM_DECISION_KIND, t, &self.reason);
        parse_decision_draft(&d, &self.buyer)?;
        Ok(d)
    }
}
fn parse_decision_draft(d: &EventDraft, author: &str) -> Result<Decision> {
    base(d, SWARM_DECISION_KIND)?;
    require(valid_hex(author), "invalid buyer key")?;
    let seller = value(d, "p")?;
    require(valid_hex(seller), "invalid seller key")?;
    let swarm = match optional(d, "a")? {
        Some(a) => {
            let (buyer, swarm) = parse_swarm_address(a)?;
            require(buyer == author, "decision names another buyer's swarm")?;
            require(
                value(d, "d")? == format!("{seller}:{swarm}"),
                "bad decision d",
            )?;
            Some(swarm.to_owned())
        }
        None => {
            require(value(d, "d")? == seller, "bad decision d")?;
            None
        }
    };
    let decision = value(d, "decision")?;
    require(
        matches!(decision, "accepted" | "denied"),
        "unknown decision",
    )?;
    let membership_event_id = optional(d, "e")?.map(str::to_owned);
    require(
        membership_event_id.as_deref().is_none_or(valid_hex),
        "invalid membership event id",
    )?;
    Ok(Decision {
        buyer: author.into(),
        seller: seller.into(),
        swarm,
        decision: decision.into(),
        membership_event_id,
        reason: d.content.clone(),
    })
}

/// The eligibility rule for one seller in one swarm (§4): at least one of the latest buyer-wide
/// and the latest per-swarm decision is `accepted`, and neither is `denied`.
pub fn decision_allows(buyer_wide: Option<&str>, per_swarm: Option<&str>) -> bool {
    let both = [buyer_wide, per_swarm];
    both.contains(&Some("accepted")) && !both.contains(&Some("denied"))
}

#[cfg(feature = "gateway")]
mod signed {
    use super::*;
    use nostr_sdk::Event;
    use std::collections::{BTreeMap, BTreeSet};
    fn verified(e: &Event) -> Result<EventDraft> {
        e.verify()
            .map_err(|_| SwarmError("invalid event id or signature"))?;
        Ok(EventDraft::new(
            e.kind.as_u16(),
            e.tags
                .iter()
                .map(|t| TagSpec(t.as_slice().to_vec()))
                .collect(),
            &e.content,
        ))
    }
    pub fn parse_swarm(e: &Event) -> Result<Swarm> {
        parse_swarm_draft(&verified(e)?, &e.pubkey.to_hex())
    }
    pub fn parse_swarm_job(e: &Event) -> Result<SwarmJob> {
        parse_swarm_job_draft(&verified(e)?, &e.pubkey.to_hex())
    }
    pub fn parse_membership(e: &Event) -> Result<Membership> {
        parse_membership_draft(&verified(e)?, &e.pubkey.to_hex())
    }
    pub fn parse_decision(e: &Event) -> Result<Decision> {
        parse_decision_draft(&verified(e)?, &e.pubkey.to_hex())
    }
    /// Verify before replacement: the newest event per `(author, kind, d)`; a `created_at` tie
    /// goes to the lower id. Callers drop invalid payloads first (the ignore-invalid rule), so an
    /// invalid newer event never hides a valid older one.
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
    /// Latest valid swarms (any buyer), as `(event, swarm)`.
    pub fn latest_swarms<'a>(
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Vec<(&'a Event, Swarm)> {
        latest_per_coordinate(events.into_iter().filter(|e| parse_swarm(e).is_ok()))
            .into_iter()
            .filter_map(|e| parse_swarm(e).ok().map(|s| (e, s)))
            .collect()
    }
    /// Latest valid swarm jobs (any buyer), as `(event, job)`.
    pub fn latest_swarm_jobs<'a>(
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Vec<(&'a Event, SwarmJob)> {
        latest_per_coordinate(events.into_iter().filter(|e| parse_swarm_job(e).is_ok()))
            .into_iter()
            .filter_map(|e| parse_swarm_job(e).ok().map(|j| (e, j)))
            .collect()
    }
    /// Latest valid memberships (any seller, any swarm), as `(event, membership)`.
    pub fn latest_memberships<'a>(
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Vec<(&'a Event, Membership)> {
        latest_per_coordinate(events.into_iter().filter(|e| parse_membership(e).is_ok()))
            .into_iter()
            .filter_map(|e| parse_membership(e).ok().map(|m| (e, m)))
            .collect()
    }

    /// One buyer's latest decisions, buyer-wide and per swarm. Only that buyer's valid signed
    /// decisions count: nothing anyone else signs changes it.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Decisions {
        pub buyer_wide: BTreeMap<String, Decision>,
        pub per_swarm: BTreeMap<(String, String), Decision>,
    }
    impl Decisions {
        pub fn read<'a>(buyer: &str, events: impl IntoIterator<Item = &'a Event>) -> Self {
            let mut out = Self::default();
            let valid = events
                .into_iter()
                .filter(|e| parse_decision(e).is_ok_and(|d| d.buyer == buyer));
            for e in latest_per_coordinate(valid) {
                let Ok(d) = parse_decision(e) else { continue };
                match d.swarm.clone() {
                    Some(swarm) => {
                        out.per_swarm.insert((d.seller.clone(), swarm), d);
                    }
                    None => {
                        out.buyer_wide.insert(d.seller.clone(), d);
                    }
                }
            }
            out
        }
        pub fn buyer_wide(&self, seller: &str) -> Option<&str> {
            self.buyer_wide.get(seller).map(|d| d.decision.as_str())
        }
        pub fn for_swarm(&self, seller: &str, swarm: &str) -> Option<&str> {
            self.per_swarm
                .get(&(seller.to_owned(), swarm.to_owned()))
                .map(|d| d.decision.as_str())
        }
        /// May `seller` take tasks in `swarm` (§4)?
        pub fn allows(&self, seller: &str, swarm: &str) -> bool {
            decision_allows(self.buyer_wide(seller), self.for_swarm(seller, swarm))
        }
        /// Sellers with an `accepted` decision that could admit them to `swarm` (buyer-wide or
        /// for that swarm; `None` = any swarm). Readers fetch memberships only for these authors.
        pub fn trusted(&self, swarm: Option<&str>) -> BTreeSet<String> {
            let wide = self
                .buyer_wide
                .values()
                .filter(|d| d.decision == "accepted")
                .map(|d| d.seller.clone());
            let scoped = self
                .per_swarm
                .iter()
                .filter(|((_, s), d)| swarm.is_none_or(|w| w == s) && d.decision == "accepted")
                .map(|((seller, _), _)| seller.clone());
            wide.chain(scoped).collect()
        }
    }

    /// The members a swarm job may give tasks to: the latest valid membership of the job's swarm,
    /// `joined`, with `since` and `created_at` at or before `starts`, and the buyer's decisions
    /// allowing the seller in that swarm.
    pub fn eligible_sellers<'a>(
        job: &SwarmJob,
        events: impl IntoIterator<Item = &'a Event>,
    ) -> Vec<Membership> {
        let events: Vec<&Event> = events.into_iter().collect();
        let decisions = Decisions::read(&job.buyer, events.iter().copied());
        latest_memberships(events)
            .into_iter()
            .filter(|(e, m)| {
                m.buyer == job.buyer
                    && m.swarm == job.swarm
                    && m.status == "joined"
                    && m.since <= job.starts
                    && e.created_at.as_secs() <= job.starts
                    && decisions.allows(&m.seller, &m.swarm)
            })
            .map(|(_, m)| m)
            .collect()
    }

    /// A seller clock up to this many seconds fast still gets its event accepted, so a running
    /// swarm job keeps counting a membership until `ends` plus this.
    pub const CREATED_AT_TOLERANCE: u64 = 900;

    /// The running swarm job (latest-ending, if several) in the membership's swarm that the
    /// current membership counts for, and that republishing it at `created` would forfeit.
    pub fn blocking_job(
        jobs: &[SwarmJob],
        membership: &Event,
        current: &Membership,
        now: u64,
        created: u64,
    ) -> Option<SwarmJob> {
        jobs.iter()
            .filter(|j| j.buyer == current.buyer && j.swarm == current.swarm && j.is_open())
            .filter(|j| created > j.starts && now <= j.ends.saturating_add(CREATED_AT_TOLERANCE))
            .filter(|j| membership.created_at.as_secs() <= j.starts && current.since <= j.starts)
            .max_by(|a, b| a.ends.cmp(&b.ends).then(b.id.cmp(&a.id)))
            .cloned()
    }

    /// An open swarm job already started at `now`: a membership published now counts only from
    /// the swarm's next job.
    pub fn running_job(jobs: &[SwarmJob], buyer: &str, swarm: &str, now: u64) -> Option<SwarmJob> {
        jobs.iter()
            .filter(|j| j.buyer == buyer && j.swarm == swarm && j.is_open())
            .filter(|j| j.starts < now && now <= j.ends)
            .max_by(|a, b| a.ends.cmp(&b.ends).then(b.id.cmp(&a.id)))
            .cloned()
    }
}
#[cfg(feature = "gateway")]
pub use signed::*;

#[cfg(test)]
mod tests {
    use super::*;
    const BUYER: &str = "abababababababababababababababababababababababababababababababab";
    const SELLER: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
    fn swarm() -> Swarm {
        Swarm {
            buyer: BUYER.into(),
            id: "math".into(),
            name: "Math".into(),
            purpose: "Hard problems".into(),
            status: "open".into(),
            tasks: "public".into(),
        }
    }
    fn job() -> SwarmJob {
        SwarmJob {
            buyer: BUYER.into(),
            swarm: "math".into(),
            id: "today".into(),
            title: "Build".into(),
            goal: "Goal".into(),
            starts: 3700,
            ends: 7300,
            budget_sats: 100,
            status: "scheduled".into(),
            update: None,
            result: None,
        }
    }
    fn membership() -> Membership {
        Membership {
            buyer: BUYER.into(),
            swarm: "math".into(),
            seller: SELLER.into(),
            since: 100,
            minutes: 60,
            status: "joined".into(),
            note: String::new(),
        }
    }
    fn decision(swarm: Option<&str>) -> Decision {
        Decision {
            buyer: BUYER.into(),
            seller: SELLER.into(),
            swarm: swarm.map(str::to_owned),
            decision: "accepted".into(),
            membership_event_id: None,
            reason: String::new(),
        }
    }
    fn set(d: &mut EventDraft, k: &str, v: &str) {
        d.tags.iter_mut().find(|t| t.first() == Some(k)).unwrap().0[1] = v.into();
    }
    fn tag_value<'a>(d: &'a EventDraft, k: &str) -> &'a str {
        d.tags.iter().find(|t| t.first() == Some(k)).unwrap().0[1].as_str()
    }
    #[test]
    fn drafts_roundtrip() {
        let s = swarm();
        assert_eq!(
            parse_swarm_draft(&s.to_event_draft().unwrap(), BUYER).unwrap(),
            s
        );
        let mut private = s.clone();
        private.tasks = "private".into();
        assert_eq!(
            parse_swarm_draft(&private.to_event_draft().unwrap(), BUYER).unwrap(),
            private
        );
        let j = job();
        assert_eq!(
            parse_swarm_job_draft(&j.to_event_draft().unwrap(), BUYER).unwrap(),
            j
        );
        let m = membership();
        assert_eq!(
            parse_membership_draft(&m.to_event_draft().unwrap(), SELLER).unwrap(),
            m
        );
        for scope in [None, Some("math")] {
            let d = decision(scope);
            assert_eq!(
                parse_decision_draft(&d.to_event_draft().unwrap(), BUYER).unwrap(),
                d
            );
        }
    }
    #[test]
    fn wire_shapes_match_the_spec() {
        let a = format!("30344:{BUYER}:math");
        assert_eq!(swarm().address(), a);
        let d = job().to_event_draft().unwrap();
        assert_eq!(d.kind, 30341);
        assert_eq!(tag_value(&d, "a"), a);
        let d = membership().to_event_draft().unwrap();
        assert_eq!(d.kind, 30342);
        assert_eq!(tag_value(&d, "d"), format!("{BUYER}:math"));
        assert_eq!(tag_value(&d, "a"), a);
        assert_eq!(tag_value(&d, "p"), BUYER);
        let d = decision(None).to_event_draft().unwrap();
        assert_eq!(d.kind, 30343);
        assert_eq!(tag_value(&d, "d"), SELLER);
        assert!(!d.tags.iter().any(|t| t.first() == Some("a")));
        let d = decision(Some("math")).to_event_draft().unwrap();
        assert_eq!(tag_value(&d, "d"), format!("{SELLER}:math"));
        assert_eq!(tag_value(&d, "a"), a);
        // A swarm without a `tasks` tag reads as public.
        let mut d = swarm().to_event_draft().unwrap();
        d.tags.retain(|t| t.first() != Some("tasks"));
        assert_eq!(parse_swarm_draft(&d, BUYER).unwrap().tasks, "public");
    }
    #[test]
    fn every_required_tag_and_duplicates_refused() {
        let optional_tags = ["tasks"];
        for original in [
            swarm().to_event_draft().unwrap(),
            job().to_event_draft().unwrap(),
            membership().to_event_draft().unwrap(),
            decision(None).to_event_draft().unwrap(),
            decision(Some("math")).to_event_draft().unwrap(),
        ] {
            let parse = |d: &EventDraft| match original.kind {
                SWARM_KIND => parse_swarm_draft(d, BUYER).map(|_| ()),
                SWARM_JOB_KIND => parse_swarm_job_draft(d, BUYER).map(|_| ()),
                SWARM_MEMBERSHIP_KIND => parse_membership_draft(d, SELLER).map(|_| ()),
                _ => parse_decision_draft(d, BUYER).map(|_| ()),
            };
            for i in 0..original.tags.len() {
                let name = original.tags[i].first().unwrap();
                let mut d = original.clone();
                d.tags.remove(i);
                // Dropping a per-swarm decision's `a` leaves `d = <seller>:<swarm>`: refused too.
                if !optional_tags.contains(&name) {
                    assert!(parse(&d).is_err(), "kind {} without {name}", original.kind);
                }
                let mut d = original.clone();
                d.tags.push(d.tags[i].clone());
                assert!(
                    parse(&d).is_err(),
                    "kind {} duplicate {name}",
                    original.kind
                );
            }
            for (k, v) in [("t", "other"), ("v", "2")] {
                let mut d = original.clone();
                set(&mut d, k, v);
                assert!(parse(&d).is_err());
            }
            let mut d = original.clone();
            set(&mut d, "d", "bad:d:x");
            assert!(parse(&d).is_err());
            let mut d = original.clone();
            d.kind = 30345;
            assert!(parse(&d).is_err());
        }
    }
    #[test]
    fn addresses_must_agree_and_name_the_same_buyer() {
        let other = "ef".repeat(32);
        let other_swarm = format!("30344:{other}:math");
        // Swarm job: `a` must be a swarm by the job's author.
        let mut d = job().to_event_draft().unwrap();
        set(&mut d, "a", &other_swarm);
        assert!(parse_swarm_job_draft(&d, BUYER).is_err());
        for bad in [
            format!("30341:{BUYER}:math"),
            format!("30344:{BUYER}:Math"),
            format!("30344:{BUYER}"),
            "30344:nothex:math".to_owned(),
        ] {
            let mut d = job().to_event_draft().unwrap();
            set(&mut d, "a", &bad);
            assert!(parse_swarm_job_draft(&d, BUYER).is_err(), "{bad}");
        }
        assert!(parse_swarm_job_draft(&job().to_event_draft().unwrap(), &other).is_err());
        assert!(parse_swarm_draft(&swarm().to_event_draft().unwrap(), "nothex").is_err());
        // Membership: `a`, `p` and `d` must agree.
        for (k, v) in [
            ("p", other.clone()),
            ("d", format!("{BUYER}:other")),
            ("d", BUYER.to_owned()),
            ("a", other_swarm.clone()),
            ("a", format!("30344:{BUYER}:other")),
        ] {
            let mut d = membership().to_event_draft().unwrap();
            set(&mut d, k, &v);
            assert!(parse_membership_draft(&d, SELLER).is_err(), "{k}={v}");
        }
        // A membership without an `a` (the old buyer-wide shape) is refused.
        let mut d = membership().to_event_draft().unwrap();
        d.tags.retain(|t| t.first() != Some("a"));
        set(&mut d, "d", BUYER);
        assert!(parse_membership_draft(&d, SELLER).is_err());
        // Decisions: per-swarm `d` suffix, `a` and `p` agree; the swarm is the author's.
        for (k, v) in [
            ("d", format!("{SELLER}:other")),
            ("d", SELLER.to_owned()),
            ("a", other_swarm.clone()),
            ("p", other.clone()),
        ] {
            let mut d = decision(Some("math")).to_event_draft().unwrap();
            set(&mut d, k, &v);
            assert!(parse_decision_draft(&d, BUYER).is_err(), "{k}={v}");
        }
        // A buyer-wide decision carries no `a`, and its `d` is exactly the seller.
        let mut d = decision(None).to_event_draft().unwrap();
        set(&mut d, "d", &format!("{SELLER}:math"));
        assert!(parse_decision_draft(&d, BUYER).is_err());
        let mut d = decision(None).to_event_draft().unwrap();
        set(&mut d, "p", "NOT-HEX");
        assert!(parse_decision_draft(&d, BUYER).is_err());
        let mut d = decision(None);
        d.membership_event_id = Some("bad".into());
        assert!(d.to_event_draft().is_err());
    }
    #[test]
    fn numbers_times_statuses_and_ids_refused() {
        for bad in ["0", "481", "-1", "+1", "1.5", "18446744073709551616", ""] {
            let mut d = membership().to_event_draft().unwrap();
            set(&mut d, "minutes", bad);
            assert!(parse_membership_draft(&d, SELLER).is_err(), "{bad}");
        }
        for n in [1, 480] {
            let mut m = membership();
            m.minutes = n;
            assert!(m.to_event_draft().is_ok());
        }
        for (k, v) in [
            ("starts", "-1"),
            ("ends", "3700"),
            ("budget", "-1"),
            ("status", "unknown"),
            ("title", " "),
            ("d", ""),
            ("d", "ABC"),
            ("d", &"x".repeat(65)),
        ] {
            let mut d = job().to_event_draft().unwrap();
            set(&mut d, k, v);
            assert!(parse_swarm_job_draft(&d, BUYER).is_err(), "{k}: {v}");
        }
        let mut d = job().to_event_draft().unwrap();
        d.tags
            .iter_mut()
            .find(|t| t.first() == Some("budget"))
            .unwrap()
            .0[2] = "usd".into();
        assert!(parse_swarm_job_draft(&d, BUYER).is_err());
        for (k, v) in [
            ("name", " "),
            ("name", &"n".repeat(129)),
            ("status", "archived"),
            ("tasks", "secret"),
            ("d", "Math"),
        ] {
            let mut d = swarm().to_event_draft().unwrap();
            set(&mut d, k, v);
            assert!(parse_swarm_draft(&d, BUYER).is_err(), "{k}: {v}");
        }
        let mut s = swarm();
        s.name = "n".repeat(128);
        assert!(s.to_event_draft().is_ok());
        let mut m = membership();
        m.status = "unknown".into();
        assert!(m.to_event_draft().is_err());
        let mut d = decision(None);
        d.decision = "maybe".into();
        assert!(d.to_event_draft().is_err());
    }
    #[test]
    fn decision_vectors() {
        // (buyer-wide, per swarm) -> eligible in that swarm.
        for (wide, scoped, want) in [
            (None, None, false),
            (Some("accepted"), None, true),
            (None, Some("accepted"), true),
            (Some("accepted"), Some("denied"), false),
            (Some("denied"), Some("accepted"), false),
            (Some("denied"), None, false),
            (None, Some("denied"), false),
            (Some("accepted"), Some("accepted"), true),
        ] {
            assert_eq!(decision_allows(wide, scoped), want, "{wide:?} {scoped:?}");
        }
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
        struct World {
            buyer: Keys,
            seller: Keys,
            job: SwarmJob,
            m: Membership,
        }
        fn world() -> World {
            let buyer = Keys::generate();
            let seller = Keys::generate();
            let mut j = job();
            j.buyer = buyer.public_key().to_hex();
            let mut m = membership();
            m.buyer = j.buyer.clone();
            m.seller = seller.public_key().to_hex();
            World {
                buyer,
                seller,
                job: j,
                m,
            }
        }
        impl World {
            fn decide(&self, swarm: Option<&str>, decision: &str, t: u64) -> Event {
                let d = Decision {
                    buyer: self.job.buyer.clone(),
                    seller: self.m.seller.clone(),
                    swarm: swarm.map(str::to_owned),
                    decision: decision.into(),
                    membership_event_id: None,
                    reason: String::new(),
                };
                sign(d.to_event_draft().unwrap(), &self.buyer, t)
            }
            fn join(&self, m: &Membership, t: u64) -> Event {
                sign(m.to_event_draft().unwrap(), &self.seller, t)
            }
        }
        #[test]
        fn decision_vectors_on_signed_events() {
            let w = world();
            let joined = w.join(&w.m, 100);
            let n = |events: &[&Event]| eligible_sellers(&w.job, events.iter().copied()).len();
            // Buyer-wide accept admits; a per-swarm deny excludes from that swarm only.
            let wide = w.decide(None, "accepted", 100);
            assert_eq!(n(&[&joined, &wide]), 1);
            let deny_math = w.decide(Some("math"), "denied", 100);
            assert_eq!(n(&[&joined, &wide, &deny_math]), 0);
            let mut physics = w.job.clone();
            physics.swarm = "physics".into();
            let mut m = w.m.clone();
            m.swarm = "physics".into();
            let joined_physics = w.join(&m, 100);
            assert_eq!(
                eligible_sellers(&physics, [&joined, &joined_physics, &wide, &deny_math]).len(),
                1
            );
            // A per-swarm accept alone admits to that swarm only.
            let accept_math = w.decide(Some("math"), "accepted", 100);
            assert_eq!(n(&[&joined, &accept_math]), 1);
            assert!(eligible_sellers(&physics, [&joined_physics, &accept_math]).is_empty());
            // A buyer-wide deny wins over a per-swarm accept.
            let wide_deny = w.decide(None, "denied", 101);
            assert_eq!(n(&[&joined, &accept_math, &wide_deny]), 0);
            assert_eq!(n(&[&joined, &wide, &accept_math, &wide_deny]), 0);
            // Latest per coordinate: a later per-swarm accept replaces the per-swarm deny.
            let re_accept = w.decide(Some("math"), "accepted", 101);
            assert_eq!(n(&[&joined, &wide, &deny_math, &re_accept]), 1);
        }
        #[test]
        fn membership_is_per_swarm() {
            let w = world();
            let wide = w.decide(None, "accepted", 100);
            let mut m = w.m.clone();
            m.swarm = "physics".into();
            let physics_only = w.join(&m, 100);
            assert!(eligible_sellers(&w.job, [&physics_only, &wide]).is_empty());
            // Leaving one swarm leaves the other untouched.
            let joined = w.join(&w.m, 100);
            m.status = "left".into();
            let left_physics = w.join(&m, 101);
            assert_eq!(
                eligible_sellers(&w.job, [&joined, &physics_only, &left_physics, &wide]),
                vec![w.m.clone()]
            );
            // Another buyer's decisions never count.
            let stranger = Keys::generate();
            let d = Decision {
                buyer: stranger.public_key().to_hex(),
                seller: w.m.seller.clone(),
                swarm: None,
                decision: "accepted".into(),
                membership_event_id: None,
                reason: String::new(),
            };
            let forged = sign(d.to_event_draft().unwrap(), &stranger, 100);
            assert!(eligible_sellers(&w.job, [&joined, &forged]).is_empty());
            // A seller cannot self-accept.
            let mut d = Decision {
                buyer: w.m.seller.clone(),
                ..d
            };
            d.buyer = w.m.seller.clone();
            let own = sign(d.to_event_draft().unwrap(), &w.seller, 100);
            assert!(eligible_sellers(&w.job, [&joined, &own]).is_empty());
        }
        #[test]
        fn oversized_replacement_preserves_left_and_minutes_but_drops_note() {
            let w = world();
            let joined = w.join(&w.m, 100);
            let accepted = w.decide(None, "accepted", 100);
            assert_eq!(eligible_sellers(&w.job, [&joined, &accepted]).len(), 1);
            let mut m = w.m.clone();
            m.status = "left".into();
            m.minutes = 20;
            for padded_tags in [false, true] {
                let mut draft = m.to_event_draft().unwrap();
                draft.content = if padded_tags {
                    "short".into()
                } else {
                    "x".repeat(4097)
                };
                if padded_tags {
                    draft.tags.push(TagSpec::new(["extra", &"x".repeat(16385)]));
                }
                let left = sign(draft, &w.seller, 101);
                let parsed = parse_membership(&left).unwrap();
                assert_eq!(parsed.status, "left");
                assert_eq!(parsed.minutes, 20);
                assert_eq!(parsed.since, m.since);
                assert!(parsed.note.is_empty());
                assert!(eligible_sellers(&w.job, [&joined, &accepted, &left]).is_empty());
            }
        }
        #[test]
        fn accepted_seller_after_300_strangers_remains_eligible() {
            let w = world();
            let mut events = Vec::new();
            for status in ["joined", "left"] {
                for _ in 0..150 {
                    let key = Keys::generate();
                    let mut m = w.m.clone();
                    m.seller = key.public_key().to_hex();
                    m.status = status.into();
                    events.push(sign(m.to_event_draft().unwrap(), &key, 100));
                }
            }
            events.push(w.join(&w.m, 100));
            events.push(w.decide(None, "accepted", 100));
            assert_eq!(eligible_sellers(&w.job, &events), vec![w.m.clone()]);
            let decisions = Decisions::read(&w.job.buyer, &events);
            assert_eq!(decisions.trusted(Some("math")).len(), 1);
        }
        #[test]
        fn trusted_roster_scopes_by_swarm() {
            let w = world();
            let math = w.decide(Some("math"), "accepted", 100);
            let d = Decisions::read(&w.job.buyer, [&math]);
            assert!(d.trusted(Some("math")).contains(&w.m.seller));
            assert!(d.trusted(Some("physics")).is_empty());
            assert!(d.trusted(None).contains(&w.m.seller));
            let deny = w.decide(None, "denied", 100);
            let d = Decisions::read(&w.job.buyer, [&deny]);
            assert!(d.trusted(None).is_empty());
        }
        #[test]
        fn signatures_and_authors_required() {
            let w = world();
            let mut s = swarm();
            s.buyer = w.job.buyer.clone();
            let mut e = sign(s.to_event_draft().unwrap(), &w.buyer, 100);
            assert_eq!(parse_swarm(&e).unwrap().buyer, w.job.buyer);
            e.content.push('x');
            assert!(parse_swarm(&e).is_err());
            let mut e = sign(w.job.to_event_draft().unwrap(), &w.buyer, 100);
            assert!(parse_swarm_job(&e).is_ok());
            // Signed by the seller, the same job names another buyer's swarm.
            let forged = sign(w.job.to_event_draft().unwrap(), &w.seller, 100);
            assert!(parse_swarm_job(&forged).is_err());
            e.sig = forged.sig;
            assert!(parse_swarm_job(&e).is_err());
            let mut e = w.join(&w.m, 100);
            assert!(parse_membership(&e).is_ok());
            e.sig = sign(w.job.to_event_draft().unwrap(), &w.buyer, 100).sig;
            assert!(parse_membership(&e).is_err());
        }
        #[test]
        fn membership_persists_across_jobs_late_join_waits_and_late_leave_wins() {
            let w = world();
            let accepted = w.decide(None, "accepted", 100);
            let joined = w.join(&w.m, 100);
            let mut day2 = w.job.clone();
            day2.id = "tomorrow".into();
            day2.starts += 86400;
            day2.ends += 86400;
            for day in [&w.job, &day2] {
                assert_eq!(
                    eligible_sellers(day, [&joined, &accepted]),
                    vec![w.m.clone()]
                );
            }
            // A late replacement cannot borrow an earlier seller-chosen epoch.
            let mut m = w.m.clone();
            m.minutes = 120;
            let changed = w.join(&m, 9000);
            assert!(eligible_sellers(&w.job, [&joined, &changed, &accepted]).is_empty());
            m.since = w.job.starts;
            let boundary = w.join(&m, w.job.starts);
            assert_eq!(eligible_sellers(&w.job, [&boundary, &accepted]).len(), 1);
            m.since += 1;
            let late = w.join(&m, 10001);
            assert!(eligible_sellers(&w.job, [&joined, &late, &accepted]).is_empty());
            assert_eq!(eligible_sellers(&day2, [&late, &accepted]).len(), 1);
            m.status = "left".into();
            let left = w.join(&m, 10002);
            assert!(eligible_sellers(&day2, [&late, &left, &accepted]).is_empty());
            let mut invalid = left.clone();
            invalid.content.push('x');
            assert_eq!(
                eligible_sellers(&day2, [&late, &invalid, &accepted]).len(),
                1
            );
        }
        #[test]
        fn latest_tie_break() {
            let w = world();
            let accepted = w.decide(None, "accepted", 100);
            let tied = w.decide(None, "denied", 100);
            let winner = if tied.id < accepted.id {
                &tied
            } else {
                &accepted
            };
            assert_eq!(latest_per_coordinate([&tied, &accepted]), vec![winner]);
            assert_eq!(latest_per_coordinate([&accepted, &tied]), vec![winner]);
        }
        #[test]
        fn blocking_and_running_jobs() {
            let w = world();
            let joined = w.join(&w.m, 100);
            let mut running = w.job.clone();
            running.status = "in_progress".into();
            let mut other_swarm = running.clone();
            other_swarm.swarm = "physics".into();
            other_swarm.ends += 1000;
            let jobs = vec![running.clone(), other_swarm];
            let now = running.starts + 10;
            // Counted in the running job: a republish now would forfeit it.
            assert_eq!(
                blocking_job(&jobs, &joined, &w.m, now, now),
                Some(running.clone())
            );
            // After ends + tolerance it no longer blocks.
            let late = running.ends + CREATED_AT_TOLERANCE + 1;
            assert_eq!(blocking_job(&jobs, &joined, &w.m, late, late), None);
            // A membership published after starts never counted, so nothing is forfeited.
            let fresh = w.join(&w.m, now);
            assert_eq!(blocking_job(&jobs, &fresh, &w.m, now, now + 1), None);
            // A closed job does not block.
            let mut closed = running.clone();
            closed.status = "completed".into();
            assert_eq!(blocking_job(&[closed], &joined, &w.m, now, now), None);
            assert_eq!(
                running_job(&jobs, &w.job.buyer, "math", now),
                Some(running.clone())
            );
            assert_eq!(
                running_job(&jobs, &w.job.buyer, "math", running.starts - 1),
                None
            );
            assert_eq!(running_job(&jobs, &w.job.buyer, "chem", now), None);
        }
    }
}
