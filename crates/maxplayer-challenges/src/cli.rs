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
                2
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
        let created = replacement_timestamp(now, &previous, &own, CHALLENGE_KIND, &c.id)?;
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
    let mut events = relay
        .fetch(
            home,
            Filter::new()
                .kind(Kind::from(CHALLENGE_MEMBERSHIP_KIND))
                .pubkey(public_key(&buyer)?),
        )
        .await
        .map_err(|e| e.to_string())?;
    events.extend(
        relay
            .fetch(
                home,
                Filter::new()
                    .kind(Kind::from(CHALLENGE_DECISION_KIND))
                    .author(public_key(&buyer)?),
            )
            .await
            .map_err(|e| e.to_string())?,
    );
    let latest = latest_per_coordinate(
        events
            .iter()
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
            let eligible = c.as_ref().map(|c| eligible_sellers(c, &events)).unwrap_or_default();
            let rows: Vec<_> = memberships.iter()
                .filter(|(_, a)| o.command == "members" || a.seller == own)
                .map(|(e, a)| {
                    let decision = decisions.iter().find(|d| d.seller == a.seller).map(|d| d.decision.as_str());
                    json!({"event_id": e.id.to_hex(), "membership": a, "decision": decision,
                        "eligible": eligible.iter().any(|s| s.seller == a.seller)})
                }).collect();
            return Ok((json!({"challenge": c, "members": rows}), None));
        }
    }.map_err(|e| e.to_string())?;
    let id = relay
        .publish(home, draft, created)
        .await
        .map_err(|e| e.to_string())?;
    Ok((json!({"ok":true,"event_id":id,"challenge":c}), warning))
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
