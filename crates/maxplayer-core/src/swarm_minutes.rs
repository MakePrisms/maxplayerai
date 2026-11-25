//! Minutes per swarm per UTC day (docs/specs/swarm.md §4, §7.4): the Rust port of
//! `web/app/src/swarm/shared/model.mjs` (`taskFacts`, `taskAt`, `minutesToday`). Both run
//! `web/app/src/swarm/shared/vectors.json`, so the board and the buyer daemon count the same minutes.
//!
//! A task is an awarded offer of a swarm job (start = AWARD) or a later session turn on it (start =
//! FOLLOW-UP). Each runs until the buyer's ACCEPT of it, else until now, capped at its deadline;
//! seller-signed times never close the meter, and a result dated at or before the start counts the
//! whole window. Offers must carry the public `[swarm:<buyer>:<swarm job d>]\n` prefix; the legacy
//! `[challenge:…]` prefix is not read.
use std::collections::{BTreeMap, HashMap};

use nostr_sdk::Event;

use crate::kinds::{
    JOB_ACCEPT_KIND, JOB_AWARD_KIND, JOB_CLAIM_KIND, JOB_OFFER_KIND, JOB_RESULT_KIND,
    SESSION_FOLLOWUP_KIND,
};
use crate::swarm::{SwarmJob, valid_hex, valid_slug};

pub const DAY_SECS: u64 = 86_400;
/// Largest integer JS reads exactly (`Number.isSafeInteger`).
const SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Start of the UTC day holding `now`.
pub fn day_start(now: u64) -> u64 {
    now - now % DAY_SECS
}

/// `[swarm:<buyer>:<swarm job d>]\n`, the public task prefix (§4).
pub fn task_prefix(buyer: &str, job: &str) -> String {
    format!("[swarm:{buyer}:{job}]\n")
}

/// `(buyer, swarm job d, prefix length)` when `text` starts with a task prefix.
pub fn parse_task_prefix(text: &str) -> Option<(&str, &str, usize)> {
    let rest = text.strip_prefix("[swarm:")?;
    let buyer = rest.get(..64)?;
    let rest = rest.get(64..)?.strip_prefix(':')?;
    let end = rest.find(']')?;
    let job = &rest[..end];
    if !valid_hex(buyer) || !valid_slug(job) || !rest[end + 1..].starts_with('\n') {
        return None;
    }
    Some((buyer, job, "[swarm:".len() + 64 + 1 + end + 2))
}

/// One task of a swarm job: turn 1 (an awarded offer) or a session turn on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskFact {
    /// The offer id (turn 1) or the follow-up id (turn ≥ 2).
    pub id: String,
    pub job: String,
    pub swarm: String,
    pub seller: String,
    pub turn: u32,
    pub start: u64,
    pub deadline: u64,
    pub accepted_at: Option<u64>,
    pub backdated: bool,
}

impl TaskFact {
    /// Seconds this task used by `now` (`taskAt` in model.mjs).
    pub fn used_secs(&self, now: u64) -> u64 {
        let end = match self.accepted_at {
            Some(at) => at,
            None if self.backdated => self.deadline,
            None => now,
        };
        end.saturating_sub(self.start)
            .min(self.deadline.saturating_sub(self.start))
    }
    /// Still running at `now`: not accepted and before its deadline.
    pub fn running(&self, now: u64) -> bool {
        self.accepted_at.is_none() && now < self.deadline
    }
}

fn tags(e: &Event) -> impl Iterator<Item = &[String]> {
    e.tags.iter().map(|t| t.as_slice())
}
fn named<'a>(e: &'a Event, k: &str) -> Vec<&'a [String]> {
    tags(e)
        .filter(|t| t.first().map(String::as_str) == Some(k))
        .collect()
}
/// JS `tag`: the first tag named `k`, its value.
fn first<'a>(e: &'a Event, k: &str) -> Option<&'a str> {
    tags(e)
        .find(|t| t.first().map(String::as_str) == Some(k))
        .and_then(|t| t.get(1))
        .map(String::as_str)
}
fn one(e: &Event, k: &str) -> bool {
    named(e, k).len() == 1
}
fn scalar(e: &Event, k: &str) -> bool {
    let n = named(e, k);
    n.len() == 1 && n[0].len() == 2
}
fn namespace(e: &Event) -> bool {
    scalar(e, "t")
        && first(e, "t") == Some("maxplayer")
        && scalar(e, "v")
        && first(e, "v") == Some("1")
}
fn trade_namespace(e: &Event) -> bool {
    let kind = e.kind.as_u16();
    namespace(e)
        || ([
            JOB_OFFER_KIND,
            JOB_CLAIM_KIND,
            JOB_RESULT_KIND,
            JOB_AWARD_KIND,
            JOB_ACCEPT_KIND,
        ]
        .contains(&kind)
            && has_maxplayer_t(e)
            && one(e, "v")
            && first(e, "v") == Some("2")
            && one(e, "visibility")
            && first(e, "visibility") == Some("public")
            && one(e, "job")
            && first(e, "job").is_some_and(valid_hex))
}
fn has_maxplayer_t(e: &Event) -> bool {
    tags(e).any(|t| {
        t.first().map(String::as_str) == Some("t")
            && t.get(1).map(String::as_str) == Some("maxplayer")
    })
}
/// The single `e` tag whose marker passes `marker`, when it names a hex id.
fn e_ref(e: &Event, marker: impl Fn(Option<&str>) -> bool) -> Option<String> {
    let refs: Vec<&[String]> = tags(e)
        .filter(|t| {
            t.first().map(String::as_str) == Some("e") && marker(t.get(3).map(String::as_str))
        })
        .collect();
    match refs.as_slice() {
        [only] => only.get(1).filter(|v| valid_hex(v)).cloned(),
        _ => None,
    }
}
fn root(e: &Event) -> Option<String> {
    e_ref(e, |m| m == Some("root"))
}
fn claim_ref(e: &Event) -> Option<String> {
    e_ref(e, |m| m.is_none_or(str::is_empty))
}
fn reply_to(e: &Event) -> Option<String> {
    e_ref(e, |m| m == Some("reply"))
}
fn has_reply(e: &Event) -> bool {
    tags(e).any(|t| {
        t.first().map(String::as_str) == Some("e") && t.get(3).map(String::as_str) == Some("reply")
    })
}
fn mentions(e: &Event, pk: &str) -> bool {
    tags(e).any(|t| {
        t.first().map(String::as_str) == Some("p") && t.get(1).map(String::as_str) == Some(pk)
    })
}
fn safe_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok().filter(|n| *n <= SAFE_INTEGER)
}
fn deadline_of(e: &Event) -> Option<u64> {
    let d: Vec<&[String]> = tags(e)
        .filter(|t| {
            t.first().map(String::as_str) == Some("param")
                && t.get(1).map(String::as_str) == Some("deadline")
        })
        .collect();
    match d.as_slice() {
        [only] => only.get(2).and_then(|v| safe_number(v)),
        _ => None,
    }
}
/// The earliest event: lowest `created_at`, a tie to the lower id.
fn earliest<'a>(events: impl IntoIterator<Item = &'a Event>) -> Option<&'a Event> {
    events.into_iter().min_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.id.to_hex().cmp(&b.id.to_hex()))
    })
}

/// Every task of `jobs` (one buyer's parsed swarm jobs): one fact per awarded offer (turn 1) and per
/// session turn on it. Time-independent; [`TaskFact::used_secs`] applies `now`.
pub fn task_facts<'a>(
    events: impl IntoIterator<Item = &'a Event>,
    buyer: &str,
    jobs: &[SwarmJob],
) -> Vec<TaskFact> {
    let window: HashMap<&str, &SwarmJob> = jobs
        .iter()
        .filter(|j| j.buyer == buyer)
        .map(|j| (j.id.as_str(), j))
        .collect();
    let mut by_root: HashMap<String, Vec<&Event>> = HashMap::new();
    let mut offers = Vec::new();
    for e in events {
        let kind = e.kind.as_u16();
        if kind == JOB_OFFER_KIND {
            if e.pubkey.to_hex() == buyer && trade_namespace(e) {
                offers.push(e);
            }
            continue;
        }
        if ![
            JOB_CLAIM_KIND,
            JOB_AWARD_KIND,
            JOB_RESULT_KIND,
            JOB_ACCEPT_KIND,
            SESSION_FOLLOWUP_KIND,
        ]
        .contains(&kind)
        {
            continue;
        }
        if let Some(r) = root(e) {
            by_root.entry(r).or_default().push(e);
        }
    }
    let mut facts = Vec::new();
    for offer in offers {
        if !one(offer, "i") || !one(offer, "p") || !first(offer, "p").is_some_and(valid_hex) {
            continue;
        }
        let Some((prefix_buyer, job_d, _)) = first(offer, "i").and_then(parse_task_prefix) else {
            continue;
        };
        let Some(job) = (prefix_buyer == buyer).then(|| window.get(job_d)).flatten() else {
            continue;
        };
        let created = offer.created_at.as_secs();
        if created < job.starts || created > job.ends {
            continue;
        }
        let Some(deadline) = deadline_of(offer) else {
            continue;
        };
        let seller = first(offer, "p").unwrap_or_default().to_owned();
        let offer_id = offer.id.to_hex();
        let related: &[&Event] = by_root.get(&offer_id).map(Vec::as_slice).unwrap_or(&[]);
        let of_kind = |kind: u16| {
            related
                .iter()
                .copied()
                .filter(move |e| e.kind.as_u16() == kind)
        };
        let claims: Vec<String> = of_kind(JOB_CLAIM_KIND)
            .filter(|e| {
                e.pubkey.to_hex() == seller
                    && trade_namespace(e)
                    && mentions(e, buyer)
                    && first(e, "status") == Some("processing")
            })
            .map(|e| e.id.to_hex())
            .collect();
        let Some(award) = earliest(of_kind(JOB_AWARD_KIND).filter(|e| {
            e.pubkey.to_hex() == buyer
                && trade_namespace(e)
                && first(e, "status") == Some("accepted")
                && mentions(e, buyer)
                && mentions(e, &seller)
                && claim_ref(e).is_some_and(|c| claims.contains(&c))
        })) else {
            continue;
        };
        let award_claim = claim_ref(award);
        let award_at = award.created_at.as_secs();
        let results: Vec<&Event> = of_kind(JOB_RESULT_KIND)
            .filter(|e| e.pubkey.to_hex() == seller && trade_namespace(e))
            .collect();
        let accepts: Vec<&Event> = of_kind(JOB_ACCEPT_KIND)
            .filter(|e| {
                e.pubkey.to_hex() == buyer
                    && trade_namespace(e)
                    && claim_ref(e) == award_claim
                    && mentions(e, buyer)
                    && mentions(e, &seller)
                    && first(e, "status") == Some("accepted")
            })
            .collect();
        // Turn 1: its result carries no `reply`; its ACCEPT names no reply, or names a turn-1 result.
        let first_results: Vec<String> = results
            .iter()
            .filter(|r| !has_reply(r))
            .map(|r| r.id.to_hex())
            .collect();
        let result = earliest(
            results
                .iter()
                .copied()
                .filter(|r| first_results.contains(&r.id.to_hex())),
        );
        let accept = earliest(accepts.iter().copied().filter(|a| {
            a.created_at.as_secs() >= award_at
                && (!has_reply(a) || reply_to(a).is_some_and(|r| first_results.contains(&r)))
        }));
        facts.push(TaskFact {
            id: offer_id.clone(),
            job: job.id.clone(),
            swarm: job.swarm.clone(),
            seller: seller.clone(),
            turn: 1,
            start: award_at,
            deadline,
            accepted_at: accept.map(|a| a.created_at.as_secs()),
            backdated: result.is_some_and(|r| r.created_at.as_secs() <= award_at),
        });
        // Later turns: buyer FOLLOW-UP (status turn) to this seller, ended by the ACCEPT of its result.
        for f in of_kind(SESSION_FOLLOWUP_KIND) {
            let at = f.created_at.as_secs();
            if f.pubkey.to_hex() != buyer
                || !has_maxplayer_t(f)
                || first(f, "status") != Some("turn")
                || !one(f, "p")
                || first(f, "p") != Some(seller.as_str())
                || at < award_at
            {
                continue;
            }
            let turn = if one(f, "turn") {
                first(f, "turn").and_then(safe_number)
            } else {
                None
            };
            let (Some(turn), Some(turn_deadline)) = (turn, deadline_of(f)) else {
                continue;
            };
            if turn < 2 {
                continue;
            }
            let fid = f.id.to_hex();
            let answers: Vec<&Event> = results
                .iter()
                .copied()
                .filter(|r| reply_to(r).as_deref() == Some(fid.as_str()))
                .collect();
            let ids: Vec<String> = answers.iter().map(|r| r.id.to_hex()).collect();
            let answer = earliest(answers.iter().copied());
            let done = earliest(accepts.iter().copied().filter(|a| {
                a.created_at.as_secs() >= at && reply_to(a).is_some_and(|r| ids.contains(&r))
            }));
            facts.push(TaskFact {
                id: fid,
                job: job.id.clone(),
                swarm: job.swarm.clone(),
                seller: seller.clone(),
                turn: u32::try_from(turn).unwrap_or(u32::MAX),
                start: at,
                deadline: turn_deadline,
                accepted_at: done.map(|a| a.created_at.as_secs()),
                backdated: answer.is_some_and(|r| r.created_at.as_secs() <= at),
            });
        }
    }
    facts
}

/// Seconds each seller used in swarm `swarm` (its `d`) in the UTC day holding `now`.
pub fn used_secs_today(facts: &[TaskFact], swarm: &str, now: u64) -> BTreeMap<String, u64> {
    let from = day_start(now);
    let mut out = BTreeMap::new();
    for f in facts {
        if f.swarm == swarm && f.start >= from && f.start < from + DAY_SECS {
            *out.entry(f.seller.clone()).or_insert(0) += f.used_secs(now);
        }
    }
    out
}

/// Minutes each seller used in `swarm` in the UTC day holding `now` (`minutesToday`).
pub fn minutes_today(facts: &[TaskFact], swarm: &str, now: u64) -> BTreeMap<String, f64> {
    used_secs_today(facts, swarm, now)
        .into_iter()
        .map(|(seller, secs)| (seller, secs as f64 / 60.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::{eligible_sellers, latest_swarm_jobs};
    use nostr_sdk::JsonUtil;
    use serde_json::Value;

    /// The board's file, read at compile time: JS and Rust run the same vectors (§7.4).
    const VECTORS: &str = include_str!("../../../web/app/src/swarm/shared/vectors.json");

    fn split(key: &str) -> (&str, &str) {
        key.split_once(':').expect("<buyer>:<d>")
    }

    fn cases() -> Vec<(String, u64, Vec<Event>, Value)> {
        let v: Value = serde_json::from_str(VECTORS).expect("vectors parse");
        v["cases"]
            .as_array()
            .expect("cases")
            .iter()
            .map(|c| {
                let events = c["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| Event::from_json(e.to_string()).expect("event"))
                    .collect::<Vec<_>>();
                (
                    c["name"].as_str().unwrap().to_owned(),
                    c["now"].as_u64().unwrap(),
                    events,
                    c["expect"].clone(),
                )
            })
            .collect()
    }

    #[test]
    fn shared_vectors_pass_in_rust() {
        let cases = cases();
        assert_eq!(cases.len(), 2, "both shared cases are read");
        let (mut eligible_checked, mut minutes_checked) = (0, 0);
        for (name, now, events, expect) in cases {
            assert!(
                events.iter().all(|e| e.verify().is_ok()),
                "{name}: every event is signed"
            );
            let jobs: Vec<SwarmJob> = latest_swarm_jobs(&events)
                .into_iter()
                .map(|(_, j)| j)
                .collect();
            if let Some(eligible) = expect["eligible"].as_object() {
                for (key, want) in eligible {
                    let (buyer, d) = split(key);
                    let job = jobs
                        .iter()
                        .find(|j| j.buyer == buyer && j.id == d)
                        .unwrap_or_else(|| panic!("{name}: job {key}"));
                    let mut got: Vec<String> = eligible_sellers(job, &events)
                        .into_iter()
                        .map(|m| m.seller)
                        .collect();
                    got.sort();
                    let want: Vec<String> = serde_json::from_value(want.clone()).unwrap();
                    assert_eq!(got, want, "{name}: eligible in {key}");
                    eligible_checked += 1;
                }
            }
            if let Some(minutes) = expect["minutesToday"].as_object() {
                for (key, want) in minutes {
                    let (buyer, d) = split(key);
                    let facts = task_facts(&events, buyer, &jobs);
                    let got = minutes_today(&facts, d, now);
                    let want: BTreeMap<String, f64> = serde_json::from_value(want.clone()).unwrap();
                    assert_eq!(got, want, "{name}: minutes in {key}");
                    minutes_checked += 1;
                }
            }
        }
        assert_eq!((eligible_checked, minutes_checked), (2, 2));
    }

    #[test]
    fn a_day_later_nothing_from_that_day_counts() {
        let (_, now, events, _) = cases().remove(1);
        let jobs: Vec<SwarmJob> = latest_swarm_jobs(&events)
            .into_iter()
            .map(|(_, j)| j)
            .collect();
        let buyer = jobs[0].buyer.clone();
        let facts = task_facts(&events, &buyer, &jobs);
        assert!(!minutes_today(&facts, "math", now).is_empty());
        assert!(minutes_today(&facts, "math", now + DAY_SECS).is_empty());
    }

    #[test]
    fn prefix_parses_only_the_swarm_form() {
        let b = "ab".repeat(32);
        let p = task_prefix(&b, "math-1");
        assert_eq!(
            parse_task_prefix(&format!("{p}text")),
            Some((b.as_str(), "math-1", p.len()))
        );
        assert_eq!(
            parse_task_prefix(&format!("[challenge:{b}:math-1]\ntext")),
            None
        );
        assert_eq!(parse_task_prefix(&format!("[swarm:{b}:Math]\ntext")), None);
        assert_eq!(parse_task_prefix(&format!("[swarm:{b}:math]text")), None);
    }
}
