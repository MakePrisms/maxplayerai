//! Community challenge commands. Untrusted relay text and keys never appear in diagnostics.
use std::collections::BTreeMap;
use std::io::Write;

const HELP: &str = "Usage: maxplayer-challenges <command> [--home <path>] [--json] [--relay URL ...]
  join --buyer <npub|hex> --minutes <1..480> [--note <text>]
  status --buyer <npub|hex> [--challenge <d>]
  leave --buyer <npub|hex>
  publish --id <d> --title <text> --goal-file <path> --starts <rfc3339> --ends <rfc3339> --budget-sats <n> [--status <scheduled|in_progress|completed|cancelled>] [--update <text>] [--result <text>]
  members [--challenge <d>]
  decide --seller <npub|hex> --accept|--deny
Join or leave at any time. Joining after work starts means eligibility from the next day.
Membership and acceptance persist across days. Minutes are offered per day.
Status/members default to the buyer's latest non-cancelled challenge. Uses the initialized home's key.
Home: $MAXPLAYER_HOME or ~/.maxplayer. Relays default to wss://nos.lol, wss://relay.primal.net, wss://offchain.pub.
Repeat --relay URL after the command to override. ws:// is loopback-only; relay.maxplayer.ai is forbidden.
Per-relay read/publish outcomes are printed to stderr; --json uses stdout.
";
/// A publish refused because the challenge is already closed on the relays exits with
/// `CLOSED_EXIT` and an error starting with `CHALLENGE_CLOSED` (nothing is signed or sent).
const CHALLENGE_CLOSED: &str = "challenge closed";
const CLOSED_EXIT: i32 = 3;
/// Authors per roster membership REQ, and the newest-first applicant sample size (matches the page).
const ROSTER_AUTHOR_CHUNK: usize = 100;
const APPLICANT_LIMIT: usize = 256;
#[derive(Debug)]
struct Opts {
    command: String,
    values: BTreeMap<String, String>,
    json: bool,
    relays: Vec<String>,
}
impl Opts {
    fn get(&self, k: &str) -> Option<&str> {
        self.values.get(k).map(String::as_str)
    }
    fn required(&self, k: &str) -> Result<&str, String> {
        self.get(k).ok_or_else(|| format!("missing {k}"))
    }
}
fn parse(args: &[String]) -> Result<Opts, String> {
    let command = args.first().ok_or("missing command")?.as_str();
    let allowed: &[&str] = match command {
        "join" => &["--buyer", "--minutes", "--note"],
        "status" => &["--buyer", "--challenge"],
        "leave" => &["--buyer"],
        "publish" => &[
            "--id",
            "--title",
            "--goal-file",
            "--starts",
            "--ends",
            "--budget-sats",
            "--status",
            "--update",
            "--result",
        ],
        "members" => &["--challenge"],
        "decide" => &["--seller", "--accept", "--deny"],
        _ => return Err("unknown challenge command".into()),
    };
    let mut o = Opts {
        command: command.into(),
        values: BTreeMap::new(),
        json: false,
        relays: Vec::new(),
    };
    let mut i = 1;
    while i < args.len() {
        let k = args[i].as_str();
        if !allowed.contains(&k) && !["--home", "--json", "--relay"].contains(&k) {
            return Err("unknown challenge option".into());
        }
        let v = if ["--accept", "--deny", "--json"].contains(&k) {
            String::new()
        } else {
            i += 1;
            args.get(i)
                .filter(|v| !v.starts_with("--"))
                .ok_or_else(|| format!("{k} requires a value"))?
                .clone()
        };
        if k == "--relay" {
            o.relays.push(v);
            i += 1;
            continue;
        }
        if o.values.insert(k.into(), v).is_some() {
            return Err(format!("duplicate {k}"));
        }
        i += 1;
    }
    o.json = o.get("--json").is_some();
    match command {
        "join" => {
            o.required("--buyer")?;
            let n = o
                .required("--minutes")?
                .parse::<u64>()
                .map_err(|_| "invalid minutes")?;
            if !(1..=480).contains(&n) {
                return Err("minutes must be 1..=480".into());
            }
        }
        "status" | "leave" => {
            o.required("--buyer")?;
        }
        "decide" => {
            o.required("--seller")?;
            if o.get("--accept").is_some() == o.get("--deny").is_some() {
                return Err("choose exactly one of --accept or --deny".into());
            }
        }
        "publish" => {
            for k in [
                "--id",
                "--title",
                "--goal-file",
                "--starts",
                "--ends",
                "--budget-sats",
            ] {
                o.required(k)?;
            }
        }
        _ => (),
    }
    Ok(o)
}
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if is_help_request(args) {
        let _ = write!(out, "{HELP}");
        return 0;
    }
    let opts = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            let _ = writeln!(err, "{e}\n{HELP}");
            return 1;
        }
    };
    {
        match execute(&opts) {
            Ok((body, warning)) => {
                if let Some(warning) = warning {
                    let _ = writeln!(err, "Warning: {warning}");
                }
                if opts.json {
                    let _ = writeln!(out, "{body}");
                } else {
                    let _ = writeln!(out, "{}", human(&body));
                }
                0
            }
            Err(e) => {
                let _ = writeln!(err, "challenge: {e}");
                if e.starts_with(CHALLENGE_CLOSED) {
                    CLOSED_EXIT
                } else {
                    2
                }
            }
        }
    }
}
fn human(body: &serde_json::Value) -> String {
    if let Some(id) = body.get("event_id").and_then(|v| v.as_str()) {
        return format!("Published challenge event {id}");
    }
    let mut lines = Vec::new();
    if let Some(c) = body.get("challenge").filter(|c| !c.is_null()) {
        lines.push(format!(
            "Challenge {}: {} ({})\nThe buyer: {}\nGoal: {}\nWork: {} – {}\nBudget: {} sat",
            c["id"].as_str().unwrap_or(""),
            c["title"].as_str().unwrap_or(""),
            c["status"].as_str().unwrap_or(""),
            c["buyer"].as_str().unwrap_or(""),
            c["goal"].as_str().unwrap_or(""),
            c["starts"],
            c["ends"],
            c["budget_sats"]
        ));
    }
    if let Some(rows) = body.get("members").and_then(|v| v.as_array()) {
        for row in rows {
            let a = &row["membership"];
            lines.push(format!(
                "{}: {} minutes, {}, decision {}, eligible {}",
                a["seller"].as_str().unwrap_or(""),
                a["minutes"],
                a["status"].as_str().unwrap_or(""),
                row["decision"].as_str().unwrap_or("pending"),
                row["eligible"]
            ));
        }
        if rows.is_empty() {
            lines.push("No membership found.".into());
        }
    }
    lines.join("\n")
}
fn public_key(s: &str) -> Result<nostr_sdk::PublicKey, String> {
    use nostr_sdk::prelude::*;
    // Explicitly restrict to the two documented encodings (not nsec, NIP-05, or URLs).
    if s.starts_with("npub1") {
        PublicKey::from_bech32(s).map_err(|_| ())
    } else {
        PublicKey::from_hex(s).map_err(|_| ())
    }
    .map_err(|_| "invalid public key: expected npub or hex".into())
}
fn timestamp(s: &str) -> Result<u64, String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).map_err(|_| "invalid RFC3339 timestamp")?;
    u64::try_from(dt.timestamp()).map_err(|_| "timestamp must be after unix epoch".into())
}
fn execute(o: &Opts) -> Result<(serde_json::Value, Option<String>), String> {
    use maxplayer_core::home;
    let root = o
        .get("--home")
        .map(std::path::PathBuf::from)
        .map(Ok)
        .unwrap_or_else(home::default_home_dir)
        .map_err(|_| "could not resolve home")?;
    if !home::is_initialized(&root) {
        return Err("home is not initialized; challenge never creates an identity".into());
    }
    let home = home::bootstrap(&root).map_err(|_| "could not load home")?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "could not start runtime")?;
    let result = rt.block_on(execute_async(o, &home))?;
    let secret = home::read_secret_key_hex(&home).map_err(|_| "could not read home key")?;
    if !secret.is_empty() && result.0.to_string().contains(&secret) {
        return Err("response would echo secret key".into());
    }
    Ok(result)
}
fn select_challenge(
    events: &[nostr_sdk::Event],
    buyer: &str,
    id: Option<&str>,
) -> Result<maxplayer_core::challenge::Challenge, String> {
    use maxplayer_core::challenge::*;
    let mut candidates: Vec<_> =
        latest_per_coordinate(events.iter().filter(|e| parse_challenge(e, buyer).is_ok()))
            .into_iter()
            .filter_map(|e| parse_challenge(e, buyer).ok().map(|c| (e, c)))
            .filter(|(_, c)| id.map_or(c.status != "cancelled", |id| c.id == id))
            .collect();
    candidates.sort_by(|(a, _), (b, _)| b.created_at.cmp(&a.created_at).then(a.id.cmp(&b.id)));
    candidates
        .into_iter()
        .next()
        .map(|(_, c)| c)
        .ok_or_else(|| "challenge not found or not verified for this buyer".into())
}
async fn execute_async(
    o: &Opts,
    home: &maxplayer_core::home::MaxplayerHome,
) -> Result<(serde_json::Value, Option<String>), String> {
    if o.command == "join" && o.get("--note").unwrap_or("").encode_utf16().count() > 4096 {
        return Err("join note exceeds 4096 UTF-16 code units (reader limit)".into());
    }
    let relay = crate::relay::Relays::new(&o.relays)?;
    use maxplayer_core::challenge::*;
    use nostr_sdk::prelude::*;
    use serde_json::json;
    let own = crate::relay::keys(home)?.public_key().to_hex();
    let buyer = o
        .get("--buyer")
        .map(public_key)
        .transpose()?
        .unwrap_or(public_key(&own)?)
        .to_hex();
    let now = Timestamp::now().as_secs();
    if o.command == "publish" {
        let starts = timestamp(o.required("--starts")?)?;
        let c = Challenge {
            buyer,
            id: o.required("--id")?.into(),
            title: o.required("--title")?.into(),
            goal: std::fs::read_to_string(o.required("--goal-file")?)
                .map_err(|_| "could not read goal file")?,
            starts,
            ends: timestamp(o.required("--ends")?)?,
            budget_sats: o
                .required("--budget-sats")?
                .parse()
                .map_err(|_| "invalid budget")?,
            status: o.get("--status").unwrap_or("scheduled").into(),
            update: o.get("--update").map(str::to_owned),
            result: o.get("--result").map(str::to_owned),
        };
        let draft = c.to_event_draft().map_err(|e| e.to_string())?;
        let previous = relay
            .fetch(
                home,
                Filter::new()
                    .author(public_key(&c.buyer)?)
                    .kind(Kind::from(CHALLENGE_KIND))
                    .identifier(&c.id),
            )
            .await
            .map_err(|e| e.to_string())?;
        // A completed/cancelled challenge is final: never sign a replacement over it, whatever
        // view the caller published from (its status read can be stale by the time we sign).
        if let Some(closed) = closed_status(&previous, &c.buyer, &c.id) {
            return Err(format!(
                "{CHALLENGE_CLOSED} ({closed}): the latest buyer-signed challenge {} is {closed}; nothing published",
                c.id
            ));
        }
        let created = replacement_timestamp(now, &previous, &own, CHALLENGE_KIND, &c.id)?;
        reject_secret_in_draft(home, &draft)?;
        let id = relay
            .publish(home, draft, created)
            .await
            .map_err(|e| e.to_string())?;
        return Ok((json!({"ok":true,"event_id":id,"challenge":c}), None));
    }
    // Membership operations do not depend on any daily challenge existing.
    let c = if matches!(o.command.as_str(), "status" | "members") {
        let challenges = relay
            .fetch(
                home,
                Filter::new()
                    .author(public_key(&buyer)?)
                    .kind(Kind::from(CHALLENGE_KIND)),
            )
            .await
            .map_err(|e| e.to_string())?;
        if challenges.is_empty() && o.get("--challenge").is_none() {
            None
        } else {
            Some(select_challenge(&challenges, &buyer, o.get("--challenge"))?)
        }
    } else {
        None
    };
    // Decisions first: the buyer-accepted roster scopes every membership read the eligible
    // view depends on (authors filter), so strangers' memberships cannot hide or crowd it out.
    let decision_events = relay
        .fetch(
            home,
            Filter::new()
                .kind(Kind::from(CHALLENGE_DECISION_KIND))
                .author(public_key(&buyer)?),
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut authors = accepted_sellers(&buyer, &decision_events);
    match o.command.as_str() {
        "join" | "leave" | "status" => {
            authors.insert(own.clone());
        }
        "decide" => {
            authors.insert(public_key(o.required("--seller")?)?.to_hex());
        }
        _ => {}
    }
    let mut events = Vec::new();
    let authors: Vec<_> = authors
        .iter()
        .map(|a| public_key(a))
        .collect::<Result<_, _>>()?;
    for chunk in authors.chunks(ROSTER_AUTHOR_CHUNK) {
        events.extend(
            relay
                .fetch(
                    home,
                    Filter::new()
                        .kind(Kind::from(CHALLENGE_MEMBERSHIP_KIND))
                        .authors(chunk.iter().copied())
                        .pubkey(public_key(&buyer)?),
                )
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    events.extend(decision_events);
    // Untrusted applicant sample for `members` only: newest APPLICANT_LIMIT joins so the buyer
    // can decide. It never feeds eligibility; a flood can only truncate this list.
    let (applicants, applicants_incomplete) = if o.command == "members" {
        let (sample, incomplete) = relay
            .fetch_sample(
                home,
                Filter::new()
                    .kind(Kind::from(CHALLENGE_MEMBERSHIP_KIND))
                    .pubkey(public_key(&buyer)?)
                    .limit(APPLICANT_LIMIT),
            )
            .await
            .map_err(|e| e.to_string())?;
        (sample, incomplete)
    } else {
        (Vec::new(), false)
    };
    // A sample event only fills a coordinate the trusted reads did not return (e.g. a denied
    // or undecided seller); it never replaces a roster membership.
    let coordinate = |e: &Event| {
        (
            e.pubkey,
            e.kind,
            e.tags.identifier().unwrap_or_default().to_owned(),
        )
    };
    let trusted: std::collections::BTreeSet<_> = events.iter().map(coordinate).collect();
    let latest = latest_per_coordinate(
        events
            .iter()
            .chain(
                applicants
                    .iter()
                    .filter(|e| !trusted.contains(&coordinate(e))),
            )
            .filter(|e| parse_membership(e, &buyer).is_ok() || parse_decision(e, &buyer).is_ok()),
    );
    let memberships: Vec<_> = latest
        .iter()
        .filter_map(|e| parse_membership(e, &buyer).ok().map(|a| (*e, a)))
        .collect();
    let decisions: Vec<_> = latest
        .iter()
        .filter_map(|e| parse_decision(e, &buyer).ok())
        .collect();
    let now = Timestamp::now().as_secs();
    let (kind, coordinate_d) = match o.command.as_str() {
        "decide" => (
            CHALLENGE_DECISION_KIND,
            public_key(o.required("--seller")?)?.to_hex(),
        ),
        _ => (CHALLENGE_MEMBERSHIP_KIND, buyer.clone()),
    };
    let created = if matches!(o.command.as_str(), "status" | "members") {
        now
    } else {
        replacement_timestamp(now, &events, &own, kind, &coordinate_d)?
    };
    let mut warning = None;
    let draft = match o.command.as_str() {
        "join" => {
            if !home.config.seller.as_ref().is_some_and(|s| s.accept_offers_only_from.contains(&buyer)) {
                warning = Some(format!("buyer {buyer} is not in seller.accept_offers_only_from; append it without removing existing entries and restart the seller."));
            }
            Membership {
                since: memberships.iter().find(|(_, a)| a.seller == own && a.status == "joined")
                    .map(|(_, a)| a.since).unwrap_or(created),
                seller: own.clone(),
                minutes: o.required("--minutes")?.parse().map_err(|_| "invalid minutes")?,
                status: "joined".into(),
                note: o.get("--note").unwrap_or("").into(),
            }.to_event_draft(&buyer)
        }
        "leave" => {
            let mut a = memberships.iter().find(|(_, a)| a.seller == own)
                .map(|(_, a)| a.clone()).ok_or("no verified membership to leave")?;
            a.status = "left".into();
            if a.note.encode_utf16().count() > 4096 { a.note.clear(); }
            a.to_event_draft(&buyer)
        }
        "decide" => {
            let seller = public_key(o.required("--seller")?)?.to_hex();
            Decision {
                membership_event_id: memberships.iter().find(|(_, a)| a.seller == seller)
                    .map(|(e, _)| e.id.to_hex()),
                seller,
                decision: if o.get("--accept").is_some() { "accepted" } else { "denied" }.into(),
                reason: String::new(),
            }.to_event_draft(&buyer)
        }
        _ => {
            // Trusted events only (decisions + roster-scoped memberships), never the sample.
            let eligible = c.as_ref().map(|c| eligible_sellers(c, &events)).unwrap_or_default();
            let rows: Vec<_> = memberships.iter()
                .filter(|(_, a)| o.command == "members" || a.seller == own)
                .map(|(e, a)| {
                    let decision = decisions.iter().find(|d| d.seller == a.seller).map(|d| d.decision.as_str());
                    json!({"event_id": e.id.to_hex(), "membership": a, "decision": decision,
                        "eligible": eligible.iter().any(|s| s.seller == a.seller)})
                }).collect();
            let mut body = json!({"challenge": c, "members": rows});
            if o.command == "members" {
                body["applicants_incomplete"] = json!(applicants_incomplete);
            }
            return Ok((body, None));
        }
    }.map_err(|e| e.to_string())?;
    reject_secret_in_draft(home, &draft)?;
    let id = relay
        .publish(home, draft, created)
        .await
        .map_err(|e| e.to_string())?;
    Ok((json!({"ok":true,"event_id":id,"challenge":c}), warning))
}

fn reject_secret_in_draft(
    home: &maxplayer_core::home::MaxplayerHome,
    draft: &maxplayer_core::gateway::EventDraft,
) -> Result<(), String> {
    let secret =
        maxplayer_core::home::read_secret_key_hex(home).map_err(|_| "could not read home key")?;
    if !secret.is_empty()
        && (draft.content.contains(&secret)
            || draft
                .tags
                .iter()
                .any(|tag| tag.0.iter().any(|value| value.contains(&secret))))
    {
        return Err("event would publish secret key".into());
    }
    Ok(())
}

/// Status of the latest valid buyer-signed CHALLENGE with this id, if it is completed/cancelled.
fn closed_status(events: &[nostr_sdk::Event], buyer: &str, id: &str) -> Option<String> {
    select_challenge(events, buyer, Some(id))
        .ok()
        .map(|c| c.status)
        .filter(|s| s == "completed" || s == "cancelled")
}

fn replacement_timestamp(
    now: u64,
    events: &[nostr_sdk::Event],
    author: &str,
    kind: u16,
    d: &str,
) -> Result<u64, String> {
    let previous = events
        .iter()
        .filter(|e| {
            e.pubkey.to_hex() == author
                && e.kind.as_u16() == kind
                && e.tags.identifier() == Some(d)
                && e.verify().is_ok()
        })
        .map(|e| e.created_at.as_secs())
        .max();
    let created = previous.map_or(now, |t| now.max(t.saturating_add(1)));
    if created > now.saturating_add(900) {
        return Err("existing event timestamp is too far in the future".into());
    }
    Ok(created)
}

fn is_help_request(args: &[String]) -> bool {
    match args.split_last() {
        Some((last, rest)) if last == "--help" => rest.iter().all(|s| !s.starts_with('-')),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }
    #[test]
    fn challenge_options_are_strict() {
        for s in [
            "join --buyer abc --minutes 1",
            "join --buyer abc --minutes 480 --json",
            "status --buyer abc",
            "leave --buyer abc",
            "members",
            "decide --seller abc --accept",
            "decide --seller abc --deny",
        ] {
            assert!(parse(&args(s)).is_ok(), "{s}");
        }
        for s in [
            "join --buyer abc --minutes 0",
            "join --buyer abc --minutes 481",
            "join --minutes 5",
            "status",
            "decide --seller abc",
            "decide --seller abc --accept --deny",
            "status --buyer abc --minutes 5",
            "status --buyer abc --buyer abc",
            "join --buyer abc --minutes",
            "publish --id today",
            "apply --buyer abc --minutes 60",
            "withdraw --buyer abc",
            "applicants",
            "join --buyer abc --minutes 60 --challenge today",
            "leave --buyer abc --challenge today",
            "decide --seller abc --accept --challenge today",
        ] {
            assert!(parse(&args(s)).is_err(), "{s}");
        }
    }
    #[test]
    fn challenge_help_is_side_effect_free() {
        for command in [
            "", "join", "status", "leave", "publish", "members", "decide",
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            assert_eq!(
                run(&args(&format!("{command} --help")), &mut out, &mut err),
                0
            );
            assert!(err.is_empty());
            assert!(
                String::from_utf8(out)
                    .unwrap()
                    .contains("maxplayer-challenges")
            );
        }
    }
    #[test]
    fn challenge_skill_commands_match_help_and_registration() {
        let skill = include_str!("../../../web/app/.well-known/skills/challenge-join/skill.md");
        for line in skill
            .lines()
            .filter(|line| line.trim().starts_with("maxplayer-challenges "))
        {
            let tokens: Vec<_> = line.split_whitespace().collect();
            assert!(["status", "join", "leave"].contains(&tokens[1]));
            for flag in tokens.iter().filter(|word| word.starts_with("--")) {
                assert!(HELP.contains(*flag), "undocumented skill flag {flag}");
            }
        }
        let index: serde_json::Value = serde_json::from_str(include_str!(
            "../../../web/app/.well-known/skills/index.json"
        ))
        .unwrap();
        assert!(
            index["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["path"] == "/.well-known/skills/challenge-join/skill.md")
        );
    }
    mod wallet {
        use super::*;
        use maxplayer_core::{challenge::*, home};
        use nostr_sdk::prelude::*;
        #[test]
        fn challenge_keys_times_and_missing_home() {
            let k = Keys::generate().public_key();
            assert_eq!(public_key(&k.to_hex()).unwrap(), k);
            assert_eq!(public_key(&k.to_bech32().unwrap()).unwrap(), k);
            assert!(public_key("nsec1invalid").is_err());
            assert_eq!(
                timestamp("2026-10-15T01:00:00+01:00").unwrap(),
                timestamp("2026-10-15T00:00:00Z").unwrap()
            );
            assert!(timestamp("1969-12-31T23:59:59Z").is_err());
            assert!(timestamp("2026-02-30T00:00:00Z").is_err());
            let root = std::env::temp_dir().join(format!("challenge-missing-{}", k.to_hex()));
            let o = parse(&args(&format!(
                "status --buyer {} --home {}",
                k.to_hex(),
                root.display()
            )))
            .unwrap();
            assert!(execute(&o).is_err());
            assert!(!root.exists());
        }
        #[test]
        fn challenge_replacement_timestamp_is_per_coordinate_and_bounded() {
            let keys = Keys::generate();
            let author = keys.public_key().to_hex();
            let event = EventBuilder::new(Kind::from(CHALLENGE_KIND), "")
                .tags([Tag::identifier("today")])
                .custom_created_at(Timestamp::from(100))
                .sign_with_keys(&keys)
                .unwrap();
            assert_eq!(
                replacement_timestamp(
                    100,
                    std::slice::from_ref(&event),
                    &author,
                    CHALLENGE_KIND,
                    "today"
                )
                .unwrap(),
                101
            );
            assert_eq!(
                replacement_timestamp(
                    100,
                    std::slice::from_ref(&event),
                    &author,
                    CHALLENGE_KIND,
                    "tomorrow"
                )
                .unwrap(),
                100
            );
            assert_eq!(
                replacement_timestamp(
                    200,
                    std::slice::from_ref(&event),
                    &author,
                    CHALLENGE_KIND,
                    "today"
                )
                .unwrap(),
                200
            );
            let future = EventBuilder::new(Kind::from(CHALLENGE_KIND), "")
                .tags([Tag::identifier("today")])
                .custom_created_at(Timestamp::from(1000))
                .sign_with_keys(&keys)
                .unwrap();
            assert!(
                replacement_timestamp(100, &[future], &author, CHALLENGE_KIND, "today").is_err()
            );
        }
        #[test]
        fn challenge_selection_verifies_author_and_latest_cancellation() {
            let buyer = Keys::generate();
            let other = Keys::generate();
            let now = Timestamp::now().as_secs();
            let c = Challenge {
                buyer: buyer.public_key().to_hex(),
                id: "today".into(),
                title: "Build".into(),
                goal: String::new(),
                starts: now + 3600,
                ends: now + 7200,
                budget_sats: 0,
                status: "scheduled".into(),
                update: None,
                result: None,
            };
            fn sign(c: &Challenge, k: &Keys, t: u64) -> Event {
                let d = c.to_event_draft().unwrap();
                EventBuilder::new(Kind::from(d.kind), d.content)
                    .tags(d.tags.into_iter().map(|t| Tag::parse(t.0).unwrap()))
                    .custom_created_at(Timestamp::from(t))
                    .sign_with_keys(k)
                    .unwrap()
            }
            let e = sign(&c, &buyer, now);
            assert!(select_challenge(&[], &c.buyer, None).is_err());
            assert!(select_challenge(&[sign(&c, &other, now)], &c.buyer, None).is_err());
            let mut tampered = e.clone();
            tampered.content.push('x');
            assert!(select_challenge(&[tampered], &c.buyer, None).is_err());
            let mut cancelled = c.clone();
            cancelled.status = "cancelled".into();
            let cancelled = sign(&cancelled, &buyer, now + 1);
            assert!(select_challenge(&[e.clone(), cancelled.clone()], &c.buyer, None).is_err());
            assert!(
                select_challenge(&[e, cancelled], &c.buyer, Some("today"))
                    .unwrap()
                    .status
                    == "cancelled"
            );
        }
        /// Advisor r1 #1: the caller's status view said in_progress; the operator closed the
        /// challenge before the sidecar's own pre-publish fetch. The publish is refused with the
        /// closed exit code and nothing is written, so the close stays the latest event.
        #[test]
        fn publish_never_replaces_a_close_seen_by_its_own_fetch() {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            for closed in ["cancelled", "completed"] {
                let root = std::env::temp_dir().join(format!(
                    "challenge-closed-{closed}-{}",
                    Keys::generate().public_key()
                ));
                let home = home::bootstrap(&root).unwrap();
                let keys = crate::relay::keys(&home).unwrap();
                let now = Timestamp::now().as_secs();
                let mut c = Challenge {
                    buyer: keys.public_key().to_hex(),
                    id: "today".into(),
                    title: "Build".into(),
                    goal: "goal".into(),
                    starts: now - 60,
                    ends: now + 3600,
                    budget_sats: 0,
                    status: "in_progress".into(),
                    update: None,
                    result: None,
                };
                let sign = |c: &Challenge, t: u64| {
                    let d = c.to_event_draft().unwrap();
                    EventBuilder::new(Kind::from(d.kind), d.content)
                        .tags(d.tags.into_iter().map(|t| Tag::parse(t.0).unwrap()))
                        .custom_created_at(Timestamp::from(t))
                        .sign_with_keys(&keys)
                        .unwrap()
                };
                let open = sign(&c, now - 30);
                c.status = closed.into();
                let close = sign(&c, now - 10);
                let goal = root.join("goal.md");
                std::fs::write(&goal, "goal").unwrap();
                let iso = |t| Timestamp::from(t).to_human_datetime();
                let publish = |url: &str| {
                    args(&format!(
                        "publish --home {} --relay {url} --id today --title Build --goal-file {} --starts {} --ends {} --budget-sats 0 --status in_progress --update progress",
                        root.display(),
                        goal.display(),
                        iso(now - 60),
                        iso(now + 3600)
                    ))
                };
                // The caller's view: only the open event (in_progress). A publish then succeeds.
                let before = rt.block_on(crate::relay::transport_tests::fixture(
                    vec![open.clone()],
                    true,
                    true,
                ));
                let (mut out, mut err) = (Vec::new(), Vec::new());
                assert_eq!(run(&publish(&before.url), &mut out, &mut err), 0);
                assert_eq!(before.writes.lock().unwrap().len(), 1);
                // The relays now hold the close (newer than the caller's view): refused, no write.
                let after = rt.block_on(crate::relay::transport_tests::fixture(
                    vec![open, close],
                    true,
                    true,
                ));
                let (mut out, mut err) = (Vec::new(), Vec::new());
                assert_eq!(run(&publish(&after.url), &mut out, &mut err), CLOSED_EXIT);
                let err = String::from_utf8(err).unwrap();
                assert!(
                    err.contains(&format!("challenge: {CHALLENGE_CLOSED} ({closed})")),
                    "{err}"
                );
                assert!(out.is_empty());
                assert!(after.writes.lock().unwrap().is_empty(), "close replaced");
                std::fs::remove_dir_all(root).unwrap();
            }
        }
        #[tokio::test]
        async fn leave_with_oversized_prior_note_publishes_bounded_note() {
            let root = std::env::temp_dir().join(format!(
                "challenge-leave-note-{}",
                Keys::generate().public_key()
            ));
            let home = home::bootstrap(&root).unwrap();
            let keys = crate::relay::keys(&home).unwrap();
            let buyer = Keys::generate().public_key().to_hex();
            let a = Membership {
                seller: keys.public_key().to_hex(),
                since: 100,
                minutes: 60,
                status: "joined".into(),
                note: "x".repeat(4097),
            };
            let d = a.to_event_draft(&buyer).unwrap();
            let joined = EventBuilder::new(Kind::from(d.kind), d.content)
                .tags(d.tags.into_iter().map(|t| Tag::parse(t.0).unwrap()))
                .custom_created_at(Timestamp::from(100))
                .sign_with_keys(&keys)
                .unwrap();
            let relay = crate::relay::transport_tests::fixture(vec![joined], true, true).await;
            let mut o = super::parse(&args(&format!("leave --buyer {buyer}"))).unwrap();
            o.relays = vec![relay.url.clone()];
            execute_async(&o, &home).await.unwrap();
            let writes = relay.writes.lock().unwrap();
            assert_eq!(writes.len(), 1);
            assert!(writes[0].content.encode_utf16().count() <= 4096);
            assert_eq!(parse_membership(&writes[0], &buyer).unwrap().status, "left");
            std::fs::remove_dir_all(root).unwrap();
        }
        #[tokio::test]
        async fn members_reads_memberships_only_for_accepted_authors_and_ignores_flood() {
            fn sign(d: maxplayer_core::gateway::EventDraft, k: &Keys, t: u64) -> Event {
                EventBuilder::new(Kind::from(d.kind), d.content)
                    .tags(d.tags.into_iter().map(|t| Tag::parse(t.0).unwrap()))
                    .custom_created_at(Timestamp::from(t))
                    .sign_with_keys(k)
                    .unwrap()
            }
            let root = std::env::temp_dir().join(format!(
                "challenge-roster-{}",
                Keys::generate().public_key()
            ));
            let home = home::bootstrap(&root).unwrap();
            let buyer_keys = crate::relay::keys(&home).unwrap();
            let buyer = buyer_keys.public_key().to_hex();
            let seller = Keys::generate();
            let c = Challenge {
                buyer: buyer.clone(),
                id: "day".into(),
                title: "Day".into(),
                goal: "Goal".into(),
                starts: 1000,
                ends: 2000,
                budget_sats: 0,
                status: "in_progress".into(),
                update: None,
                result: None,
            };
            let member = |k: &Keys, status: &str| Membership {
                since: 100,
                seller: k.public_key().to_hex(),
                minutes: 60,
                status: status.into(),
                note: String::new(),
            };
            let mut events = vec![
                sign(c.to_event_draft().unwrap(), &buyer_keys, 900),
                sign(
                    Decision {
                        seller: seller.public_key().to_hex(),
                        decision: "accepted".into(),
                        membership_event_id: None,
                        reason: String::new(),
                    }
                    .to_event_draft(&buyer)
                    .unwrap(),
                    &buyer_keys,
                    110,
                ),
                sign(
                    member(&seller, "joined").to_event_draft(&buyer).unwrap(),
                    &seller,
                    100,
                ),
            ];
            // 300 stranger joins at the same second, served before the real one.
            for _ in 0..300 {
                let k = Keys::generate();
                events.insert(
                    0,
                    sign(member(&k, "left").to_event_draft(&buyer).unwrap(), &k, 100),
                );
            }
            let relay = crate::relay::transport_tests::fixture(events, true, true).await;
            let mut o = super::parse(&args("members --json")).unwrap();
            o.relays = vec![relay.url.clone()];
            let (body, _) = execute_async(&o, &home).await.unwrap();
            let rows = body["members"].as_array().unwrap();
            let eligible: Vec<_> = rows
                .iter()
                .filter(|r| r["eligible"] == true)
                .map(|r| r["membership"]["seller"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(eligible, vec![seller.public_key().to_hex()]);
            assert_eq!(body["applicants_incomplete"], true);
            // Every trusted (paged, limit 500) membership REQ is scoped to the accepted author.
            let reqs = relay.reqs.lock().unwrap();
            let trusted: Vec<_> = reqs
                .iter()
                .filter(|f| {
                    f["kinds"][0] == CHALLENGE_MEMBERSHIP_KIND
                        && f["limit"] == crate::relay::PAGE_LIMIT
                })
                .collect();
            assert_eq!(trusted.len(), 1);
            assert_eq!(
                trusted[0]["authors"],
                serde_json::json!([seller.public_key().to_hex()])
            );
            std::fs::remove_dir_all(root).unwrap();
        }
        fn signed(d: maxplayer_core::gateway::EventDraft, k: &Keys, t: u64) -> Event {
            EventBuilder::new(Kind::from(d.kind), d.content)
                .tags(d.tags.into_iter().map(|t| Tag::parse(t.0).unwrap()))
                .custom_created_at(Timestamp::from(t))
                .sign_with_keys(k)
                .unwrap()
        }
        fn membership(k: &Keys, buyer: &str, status: &str, since: u64, t: u64) -> Event {
            let a = Membership {
                seller: k.public_key().to_hex(),
                since,
                minutes: 60,
                status: status.into(),
                note: String::new(),
            };
            signed(a.to_event_draft(buyer).unwrap(), k, t)
        }
        fn decision(buyer: &Keys, seller: &Keys, verdict: &str, t: u64) -> Event {
            let d = Decision {
                seller: seller.public_key().to_hex(),
                decision: verdict.into(),
                membership_event_id: None,
                reason: String::new(),
            };
            signed(
                d.to_event_draft(&buyer.public_key().to_hex()).unwrap(),
                buyer,
                t,
            )
        }
        fn day(buyer: &Keys, t: u64) -> Event {
            let c = Challenge {
                buyer: buyer.public_key().to_hex(),
                id: "day".into(),
                title: "Day".into(),
                goal: "Goal".into(),
                starts: 5000,
                ends: 6000,
                budget_sats: 0,
                status: "scheduled".into(),
                update: None,
                result: None,
            };
            signed(c.to_event_draft().unwrap(), buyer, t)
        }
        /// Kind-30342 events from the seller that mention the buyer but are not memberships
        /// (distinct `d`, so a real relay keeps every one); they match the authors filter.
        fn junk(seller: &Keys, buyer: &str, i: usize, t: u64) -> Event {
            EventBuilder::new(Kind::from(CHALLENGE_MEMBERSHIP_KIND), "")
                .tags([
                    Tag::identifier(format!("junk-{i}")),
                    Tag::public_key(PublicKey::from_hex(buyer).unwrap()),
                ])
                .custom_created_at(Timestamp::from(t))
                .sign_with_keys(seller)
                .unwrap()
        }
        #[tokio::test]
        async fn r5_full_junk_page_hiding_older_left_fails_and_publishes_nothing() {
            let root = std::env::temp_dir().join(format!(
                "challenge-r5-junk-{}",
                Keys::generate().public_key()
            ));
            let home = home::bootstrap(&root).unwrap();
            let seller = crate::relay::keys(&home).unwrap();
            let buyer_keys = Keys::generate();
            let buyer = buyer_keys.public_key().to_hex();
            // 500 newer junk events at one second fill the authors page; the real `left`
            // (and the original `joined`) are older and never fit in it.
            let mut events: Vec<_> = (0..500).map(|i| junk(&seller, &buyer, i, 3000)).collect();
            events.extend([
                day(&buyer_keys, 90),
                decision(&buyer_keys, &seller, "accepted", 110),
                membership(&seller, &buyer, "joined", 100, 100),
                membership(&seller, &buyer, "left", 100, 200),
            ]);
            let relay = crate::relay::transport_tests::relay_like(events, vec![]).await;
            for command in [
                format!("join --buyer {buyer} --minutes 60"),
                format!("leave --buyer {buyer}"),
                format!("status --buyer {buyer}"),
            ] {
                let mut o = super::parse(&args(&command)).unwrap();
                o.relays = vec![relay.url.clone()];
                let result = execute_async(&o, &home).await;
                assert!(
                    relay.writes.lock().unwrap().is_empty(),
                    "{command} published (join with since=now) after a truncated read"
                );
                let error = result.expect_err("truncated accepted-author history must fail");
                assert!(
                    error.contains("500+ events in one second"),
                    "{command}: {error}"
                );
            }
            // `members` as the buyer, with the same truncated accepted-author history.
            let buyer_root = root.join("buyer");
            let buyer_home = home::bootstrap(&buyer_root).unwrap();
            let buyer_keys = crate::relay::keys(&buyer_home).unwrap();
            let buyer = buyer_keys.public_key().to_hex();
            let mut events: Vec<_> = (0..500).map(|i| junk(&seller, &buyer, i, 3000)).collect();
            events.extend([
                day(&buyer_keys, 90),
                decision(&buyer_keys, &seller, "accepted", 110),
                membership(&seller, &buyer, "left", 100, 200),
            ]);
            let relay = crate::relay::transport_tests::relay_like(events, vec![]).await;
            let mut o = super::parse(&args("members")).unwrap();
            o.relays = vec![relay.url.clone()];
            let result = execute_async(&o, &buyer_home).await;
            assert!(relay.writes.lock().unwrap().is_empty());
            let error = result.expect_err("members must not list a truncated roster");
            assert!(
                error.contains("500+ events in one second"),
                "members: {error}"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
        #[tokio::test]
        async fn r5_paging_past_full_page_reads_older_membership_and_keeps_since() {
            let root = std::env::temp_dir().join(format!(
                "challenge-r5-page-{}",
                Keys::generate().public_key()
            ));
            let home = home::bootstrap(&root).unwrap();
            let seller = crate::relay::keys(&home).unwrap();
            let buyer_keys = Keys::generate();
            let buyer = buyer_keys.public_key().to_hex();
            // 600 newer junk events, one per second, the oldest at 1001: the first page is full
            // and its cursor moves strictly older, so the second page reaches the real join.
            let mut events: Vec<_> = (0..600)
                .map(|i| junk(&seller, &buyer, i, 1001 + i as u64))
                .collect();
            events.extend([
                day(&buyer_keys, 90),
                decision(&buyer_keys, &seller, "accepted", 110),
                membership(&seller, &buyer, "joined", 100, 100),
            ]);
            let relay = crate::relay::transport_tests::relay_like(events, vec![]).await;
            let mut o = super::parse(&args(&format!("status --buyer {buyer}"))).unwrap();
            o.relays = vec![relay.url.clone()];
            let (body, _) = execute_async(&o, &home).await.unwrap();
            assert_eq!(body["members"][0]["membership"]["status"], "joined");
            assert_eq!(body["members"][0]["membership"]["since"], 100);
            assert_eq!(body["members"][0]["eligible"], true);
            let paged = relay
                .reqs
                .lock()
                .unwrap()
                .iter()
                .filter(|f| f.get("authors").is_some() && f["until"] == 1101)
                .count();
            assert_eq!(
                paged, 1,
                "second page starts at the first page's oldest second"
            );
            let mut o =
                super::parse(&args(&format!("join --buyer {buyer} --minutes 120"))).unwrap();
            o.relays = vec![relay.url.clone()];
            execute_async(&o, &home).await.unwrap();
            let writes = relay.writes.lock().unwrap();
            assert_eq!(writes.len(), 1);
            let a = parse_membership(&writes[0], &buyer).unwrap();
            assert_eq!(
                (a.since, a.minutes),
                (100, 120),
                "join keeps the original since"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
        /// Like `junk`, but already expired (NIP-40): nostr-sdk drops it before notifying, so
        /// only a raw frame count sees that the relay spent its page limit on it.
        fn expired_junk(seller: &Keys, buyer: &str, i: usize, t: u64) -> Event {
            EventBuilder::new(Kind::from(CHALLENGE_MEMBERSHIP_KIND), "")
                .tags([
                    Tag::identifier(format!("expired-{i}")),
                    Tag::public_key(PublicKey::from_hex(buyer).unwrap()),
                    Tag::expiration(Timestamp::from(t + 1)),
                ])
                .custom_created_at(Timestamp::from(t))
                .sign_with_keys(seller)
                .unwrap()
        }
        /// Every membership command against `relay`, as the seller (join/leave/status) and as the
        /// buyer (members). Returns each command's result; never panics on an `Ok`.
        async fn run_all(
            seller_home: &home::MaxplayerHome,
            buyer_home: &home::MaxplayerHome,
            buyer: &str,
            url: &str,
        ) -> Vec<(String, Result<serde_json::Value, String>)> {
            let mut out = Vec::new();
            for (command, h) in [
                (format!("join --buyer {buyer} --minutes 60"), seller_home),
                (format!("leave --buyer {buyer}"), seller_home),
                (format!("status --buyer {buyer}"), seller_home),
                ("members".to_owned(), buyer_home),
            ] {
                let mut o = super::parse(&args(&command)).unwrap();
                o.relays = vec![url.to_owned()];
                out.push((command, execute_async(&o, h).await.map(|(body, _)| body)));
            }
            out
        }
        #[tokio::test]
        async fn r6_expired_junk_page_never_reads_as_complete() {
            let root = std::env::temp_dir().join(format!(
                "challenge-r6-expired-{}",
                Keys::generate().public_key()
            ));
            let seller_home = home::bootstrap(root.join("seller")).unwrap();
            let buyer_home = home::bootstrap(root.join("buyer")).unwrap();
            let seller = crate::relay::keys(&seller_home).unwrap();
            let buyer_keys = crate::relay::keys(&buyer_home).unwrap();
            let buyer = buyer_keys.public_key().to_hex();
            let real = || {
                vec![
                    day(&buyer_keys, 90),
                    decision(&buyer_keys, &seller, "accepted", 110),
                    membership(&seller, &buyer, "joined", 100, 100),
                ]
            };
            // (a) 500 expired frames in one second newer than the real `joined`: the SDK
            // delivers none of them, yet the page is full, so every command fails closed.
            let mut events: Vec<_> = (0..500)
                .map(|i| expired_junk(&seller, &buyer, i, 3000))
                .collect();
            events.extend(real());
            let relay = crate::relay::transport_tests::relay_like(events, vec![]).await;
            for (command, result) in run_all(&seller_home, &buyer_home, &buyer, &relay.url).await {
                let writes = relay.writes.lock().unwrap().len();
                assert_eq!(writes, 0, "{command} published after an unread full page");
                match result {
                    Ok(body) => panic!("{command}: expired full page read as complete: {body}"),
                    Err(error) => {
                        assert!(error.contains("history truncated"), "{command}: {error}")
                    }
                }
            }
            // (b) The same 500 expired frames on distinct newer seconds: the raw cursor pages
            // past them to the real `joined`, so every command reads the real membership.
            let mut events: Vec<_> = (0..500)
                .map(|i| expired_junk(&seller, &buyer, i, 1001 + i as u64))
                .collect();
            events.extend(real());
            let relay = crate::relay::transport_tests::relay_like(events, vec![]).await;
            let results = run_all(&seller_home, &buyer_home, &buyer, &relay.url).await;
            for (command, result) in &results {
                if let Err(error) = result {
                    panic!("{command}: {error}");
                }
            }
            let writes = relay.writes.lock().unwrap();
            assert_eq!(writes.len(), 2, "join and leave publish once each");
            let joined = parse_membership(&writes[0], &buyer).unwrap();
            assert_eq!(
                (joined.status.as_str(), joined.since),
                ("joined", 100),
                "join republished since = now instead of the real since"
            );
            let left = parse_membership(&writes[1], &buyer).unwrap();
            assert_eq!((left.status.as_str(), left.since), ("left", 100));
            // The fixture never serves back what join/leave wrote, so status and members
            // read the real (older) `joined` behind the expired page.
            for (command, result) in &results[2..] {
                let body = result.as_ref().unwrap();
                let row = &body["members"][0];
                assert_eq!(row["membership"]["status"], "joined", "{command}: {body}");
                assert_eq!(row["membership"]["since"], 100, "{command}");
                assert_eq!(row["eligible"], true, "{command}: {body}");
            }
            let paged = relay
                .reqs
                .lock()
                .unwrap()
                .iter()
                .filter(|f| f.get("authors").is_some() && f["until"] == 1001)
                .count();
            assert!(paged >= 1, "second page starts at the oldest raw frame");
            std::fs::remove_dir_all(root).unwrap();
        }
        #[tokio::test]
        async fn r6_newer_same_d_event_does_not_hide_older_valid_membership() {
            let root = std::env::temp_dir().join(format!(
                "challenge-r6-replaced-{}",
                Keys::generate().public_key()
            ));
            let home = home::bootstrap(&root).unwrap();
            let seller = crate::relay::keys(&home).unwrap();
            let buyer_keys = Keys::generate();
            let buyer = buyer_keys.public_key().to_hex();
            // Same (kind, author, d) as the real `joined`, newer, but not a valid membership
            // (no since/minutes/status). A store that applied replacement would reject the
            // older valid event as `Replaced` and never deliver it.
            let invalid = EventBuilder::new(Kind::from(CHALLENGE_MEMBERSHIP_KIND), "")
                .tags([
                    Tag::identifier(buyer.clone()),
                    Tag::public_key(buyer_keys.public_key()),
                ])
                .custom_created_at(Timestamp::from(300))
                .sign_with_keys(&seller)
                .unwrap();
            let events = vec![
                invalid,
                day(&buyer_keys, 90),
                decision(&buyer_keys, &seller, "accepted", 110),
                membership(&seller, &buyer, "joined", 100, 100),
            ];
            let relay = crate::relay::transport_tests::relay_like(events, vec![]).await;
            let mut o = super::parse(&args(&format!("status --buyer {buyer}"))).unwrap();
            o.relays = vec![relay.url.clone()];
            let (body, _) = execute_async(&o, &home).await.unwrap();
            assert_eq!(
                body["members"][0]["membership"]["status"], "joined",
                "{body}"
            );
            assert_eq!(body["members"][0]["membership"]["since"], 100);
            let mut o = super::parse(&args(&format!("join --buyer {buyer} --minutes 90"))).unwrap();
            o.relays = vec![relay.url.clone()];
            execute_async(&o, &home).await.unwrap();
            let writes = relay.writes.lock().unwrap();
            assert_eq!(writes.len(), 1);
            let a = parse_membership(&writes[0], &buyer).unwrap();
            assert_eq!(
                (a.since, a.minutes),
                (100, 90),
                "join lost the original since"
            );
            assert_eq!(
                writes[0].created_at.as_secs(),
                Timestamp::now().as_secs().max(301)
            );
            std::fs::remove_dir_all(root).unwrap();
        }
        #[tokio::test]
        async fn r6_trusted_paging_stops_at_page_cap_and_publishes_nothing() {
            use crate::relay::{MAX_TRUSTED_PAGES, PAGE_LIMIT};
            let root = std::env::temp_dir().join(format!(
                "challenge-r6-cap-{}",
                Keys::generate().public_key()
            ));
            let home = home::bootstrap(&root).unwrap();
            let seller = crate::relay::keys(&home).unwrap();
            let buyer_keys = Keys::generate();
            let buyer = buyer_keys.public_key().to_hex();
            // One junk event per second, more than MAX_TRUSTED_PAGES pages can walk (each page
            // after the first adds PAGE_LIMIT - 1 new seconds); the real `joined` is older.
            let span = PAGE_LIMIT * (MAX_TRUSTED_PAGES + 1);
            let mut events: Vec<_> = (0..span)
                .map(|i| junk(&seller, &buyer, i, 1001 + i as u64))
                .collect();
            events.extend([
                day(&buyer_keys, 90),
                decision(&buyer_keys, &seller, "accepted", 110),
                membership(&seller, &buyer, "joined", 100, 100),
            ]);
            let relay = crate::relay::transport_tests::relay_like(events, vec![]).await;
            for command in [
                format!("join --buyer {buyer} --minutes 60"),
                format!("status --buyer {buyer}"),
            ] {
                relay.reqs.lock().unwrap().clear();
                let mut o = super::parse(&args(&command)).unwrap();
                o.relays = vec![relay.url.clone()];
                let result = execute_async(&o, &home).await;
                assert!(
                    relay.writes.lock().unwrap().is_empty(),
                    "{command} published past the page cap"
                );
                match result {
                    Ok((body, _)) => panic!("{command}: read past the page cap: {body}"),
                    Err(error) => assert!(
                        error.contains("history truncated: trusted page cap reached"),
                        "{command}: {error}"
                    ),
                }
                let membership_reqs = relay
                    .reqs
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|f| f["kinds"][0] == CHALLENGE_MEMBERSHIP_KIND)
                    .count();
                assert_eq!(
                    membership_reqs, MAX_TRUSTED_PAGES,
                    "{command}: REQs past the cap"
                );
            }
            std::fs::remove_dir_all(root).unwrap();
        }
        #[tokio::test]
        async fn r5_members_sample_never_overrides_trusted_row() {
            let root = std::env::temp_dir().join(format!(
                "challenge-r5-sample-{}",
                Keys::generate().public_key()
            ));
            let home = home::bootstrap(&root).unwrap();
            let buyer_keys = crate::relay::keys(&home).unwrap();
            let buyer = buyer_keys.public_key().to_hex();
            let accepted = Keys::generate();
            let denied = Keys::generate();
            let events = vec![
                day(&buyer_keys, 90),
                decision(&buyer_keys, &accepted, "accepted", 110),
                decision(&buyer_keys, &denied, "denied", 110),
                membership(&accepted, &buyer, "joined", 100, 200),
                membership(&denied, &buyer, "joined", 100, 150),
            ];
            // Only the untrusted sample sees a newer `left` for the accepted seller.
            let sample_only = vec![membership(&accepted, &buyer, "left", 100, 300)];
            let relay = crate::relay::transport_tests::relay_like(events, sample_only).await;
            let mut o = super::parse(&args("members --json")).unwrap();
            o.relays = vec![relay.url.clone()];
            let (body, _) = execute_async(&o, &home).await.unwrap();
            let rows = body["members"].as_array().unwrap();
            let row = |k: &Keys| {
                rows.iter()
                    .find(|r| r["membership"]["seller"] == k.public_key().to_hex())
                    .unwrap_or_else(|| panic!("missing row: {body}"))
            };
            assert_eq!(rows.len(), 2);
            let a = row(&accepted);
            assert_eq!(
                a["membership"]["status"], "joined",
                "sample replaced roster row"
            );
            assert_eq!(a["eligible"], true);
            assert_eq!(a["decision"], "accepted");
            // The denied seller is never fetched by the trusted read; it shows from the sample.
            let d = row(&denied);
            assert_eq!(d["decision"], "denied");
            assert_eq!(d["eligible"], false);
            std::fs::remove_dir_all(root).unwrap();
        }
        #[tokio::test]
        async fn oversized_join_note_is_refused_before_publish() {
            let relay = crate::relay::transport_tests::fixture(vec![], true, true).await;
            let root = std::env::temp_dir()
                .join(format!("challenge-note-{}", Keys::generate().public_key()));
            let home = home::bootstrap(&root).unwrap();
            let buyer = crate::relay::keys(&home).unwrap().public_key().to_hex();
            for note in ["x".repeat(4097), "😀".repeat(2049)] {
                let mut o = super::parse(&args(&format!(
                    "join --buyer {buyer} --minutes 60 --note {note}"
                )))
                .unwrap();
                o.relays = vec![relay.url.clone()];
                let result = execute_async(&o, &home).await;
                assert!(
                    relay.writes.lock().unwrap().is_empty(),
                    "oversized note published"
                );
                assert!(result.unwrap_err().contains("4096 UTF-16"));
            }
            std::fs::remove_dir_all(root).unwrap();
        }
        #[tokio::test]
        async fn secret_in_content_or_tags_never_writes_relay() {
            let relay = crate::relay::transport_tests::fixture(vec![], true, true).await;
            let root = std::env::temp_dir().join(format!(
                "challenge-secret-{}",
                Keys::generate().public_key()
            ));
            let home = home::bootstrap(&root).unwrap();
            let secret = home::read_secret_key_hex(&home).unwrap();
            let goal = root.join("goal.md");
            let now = Timestamp::now().as_secs();
            for in_content in [true, false] {
                std::fs::write(
                    &goal,
                    if in_content {
                        secret.as_str()
                    } else {
                        "Safe goal"
                    },
                )
                .unwrap();
                let title = if in_content { "Safe" } else { secret.as_str() };
                let mut o = super::parse(&args(&format!("publish --id test --title {title} --goal-file {} --starts {} --ends {} --budget-sats 0", goal.display(), Timestamp::from(now+3600).to_human_datetime(), Timestamp::from(now+7200).to_human_datetime()))).unwrap();
                o.relays = vec![relay.url.clone()];
                let result = execute_async(&o, &home).await;
                assert!(
                    relay.writes.lock().unwrap().is_empty(),
                    "secret-bearing event reached relay"
                );
                assert!(result.is_err());
            }
            std::fs::remove_dir_all(root).unwrap();
        }
        #[tokio::test]
        async fn challenge_cli_local_relay_publish_join_decide_status_leave() {
            use nostr_relay_builder::prelude::*;
            let relay =
                LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42::default()));
            relay.run().await.unwrap();
            let root = std::env::temp_dir().join(format!(
                "challenge-cli-{}",
                Keys::generate().public_key().to_hex()
            ));
            let mut buyer = home::bootstrap(root.join("buyer")).unwrap();
            let mut seller = home::bootstrap(root.join("seller")).unwrap();
            let relay_url = relay.url().await.to_string();
            for h in [&mut buyer, &mut seller] {
                home::save_config(h, |c| c.relay_url = relay_url.clone()).unwrap();
            }
            let buyer_key = crate::relay::keys(&buyer).unwrap().public_key().to_hex();
            let seller_key = crate::relay::keys(&seller).unwrap().public_key().to_hex();
            let parse = |a: &[String]| {
                let mut o = super::parse(a)?;
                o.relays = vec![relay_url.clone()];
                Ok::<_, String>(o)
            };
            let now = Timestamp::now().as_secs();
            let goal = root.join("goal.md");
            std::fs::write(&goal, "Build a thing").unwrap();
            let iso = |t| Timestamp::from(t).to_human_datetime();
            // Membership exists before any challenge; decisions are equally persistent.
            let before = parse(&args(&format!("join --buyer {buyer_key} --minutes 60"))).unwrap();
            execute_async(&before, &seller).await.unwrap();
            let status = parse(&args(&format!("status --buyer {buyer_key} --json"))).unwrap();
            let (body, _) = execute_async(&status, &seller).await.unwrap();
            assert!(body["challenge"].is_null());
            let since = body["members"][0]["membership"]["since"].clone();
            let publish=parse(&args(&format!("publish --id today --title Build --goal-file {} --starts {} --ends {} --budget-sats 0",goal.display(),iso(now+3590),iso(now+7200)))).unwrap();
            let (published, _) = execute_async(&publish, &buyer).await.unwrap();
            assert_eq!(published["challenge"]["starts"], now + 3590);
            let join = parse(&args(&format!("join --buyer {buyer_key} --minutes 60"))).unwrap();
            let (_, warning) = execute_async(&join, &seller).await.unwrap();
            assert!(warning.is_some());
            let decide = parse(&args(&format!("decide --seller {seller_key} --accept"))).unwrap();
            execute_async(&decide, &buyer).await.unwrap();
            let status = parse(&args(&format!("status --buyer {buyer_key} --json"))).unwrap();
            let (body, _) = execute_async(&status, &seller).await.unwrap();
            assert_eq!(body["members"][0]["eligible"], true);
            assert_eq!(body["members"][0]["membership"]["since"], since);
            let change = parse(&args(&format!("join --buyer {buyer_key} --minutes 120"))).unwrap();
            execute_async(&change, &seller).await.unwrap();
            let (body, _) = execute_async(&status, &seller).await.unwrap();
            assert_eq!(body["members"][0]["membership"]["since"], since);
            assert_eq!(body["members"][0]["membership"]["minutes"], 120);
            let tomorrow=parse(&args(&format!("publish --id tomorrow --title Build --goal-file {} --starts {} --ends {} --budget-sats 0",goal.display(),iso(now+86400),iso(now+90000)))).unwrap();
            execute_async(&tomorrow, &buyer).await.unwrap();
            let status = parse(&args(&format!(
                "status --buyer {buyer_key} --challenge tomorrow --json"
            )))
            .unwrap();
            let (body, _) = execute_async(&status, &seller).await.unwrap();
            assert_eq!(body["challenge"]["id"], "tomorrow");
            assert_eq!(body["members"][0]["eligible"], true);
            let list = parse(&args("members --json")).unwrap();
            let (body, _) = execute_async(&list, &buyer).await.unwrap();
            assert_eq!(body["members"].as_array().unwrap().len(), 1);
            let leave = parse(&args(&format!("leave --buyer {buyer_key}"))).unwrap();
            execute_async(&leave, &seller).await.unwrap();
            let (body, _) = execute_async(&status, &seller).await.unwrap();
            assert_eq!(body["members"][0]["membership"]["status"], "left");
            assert_eq!(body["members"][0]["eligible"], false);
            execute_async(&join, &seller).await.unwrap();
            let (body, _) = execute_async(&status, &seller).await.unwrap();
            assert!(
                body["members"][0]["membership"]["since"].as_u64().unwrap()
                    > since.as_u64().unwrap()
            );
            assert_eq!(body["members"][0]["eligible"], true);
            let secret = home::read_secret_key_hex(&seller).unwrap();
            assert!(!body.to_string().contains(&secret));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
