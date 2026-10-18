//! Community challenge commands. Untrusted relay text and keys never appear in diagnostics.
use std::collections::BTreeMap;
use std::io::Write;

const HELP: &str = "Usage: maxplayer-challenges <command> [--home <path>] [--json] [--relay URL ...] [--quorum N]
  join --buyer <npub|hex> --minutes <1..480> [--note <text>]
  status --buyer <npub|hex> [--challenge <d>]
  leave --buyer <npub|hex>
  publish --id <d> --title <text> --goal-file <path> --starts <rfc3339> --ends <rfc3339> --budget-sats <n> [--status <scheduled|in_progress|completed|cancelled>] [--update <text>] [--result <text>]
  members [--challenge <d>]
  decide --seller <npub|hex> --accept|--deny
Join or leave at any time. Joining after work starts means eligibility from the next day.
While a running challenge counts your accepted membership, join refuses to change minutes or
note (republishing would forfeit that day); change them 15 minutes after it ends (clock drift).
Never leave and rejoin to change minutes: that forfeits the day too. Same minutes with --note
omitted or equal is a no-op; on a change, omitting --note keeps the note, --note '' clears it.
Membership and acceptance persist across days. Minutes are offered per day.
Status/members default to the buyer's latest non-cancelled challenge. Uses the initialized home's key.
Home: $MAXPLAYER_HOME or ~/.maxplayer. Relays default to wss://nos.lol, wss://relay.primal.net, wss://offchain.pub.
Repeat --relay URL after the command to override. ws:// is loopback-only; relay.maxplayer.ai is forbidden.
Reads and publishes need a quorum of relays: --quorum N (default 2, at least a majority of the relays
and at most all of them). Quorum met with a relay missing is degraded: a warning on stderr and in
--json (relay_quorum), and the event is re-sent unchanged to that relay once it answers again.
A minutes/note change during a running challenge still needs every relay.
Per-relay read/publish outcomes are printed to stderr; --json uses stdout.
";
/// A publish refused because the challenge is already closed on the relays exits with
/// `CLOSED_EXIT` and an error starting with `CHALLENGE_CLOSED` (nothing is signed or sent).
const CHALLENGE_CLOSED: &str = "challenge closed";
const CLOSED_EXIT: i32 = 3;
/// Authors per roster membership REQ, and the newest-first applicant sample size (matches the page).
const ROSTER_AUTHOR_CHUNK: usize = 100;
const APPLICANT_LIMIT: usize = 256;
/// The protocol's `created_at` drift tolerance (`docs/challenges-v1.md`): relays and readers
/// accept an event dated up to this far ahead of their own clock.
const CREATED_AT_TOLERANCE: u64 = 900;
#[cfg(not(test))]
fn now_secs() -> u64 {
    nostr_sdk::Timestamp::now().as_secs()
}
#[cfg(test)]
thread_local! {
    /// Test-only wall clock override for boundary tests (the current thread's runtime).
    static TEST_CLOCK: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn now_secs() -> u64 {
    TEST_CLOCK
        .with(std::cell::Cell::get)
        .unwrap_or_else(|| nostr_sdk::Timestamp::now().as_secs())
}
#[derive(Debug)]
struct Opts {
    command: String,
    values: BTreeMap<String, String>,
    json: bool,
    relays: Vec<String>,
    quorum: Option<usize>,
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
        quorum: None,
    };
    let mut i = 1;
    while i < args.len() {
        let k = args[i].as_str();
        if !allowed.contains(&k) && !["--home", "--json", "--relay", "--quorum"].contains(&k) {
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
        if k == "--quorum" {
            let q = v.parse::<usize>().map_err(|_| "invalid --quorum")?;
            if o.quorum.replace(q).is_some() {
                return Err("duplicate --quorum".into());
            }
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
                // Degraded quorum (met, with a relay missing) and re-sends: always on stderr, and
                // in --json as `relay_quorum`.
                let quorum = &body["relay_quorum"];
                for w in quorum["warnings"].as_array().into_iter().flatten() {
                    let _ = writeln!(err, "Warning: {}", w.as_str().unwrap_or_default());
                }
                for r in quorum["resent"].as_array().into_iter().flatten() {
                    let _ = writeln!(
                        err,
                        "Re-sent missed event {}",
                        r.as_str().unwrap_or_default()
                    );
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
    if body.get("unchanged") == Some(&serde_json::Value::Bool(true)) {
        return format!(
            "Membership unchanged (joined, {} minutes per day); nothing published.",
            body["membership"]["minutes"]
        );
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
/// Runs the command, then re-sends queued events to relays that answered in this run (also after a
/// refusal: the queued events already met quorum when they were published), and reports the
/// relay quorum (degraded warnings, missing relays, re-sends) in the JSON body.
async fn execute_async(
    o: &Opts,
    home: &maxplayer_core::home::MaxplayerHome,
) -> Result<(serde_json::Value, Option<String>), String> {
    if o.command == "join" && o.get("--note").unwrap_or("").encode_utf16().count() > 4096 {
        return Err("join note exceeds 4096 UTF-16 code units (reader limit)".into());
    }
    let relay = crate::relay::Relays::with_quorum(&o.relays, o.quorum)?;
    let result = command_async(o, home, &relay).await;
    relay.resend(home).await;
    let (body, warning) = result?;
    let report = relay.take_report();
    let (required, total) = relay.quorum();
    let mut body = body;
    if let Some(map) = body.as_object_mut() {
        map.insert(
            "relay_quorum".into(),
            serde_json::json!({
                "required": required,
                "total": total,
                "degraded": !report.warnings.is_empty(),
                "warnings": report.warnings,
                "missing": report.missing,
                "resent": report.resent,
            }),
        );
    }
    Ok((body, warning))
}
async fn command_async(
    o: &Opts,
    home: &maxplayer_core::home::MaxplayerHome,
    relay: &crate::relay::Relays,
) -> Result<(serde_json::Value, Option<String>), String> {
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
    let now = now_secs();
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
        let filter = Filter::new()
            .author(public_key(&c.buyer)?)
            .kind(Kind::from(CHALLENGE_KIND))
            .identifier(&c.id);
        let previous = relay
            .fetch(home, filter.clone())
            .await
            .map_err(|e| e.to_string())?;
        // A quorum read can complete while omitting a close that only the missing relay holds
        // (published at a bare quorum whose other relay is the one this read shares and lost it).
        // A completed/cancelled status is final either way, but a scheduled/in_progress update over
        // an existing challenge would resurrect it: that needs every relay to answer.
        // A close the quorum read already sees is final: refuse it (exit 3) before any strict re-read.
        if let Some(closed) = closed_status(&previous, &c.buyer, &c.id) {
            return Err(format!(
                "{CHALLENGE_CLOSED} ({closed}): the latest buyer-signed challenge {} is {closed}; nothing published",
                c.id
            ));
        }
        let previous = if !relay.missing().is_empty()
            && !previous.is_empty()
            && !matches!(c.status.as_str(), "completed" | "cancelled")
        {
            relay.fetch_all(home, filter).await.map_err(|e| {
                format!(
                    "challenge relays degraded ({e}); a {} update over an existing challenge needs every relay, so a close held only by the missing relay is never overwritten. Nothing was published; retry when every relay answers.",
                    c.status
                )
            })?
        } else {
            previous
        };
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
    let now = now_secs();
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
    // The caller's current `joined` membership, if any: a join then republishes (an update).
    let joined = memberships
        .iter()
        .find(|(_, a)| a.seller == own && a.status == "joined");
    if o.command == "join" {
        if !home
            .config
            .seller
            .as_ref()
            .is_some_and(|s| s.accept_offers_only_from.contains(&buyer))
        {
            warning = Some(format!(
                "buyer {buyer} is not in seller.accept_offers_only_from; append it without removing existing entries and restart the seller."
            ));
        }
        if let Some((event, current)) = joined {
            let minutes: u64 = o
                .required("--minutes")?
                .parse()
                .map_err(|_| "invalid minutes")?;
            if current.minutes == minutes && o.get("--note").is_none_or(|n| n == current.note) {
                return Ok((
                    json!({"ok": true, "unchanged": true, "membership": current, "challenge": null}),
                    warning,
                ));
            }
            // Memberships are replaceable: the republished event's `created_at` would be
            // `created`, and eligibility needs it at or before the start. Refuse while a running
            // challenge still counts the current membership; fail closed if that is unreadable.
            // Eligibility also needs the latest buyer decision `accepted` (trusted read above):
            // a pending or denied seller is not counted today, so its edit forfeits nothing.
            let mut accepted = decisions
                .iter()
                .any(|d| d.seller == own && d.decision == "accepted");
            // A degraded (quorum, not every-relay) read can omit the buyer's acceptance when only
            // the missing relay holds it; then `accepted` above is wrong and the strict challenge
            // read below would be skipped. So while degraded, re-read the decisions from every
            // relay too, and let that strict read alone decide. Any failure refuses (nothing
            // published).
            if !relay.missing().is_empty() {
                let strict = relay
                    .fetch_all(
                        home,
                        Filter::new()
                            .kind(Kind::from(CHALLENGE_DECISION_KIND))
                            .author(public_key(&buyer)?),
                    )
                    .await
                    .map_err(|e| format!("could not read the buyer's challenge state reliably ({e}); refusing to change your membership, because republishing it during a running challenge would make you ineligible for that day. Nothing was published; retry when every relay answers."))?;
                accepted = latest_per_coordinate(
                    strict.iter().filter(|e| parse_decision(e, &buyer).is_ok()),
                )
                .into_iter()
                .filter_map(|e| parse_decision(e, &buyer).ok())
                .any(|d| d.seller == own && d.decision == "accepted");
            }
            if accepted {
                // Strict every-relay read, not the quorum read: a challenge published to a bare
                // quorum is on only some relays, and the relay left out could be the only one
                // holding the running challenge (then this would publish a forfeiting replacement).
                let challenges = relay
                    .fetch_all(
                        home,
                        Filter::new()
                            .author(public_key(&buyer)?)
                            .kind(Kind::from(CHALLENGE_KIND)),
                    )
                    .await
                    .map_err(|e| format!("could not read the buyer's challenge state reliably ({e}); refusing to change your membership, because republishing it during a running challenge would make you ineligible for that day. Nothing was published; retry when every relay answers."))?;
                if let Some(c) =
                    blocking_challenge(&challenges, &buyer, event, current, now, created)
                {
                    return Err(format!(
                        "You're in today's challenge ({}, ends {}). Change your minutes (or note) after {} (15 minutes after it ends, allowing for clock drift); they apply from the next day. Re-publishing now would make you ineligible for today. Do not leave and rejoin to change minutes: that also forfeits today. Nothing was published.",
                        c.id,
                        Timestamp::from(c.ends).to_human_datetime(),
                        Timestamp::from(c.ends.saturating_add(CREATED_AT_TOLERANCE))
                            .to_human_datetime()
                    ));
                }
            }
        }
    }
    let draft = match o.command.as_str() {
        "join" => {
            Membership {
                since: joined.map(|(_, a)| a.since).unwrap_or(created),
                seller: own.clone(),
                minutes: o
                    .required("--minutes")?
                    .parse()
                    .map_err(|_| "invalid minutes")?,
                status: "joined".into(),
                // Omitting --note keeps the current note; `--note ''` clears it.
                note: o
                    .get("--note")
                    .map(str::to_owned)
                    .or_else(|| joined.map(|(_, a)| a.note.clone()))
                    .unwrap_or_default(),
            }
            .to_event_draft(&buyer)
        }
        "leave" => {
            let mut a = memberships
                .iter()
                .find(|(_, a)| a.seller == own)
                .map(|(_, a)| a.clone())
                .ok_or("no verified membership to leave")?;
            a.status = "left".into();
            if a.note.encode_utf16().count() > 4096 {
                a.note.clear();
            }
            a.to_event_draft(&buyer)
        }
        "decide" => {
            let seller = public_key(o.required("--seller")?)?.to_hex();
            Decision {
                membership_event_id: memberships
                    .iter()
                    .find(|(_, a)| a.seller == seller)
                    .map(|(e, _)| e.id.to_hex()),
                seller,
                decision: if o.get("--accept").is_some() {
                    "accepted"
                } else {
                    "denied"
                }
                .into(),
                reason: String::new(),
            }
            .to_event_draft(&buyer)
        }
        _ => {
            // Trusted events only (decisions + roster-scoped memberships), never the sample.
            let eligible = c
                .as_ref()
                .map(|c| eligible_sellers(c, &events))
                .unwrap_or_default();
            let rows: Vec<_> = memberships
                .iter()
                .filter(|(_, a)| o.command == "members" || a.seller == own)
                .map(|(e, a)| {
                    let decision = decisions
                        .iter()
                        .find(|d| d.seller == a.seller)
                        .map(|d| d.decision.as_str());
                    json!({"event_id": e.id.to_hex(), "membership": a, "decision": decision,
                        "eligible": eligible.iter().any(|s| s.seller == a.seller)})
                })
                .collect();
            let mut body = json!({"challenge": c, "members": rows});
            if o.command == "members" {
                body["applicants_incomplete"] = json!(applicants_incomplete);
            }
            return Ok((body, None));
        }
    }
    .map_err(|e| e.to_string())?;
    reject_secret_in_draft(home, &draft)?;
    let id = relay
        .publish(home, draft, created)
        .await
        .map_err(|e| e.to_string())?;
    Ok((json!({"ok":true,"event_id":id,"challenge":c}), warning))
}

/// The running challenge (latest-ending, if several) that the caller's current membership still
/// counts for, and that a republish at `created` would forfeit. Only buyer-signed challenges
/// (`parse_challenge` verifies author and signature), latest per coordinate, `scheduled` or
/// `in_progress`, with the membership's `created_at` and `since` at or before `starts` (the
/// eligibility rule). Time window: the republish forfeits only if it is dated after `starts`
/// (`created > starts`; `created >= now`, so this covers the whole open interval after the
/// start, and a bumped timestamp before it), and the buyer may post through `ends` inclusive.
/// A seller clock up to `CREATED_AT_TOLERANCE` fast still gets its event accepted, so the
/// window stays closed until `now > ends + CREATED_AT_TOLERANCE`; a slow clock dates the event
/// earlier, which never forfeits.
fn blocking_challenge(
    challenges: &[nostr_sdk::Event],
    buyer: &str,
    membership: &nostr_sdk::Event,
    current: &maxplayer_core::challenge::Membership,
    now: u64,
    created: u64,
) -> Option<maxplayer_core::challenge::Challenge> {
    use maxplayer_core::challenge::*;
    latest_per_coordinate(
        challenges
            .iter()
            .filter(|e| parse_challenge(e, buyer).is_ok()),
    )
    .into_iter()
    .filter_map(|e| parse_challenge(e, buyer).ok())
    .filter(|c| matches!(c.status.as_str(), "scheduled" | "in_progress"))
    .filter(|c| created > c.starts && now <= c.ends.saturating_add(CREATED_AT_TOLERANCE))
    .filter(|c| membership.created_at.as_secs() <= c.starts && current.since <= c.starts)
    .max_by(|a, b| a.ends.cmp(&b.ends).then(b.id.cmp(&a.id)))
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
    if created > now.saturating_add(CREATED_AT_TOLERANCE) {
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
                (format!("join --buyer {buyer} --minutes 90"), seller_home),
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
            // 90 differs from the stored 60 minutes: an identical join is a no-op (no publish).
            assert_eq!(
                (joined.status.as_str(), joined.since, joined.minutes),
                ("joined", 100, 90),
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
            let before = Timestamp::now().as_secs();
            execute_async(&o, &home).await.unwrap();
            let after = Timestamp::now().as_secs();
            let writes = relay.writes.lock().unwrap();
            assert_eq!(writes.len(), 1);
            let a = parse_membership(&writes[0], &buyer).unwrap();
            assert_eq!(
                (a.since, a.minutes),
                (100, 90),
                "join lost the original since"
            );
            // `created` is taken mid-command (before the challenge-state read), so bound it.
            let created = writes[0].created_at.as_secs();
            assert!(
                (before.max(301)..=after.max(301)).contains(&created),
                "{before}..={after}: {created}"
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

        // BEGIN midday-minutes tests
        /// Run 2 (issue #14): a minutes change republishes the replaceable membership, moving
        /// its `created_at` past the start, which the eligibility rule (rightly) rejects.
        mod midday_minutes {
            use super::*;
            use crate::relay::transport_tests::{relay_like, relay_like_closing};
            pub(super) struct Setup {
                pub(super) root: std::path::PathBuf,
                pub(super) home: home::MaxplayerHome,
                pub(super) seller: Keys,
                pub(super) buyer_keys: Keys,
                pub(super) buyer: String,
                pub(super) now: u64,
            }
            impl Drop for Setup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.root);
                }
            }
            fn setup(name: &str) -> Setup {
                let root = std::env::temp_dir().join(format!(
                    "challenge-midday-{name}-{}",
                    Keys::generate().public_key()
                ));
                let home = home::bootstrap(&root).unwrap();
                let seller = crate::relay::keys(&home).unwrap();
                let buyer_keys = Keys::generate();
                let buyer = buyer_keys.public_key().to_hex();
                Setup {
                    root,
                    home,
                    seller,
                    buyer_keys,
                    buyer,
                    now: Timestamp::now().as_secs(),
                }
            }
            fn challenge(
                s: &Setup,
                id: &str,
                starts: u64,
                ends: u64,
                status: &str,
                t: u64,
            ) -> Event {
                let c = Challenge {
                    buyer: s.buyer.clone(),
                    id: id.into(),
                    title: id.into(),
                    goal: "Goal".into(),
                    starts,
                    ends,
                    budget_sats: 0,
                    status: status.into(),
                    update: None,
                    result: None,
                };
                signed(c.to_event_draft().unwrap(), &s.buyer_keys, t)
            }
            /// The seller's `joined` membership, signed at `t` with `since = t`.
            fn joined(s: &Setup, minutes: u64, note: &str, t: u64) -> Event {
                joined_since(s, minutes, note, t, t)
            }
            /// The seller's `joined` membership with `since` and `created_at` set separately.
            fn joined_since(s: &Setup, minutes: u64, note: &str, since: u64, t: u64) -> Event {
                let a = Membership {
                    seller: s.seller.public_key().to_hex(),
                    since,
                    minutes,
                    status: "joined".into(),
                    note: note.into(),
                };
                signed(a.to_event_draft(&s.buyer).unwrap(), &s.seller, t)
            }
            /// Running `day` + accepted decision + a 60-minute membership from well before start.
            fn running(s: &Setup) -> Vec<Event> {
                vec![
                    challenge(
                        s,
                        "day",
                        s.now - 100,
                        s.now + 3600,
                        "in_progress",
                        s.now - 30_000,
                    ),
                    decision(&s.buyer_keys, &s.seller, "accepted", s.now - 19_000),
                    joined(s, 60, "hello", s.now - 20_000),
                ]
            }
            async fn command(
                s: &Setup,
                url: &str,
                line: &str,
            ) -> Result<serde_json::Value, String> {
                let mut o = super::super::parse(&args(line)).unwrap();
                o.relays = vec![url.to_owned()];
                execute_async(&o, &s.home).await.map(|(body, _)| body)
            }
            fn writes(relay: &crate::relay::transport_tests::Fixture) -> Vec<Event> {
                relay.writes.lock().unwrap().clone()
            }
            /// `join` with an exact argv (so `--note ''` can be an empty argument).
            async fn join_argv(
                s: &Setup,
                url: &str,
                extra: &[&str],
            ) -> Result<serde_json::Value, String> {
                let mut argv = args(&format!("join --buyer {}", s.buyer));
                argv.extend(extra.iter().map(|a| (*a).to_owned()));
                let mut o = super::super::parse(&argv).unwrap();
                o.relays = vec![url.to_owned()];
                execute_async(&o, &s.home).await.map(|(body, _)| body)
            }
            /// Pins the sidecar's wall clock (`now_secs`) on this thread until dropped.
            struct Clock;
            impl Clock {
                fn at(t: u64) -> Self {
                    crate::cli::TEST_CLOCK.with(|c| c.set(Some(t)));
                    Clock
                }
            }
            impl Drop for Clock {
                fn drop(&mut self) {
                    crate::cli::TEST_CLOCK.with(|c| c.set(None));
                }
            }
            /// How many REQs asked for the buyer's challenges (kind 30340).
            fn challenge_reqs(relay: &crate::relay::transport_tests::Fixture) -> usize {
                relay
                    .reqs
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|f| {
                        f["kinds"]
                            .as_array()
                            .is_some_and(|k| k.iter().any(|v| v == CHALLENGE_KIND))
                    })
                    .count()
            }
            #[test]
            fn midday_minutes_change_is_refused_exit_nonzero_and_publishes_nothing() {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .unwrap();
                let s = setup("refuse");
                let relay = rt.block_on(relay_like(running(&s), vec![]));
                let argv = args(&format!(
                    "join --buyer {} --minutes 120 --home {} --relay {}",
                    s.buyer,
                    s.root.display(),
                    relay.url
                ));
                let (mut out, mut err) = (Vec::new(), Vec::new());
                let code = run(&argv, &mut out, &mut err);
                let err = String::from_utf8(err).unwrap();
                assert_eq!(code, 2, "{err}");
                assert!(
                    writes(&relay).is_empty(),
                    "minutes change published mid-challenge"
                );
                let ends = Timestamp::from(s.now + 3600).to_human_datetime();
                assert!(
                    err.contains(&format!("You're in today's challenge (day, ends {ends})")),
                    "{err}"
                );
                assert!(err.contains("apply from the next day"), "{err}");
                assert!(err.contains("ineligible for today"), "{err}");
                assert!(err.contains("Nothing was published"), "{err}");
                assert!(out.is_empty());
                // A note-only change republishes too, so it is refused the same way.
                let line = format!("join --buyer {} --minutes 60 --note changed", s.buyer);
                let error = rt.block_on(command(&s, &relay.url, &line)).unwrap_err();
                assert!(
                    error.contains("You're in today's challenge (day,"),
                    "{error}"
                );
                assert!(
                    writes(&relay).is_empty(),
                    "note change published mid-challenge"
                );
                // A newer, not-yet-started challenge must not hide the running one.
                let mut events = running(&s);
                events.push(challenge(
                    &s,
                    "tomorrow",
                    s.now + 86_400,
                    s.now + 90_000,
                    "scheduled",
                    s.now - 10,
                ));
                let relay = rt.block_on(relay_like(events, vec![]));
                let line = format!("join --buyer {} --minutes 120", s.buyer);
                let error = rt.block_on(command(&s, &relay.url, &line)).unwrap_err();
                assert!(
                    error.contains("You're in today's challenge (day,"),
                    "{error}"
                );
                assert!(writes(&relay).is_empty());
            }
            #[tokio::test]
            async fn midday_same_minutes_join_is_a_noop_and_publishes_nothing() {
                let s = setup("noop");
                let relay = relay_like(running(&s), vec![]).await;
                for line in [
                    format!("join --buyer {} --minutes 60", s.buyer),
                    format!("join --buyer {} --minutes 60 --note hello", s.buyer),
                ] {
                    let body = command(&s, &relay.url, &line)
                        .await
                        .unwrap_or_else(|e| panic!("{line}: {e}"));
                    assert_eq!(body["unchanged"], true, "{line}: {body}");
                    assert_eq!(body["membership"]["minutes"], 60);
                    assert!(body.get("event_id").is_none(), "{line}: {body}");
                    assert!(
                        writes(&relay).is_empty(),
                        "{line} republished an unchanged membership"
                    );
                }
                // Also a no-op with no challenge at all: nothing to change, nothing to publish.
                let relay = relay_like(vec![joined(&s, 60, "", s.now - 20_000)], vec![]).await;
                let line = format!("join --buyer {} --minutes 60", s.buyer);
                assert_eq!(
                    command(&s, &relay.url, &line).await.unwrap()["unchanged"],
                    true
                );
                assert!(writes(&relay).is_empty());
            }
            #[tokio::test]
            async fn midday_minutes_change_wall_clock_before_start_after_end_or_not_counted_is_published()
             {
                let s = setup("allowed");
                let n = s.now;
                let member = joined(&s, 60, "hello", n - 20_000);
                let cases: [(&str, Vec<Event>); 4] = [
                    (
                        "before start",
                        vec![challenge(
                            &s,
                            "day",
                            n + 3600,
                            n + 7200,
                            "scheduled",
                            n - 30_000,
                        )],
                    ),
                    (
                        "after end",
                        vec![challenge(
                            &s,
                            "day",
                            n - 7200,
                            n - 3600,
                            "in_progress",
                            n - 30_000,
                        )],
                    ),
                    (
                        "completed early",
                        vec![challenge(
                            &s,
                            "day",
                            n - 100,
                            n + 3600,
                            "completed",
                            n - 30_000,
                        )],
                    ),
                    (
                        "cancelled (latest version)",
                        vec![
                            challenge(&s, "day", n - 100, n + 3600, "in_progress", n - 30_000),
                            challenge(&s, "day", n - 100, n + 3600, "cancelled", n - 50),
                        ],
                    ),
                ];
                // Already ineligible today, so there is nothing to lose: each conjunct alone.
                let running_day =
                    || challenge(&s, "day", n - 100, n + 3600, "in_progress", n - 30_000);
                let replaced = [
                    (
                        "membership replaced after start (since <= starts < created_at)",
                        joined_since(&s, 60, "hello", n - 20_000, n - 50),
                        n - 20_000,
                    ),
                    (
                        "membership period began after start (created_at <= starts < since)",
                        joined_since(&s, 60, "hello", n - 50, n - 20_000),
                        n - 50,
                    ),
                ];
                let all = cases
                    .into_iter()
                    .map(|(case, events)| (case, events, member.clone(), n - 20_000))
                    .chain(
                        replaced
                            .into_iter()
                            .map(|(case, m, since)| (case, vec![running_day()], m, since)),
                    );
                for (case, mut events, member, since) in all {
                    events.push(decision(&s.buyer_keys, &s.seller, "accepted", n - 19_000));
                    events.push(member);
                    let relay = relay_like(events, vec![]).await;
                    let line = format!("join --buyer {} --minutes 120", s.buyer);
                    let body = command(&s, &relay.url, &line)
                        .await
                        .unwrap_or_else(|e| panic!("{case}: {e}"));
                    assert!(body["event_id"].is_string(), "{case}: {body}");
                    let w = writes(&relay);
                    assert_eq!(w.len(), 1, "{case}");
                    let a = parse_membership(&w[0], &s.buyer).unwrap();
                    assert_eq!(
                        (a.status.as_str(), a.minutes, a.since),
                        ("joined", 120, since),
                        "{case}"
                    );
                    assert_eq!(a.note, "hello", "{case}: omitting --note keeps the note");
                }
            }
            #[tokio::test]
            async fn midday_leave_during_running_challenge_still_publishes() {
                let s = setup("leave");
                for closing in [None, Some(CHALLENGE_KIND)] {
                    let relay = relay_like_closing(running(&s), vec![], closing).await;
                    let line = format!("leave --buyer {}", s.buyer);
                    command(&s, &relay.url, &line)
                        .await
                        .unwrap_or_else(|e| panic!("{closing:?}: {e}"));
                    let w = writes(&relay);
                    assert_eq!(w.len(), 1, "{closing:?}");
                    let a = parse_membership(&w[0], &s.buyer).unwrap();
                    assert_eq!((a.status.as_str(), a.since), ("left", s.now - 20_000));
                }
            }
            #[tokio::test]
            async fn midday_first_join_publishes_and_is_ineligible_until_next_day() {
                let s = setup("first");
                let day = challenge(
                    &s,
                    "day",
                    s.now - 100,
                    s.now + 3600,
                    "in_progress",
                    s.now - 30_000,
                );
                let accepted = decision(&s.buyer_keys, &s.seller, "accepted", s.now - 19_000);
                for closing in [None, Some(CHALLENGE_KIND)] {
                    let relay =
                        relay_like_closing(vec![day.clone(), accepted.clone()], vec![], closing)
                            .await;
                    let line = format!("join --buyer {} --minutes 120", s.buyer);
                    let body = command(&s, &relay.url, &line)
                        .await
                        .unwrap_or_else(|e| panic!("{closing:?}: {e}"));
                    assert!(body["event_id"].is_string(), "{closing:?}: {body}");
                    let w = writes(&relay);
                    assert_eq!(w.len(), 1, "{closing:?}");
                    let a = parse_membership(&w[0], &s.buyer).unwrap();
                    assert_eq!((a.status.as_str(), a.minutes), ("joined", 120));
                    assert!(a.since > s.now - 100 && w[0].created_at.as_secs() > s.now - 100);
                    // Unchanged rule: not eligible today, eligible for the next day's challenge.
                    let today = parse_challenge(&day, &s.buyer).unwrap();
                    assert!(eligible_sellers(&today, [&w[0], &accepted]).is_empty());
                    let mut next = today.clone();
                    next.starts += 86_400;
                    next.ends += 86_400;
                    assert_eq!(eligible_sellers(&next, [&w[0], &accepted]).len(), 1);
                    if closing.is_none() {
                        let readback =
                            relay_like(vec![day.clone(), accepted.clone(), w[0].clone()], vec![])
                                .await;
                        let status = format!("status --buyer {}", s.buyer);
                        let body = command(&s, &readback.url, &status).await.unwrap();
                        assert_eq!(body["members"][0]["eligible"], false, "{body}");
                    }
                }
            }
            #[tokio::test]
            async fn midday_unreadable_challenge_state_refuses_minutes_change() {
                let s = setup("unreadable");
                let relay = relay_like_closing(running(&s), vec![], Some(CHALLENGE_KIND)).await;
                let line = format!("join --buyer {} --minutes 120", s.buyer);
                let error = command(&s, &relay.url, &line).await.unwrap_err();
                assert!(
                    error.contains("could not read the buyer's challenge state reliably"),
                    "{error}"
                );
                assert!(error.contains("Nothing was published"), "{error}");
                assert!(
                    writes(&relay).is_empty(),
                    "published without a trusted challenge read"
                );
                // The same-minutes no-op needs no challenge read.
                let line = format!("join --buyer {} --minutes 60", s.buyer);
                assert_eq!(
                    command(&s, &relay.url, &line).await.unwrap()["unchanged"],
                    true
                );
                assert!(writes(&relay).is_empty());
            }
            /// Fixed-clock day: starts `S`, ends `E`, accepted seller joined well before `S`.
            fn pinned(s: &Setup, minutes_note: &str) -> (u64, u64, Vec<Event>) {
                let (starts, ends) = (s.now - 50_000, s.now - 40_000);
                let events = vec![
                    challenge(s, "day", starts, ends, "in_progress", starts - 10_000),
                    decision(&s.buyer_keys, &s.seller, "accepted", starts - 9_000),
                    joined(s, 60, minutes_note, starts - 8_000),
                ];
                (starts, ends, events)
            }
            #[tokio::test]
            async fn midday_end_bound_refuses_through_ends_plus_drift_tolerance() {
                let s = setup("end-bound");
                let (_, ends, events) = pinned(&s, "hello");
                // The buyer may post through `ends` inclusive, and a seller clock up to 900 s
                // fast still has its event accepted: refuse through `ends + 900`.
                for now in [ends, ends + CREATED_AT_TOLERANCE] {
                    let relay = relay_like(events.clone(), vec![]).await;
                    let _clock = Clock::at(now);
                    let error = join_argv(&s, &relay.url, &["--minutes", "120"])
                        .await
                        .unwrap_err();
                    assert!(
                        error.contains("You're in today's challenge (day,"),
                        "{now}: {error}"
                    );
                    let after = Timestamp::from(ends + CREATED_AT_TOLERANCE).to_human_datetime();
                    assert!(error.contains(&format!("after {after}")), "{error}");
                    assert!(error.contains("Do not leave and rejoin"), "{error}");
                    assert!(writes(&relay).is_empty(), "now = {now}: published");
                }
                // Just past the bound: the change publishes, dated at the (pinned) clock.
                let relay = relay_like(events.clone(), vec![]).await;
                let _clock = Clock::at(ends + CREATED_AT_TOLERANCE + 1);
                join_argv(&s, &relay.url, &["--minutes", "120"])
                    .await
                    .unwrap();
                let w = writes(&relay);
                assert_eq!(w.len(), 1);
                assert_eq!(w[0].created_at.as_secs(), ends + CREATED_AT_TOLERANCE + 1);
                assert_eq!(parse_membership(&w[0], &s.buyer).unwrap().minutes, 120);
            }
            #[tokio::test]
            async fn midday_start_second_is_allowed_and_stays_eligible() {
                let s = setup("start-second");
                let (starts, _, events) = pinned(&s, "hello");
                let accepted = events[1].clone();
                let day = parse_challenge(&events[0], &s.buyer).unwrap();
                // now == starts == created: the signed event still satisfies created_at <= starts.
                let relay = relay_like(events.clone(), vec![]).await;
                {
                    let _clock = Clock::at(starts);
                    join_argv(&s, &relay.url, &["--minutes", "120"])
                        .await
                        .unwrap();
                }
                let w = writes(&relay);
                assert_eq!(w.len(), 1);
                assert_eq!(w[0].created_at.as_secs(), starts);
                assert_eq!(eligible_sellers(&day, [&w[0], &accepted]).len(), 1);
                // One second later the event would be dated after the start: refused.
                let relay = relay_like(events, vec![]).await;
                let _clock = Clock::at(starts + 1);
                let error = join_argv(&s, &relay.url, &["--minutes", "120"])
                    .await
                    .unwrap_err();
                assert!(
                    error.contains("You're in today's challenge (day,"),
                    "{error}"
                );
                assert!(writes(&relay).is_empty());
            }
            #[tokio::test]
            async fn midday_bumped_created_after_start_is_refused_before_start() {
                let s = setup("bumped");
                let (starts, ends) = (s.now - 50_000, s.now - 40_000);
                // The previous membership is dated exactly at `starts` (still eligible), so the
                // NIP-01 bump signs `starts + 1` even though the wall clock is before the start.
                let events = vec![
                    challenge(&s, "day", starts, ends, "scheduled", starts - 10_000),
                    decision(&s.buyer_keys, &s.seller, "accepted", starts - 9_000),
                    joined(&s, 60, "hello", starts),
                ];
                let relay = relay_like(events, vec![]).await;
                let _clock = Clock::at(starts - 10);
                let error = join_argv(&s, &relay.url, &["--minutes", "120"])
                    .await
                    .unwrap_err();
                assert!(
                    error.contains("You're in today's challenge (day,"),
                    "{error}"
                );
                assert!(writes(&relay).is_empty(), "bumped replacement published");
            }
            #[tokio::test]
            async fn midday_pending_or_denied_seller_can_edit_minutes() {
                let s = setup("not-accepted");
                let n = s.now;
                let day = challenge(&s, "day", n - 100, n + 3600, "in_progress", n - 30_000);
                let member = joined(&s, 60, "hello", n - 20_000);
                let cases = [
                    ("pending (no decision)", vec![]),
                    (
                        "denied",
                        vec![decision(&s.buyer_keys, &s.seller, "denied", n - 19_000)],
                    ),
                    (
                        "accepted, then denied (latest wins)",
                        vec![
                            decision(&s.buyer_keys, &s.seller, "accepted", n - 19_000),
                            decision(&s.buyer_keys, &s.seller, "denied", n - 18_000),
                        ],
                    ),
                ];
                for (case, decisions) in cases {
                    let mut events = vec![day.clone(), member.clone()];
                    events.extend(decisions);
                    // Not counted today, so nothing to forfeit: no challenge read is needed,
                    // and an unreadable challenge kind does not block the edit.
                    let relay = relay_like_closing(events, vec![], Some(CHALLENGE_KIND)).await;
                    let line = format!("join --buyer {} --minutes 120", s.buyer);
                    let body = command(&s, &relay.url, &line)
                        .await
                        .unwrap_or_else(|e| panic!("{case}: {e}"));
                    assert!(body["event_id"].is_string(), "{case}: {body}");
                    let w = writes(&relay);
                    assert_eq!(w.len(), 1, "{case}");
                    let a = parse_membership(&w[0], &s.buyer).unwrap();
                    assert_eq!((a.minutes, a.since), (120, n - 20_000), "{case}");
                    assert_eq!(challenge_reqs(&relay), 0, "{case}");
                }
                // The accepted seller in the same state is still refused.
                let events = vec![
                    day,
                    member,
                    decision(&s.buyer_keys, &s.seller, "accepted", n - 19_000),
                ];
                let relay = relay_like(events, vec![]).await;
                let line = format!("join --buyer {} --minutes 120", s.buyer);
                assert!(command(&s, &relay.url, &line).await.is_err());
                assert!(writes(&relay).is_empty());
            }
            #[tokio::test]
            async fn midday_empty_note_clears_and_counts_as_a_change() {
                let s = setup("empty-note");
                let n = s.now;
                // Running: `--note ''` against note "hello" is a change, so it is refused.
                let relay = relay_like(running(&s), vec![]).await;
                let error = join_argv(&s, &relay.url, &["--minutes", "60", "--note", ""])
                    .await
                    .unwrap_err();
                assert!(
                    error.contains("You're in today's challenge (day,"),
                    "{error}"
                );
                assert!(writes(&relay).is_empty());
                // Before the start: it publishes with the note cleared.
                let events = vec![
                    challenge(&s, "day", n + 3600, n + 7200, "scheduled", n - 30_000),
                    decision(&s.buyer_keys, &s.seller, "accepted", n - 19_000),
                    joined(&s, 60, "hello", n - 20_000),
                ];
                let relay = relay_like(events, vec![]).await;
                join_argv(&s, &relay.url, &["--minutes", "60", "--note", ""])
                    .await
                    .unwrap();
                let w = writes(&relay);
                assert_eq!(w.len(), 1);
                let a = parse_membership(&w[0], &s.buyer).unwrap();
                assert_eq!((a.minutes, a.note.as_str()), (60, ""));
                // Already empty: `--note ''` with the same minutes is a no-op.
                let mut events = running(&s);
                events[2] = joined(&s, 60, "", n - 20_000);
                let relay = relay_like(events, vec![]).await;
                let body = join_argv(&s, &relay.url, &["--minutes", "60", "--note", ""])
                    .await
                    .unwrap();
                assert_eq!(body["unchanged"], true, "{body}");
                assert!(writes(&relay).is_empty());
            }
            #[tokio::test]
            async fn midday_timeout_or_truncated_challenge_read_refuses_minutes_change() {
                use crate::relay::transport_tests::{Refusal, relay_like_refusing};
                let s = setup("timeout-truncated");
                // No EOSE: the reader waits out its timeout, then fails closed.
                let relay = relay_like_refusing(
                    running(&s),
                    vec![],
                    Some((CHALLENGE_KIND, Refusal::NoEose)),
                )
                .await;
                let error = join_argv(&s, &relay.url, &["--minutes", "120"])
                    .await
                    .unwrap_err();
                assert!(
                    error.contains("could not read the buyer's challenge state reliably"),
                    "{error}"
                );
                assert!(error.contains("no EOSE before timeout"), "{error}");
                assert!(
                    writes(&relay).is_empty(),
                    "published after a timed-out read"
                );
                // Truncated history: a full page of buyer kind-30340 events in one second.
                let flood = s.now - 10;
                let mut events = running(&s);
                events.extend((0..crate::relay::PAGE_LIMIT).map(|i| {
                    EventBuilder::new(Kind::from(CHALLENGE_KIND), "")
                        .tags([Tag::identifier(format!("flood-{i}"))])
                        .custom_created_at(Timestamp::from(flood))
                        .sign_with_keys(&s.buyer_keys)
                        .unwrap()
                }));
                let relay = relay_like(events, vec![]).await;
                let error = join_argv(&s, &relay.url, &["--minutes", "120"])
                    .await
                    .unwrap_err();
                assert!(
                    error.contains("could not read the buyer's challenge state reliably"),
                    "{error}"
                );
                assert!(error.contains("history truncated"), "{error}");
                assert!(
                    writes(&relay).is_empty(),
                    "published after a truncated read"
                );
            }
        }
        // END midday-minutes tests

        // BEGIN quorum tests
        /// Run 2 #13: a 2-of-3 challenge relay quorum for reads and publishes, degraded warnings,
        /// and re-sending the exact signed event to a relay that missed it.
        mod quorum {
            use super::*;
            use crate::relay::transport_tests::{Fixture, relay_like};
            use crate::relay::{
                OUTBOX_MAX_AGE_SECS, OUTBOX_MAX_ATTEMPTS, OUTBOX_MAX_ENTRIES, Outbox, Relays,
            };
            use serde_json::json;
            use std::sync::atomic::Ordering;
            struct Setup {
                root: std::path::PathBuf,
                home: home::MaxplayerHome,
                seller: Keys,
                buyer_keys: Keys,
                buyer: String,
            }
            impl Drop for Setup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.root);
                }
            }
            fn setup(name: &str) -> Setup {
                let root = std::env::temp_dir().join(format!(
                    "challenge-quorum-{name}-{}",
                    Keys::generate().public_key()
                ));
                let home = home::bootstrap(&root).unwrap();
                let seller = crate::relay::keys(&home).unwrap();
                let buyer_keys = Keys::generate();
                let buyer = buyer_keys.public_key().to_hex();
                Setup {
                    root,
                    home,
                    seller,
                    buyer_keys,
                    buyer,
                }
            }
            /// Accepted decision + a joined membership from the seller (this home).
            fn roster(s: &Setup) -> Vec<Event> {
                vec![
                    decision(&s.buyer_keys, &s.seller, "accepted", 100),
                    membership(&s.seller, &s.buyer, "joined", 100, 100),
                ]
            }
            fn challenge_event(
                buyer: &Keys,
                id: &str,
                starts: u64,
                ends: u64,
                status: &str,
                t: u64,
            ) -> Event {
                let c = Challenge {
                    buyer: buyer.public_key().to_hex(),
                    id: id.into(),
                    title: id.into(),
                    goal: "Goal".into(),
                    starts,
                    ends,
                    budget_sats: 0,
                    status: status.into(),
                    update: None,
                    result: None,
                };
                signed(c.to_event_draft().unwrap(), buyer, t)
            }
            async fn three(events: Vec<Event>) -> Vec<Fixture> {
                let mut out = Vec::new();
                for _ in 0..3 {
                    out.push(relay_like(events.clone(), vec![]).await);
                }
                out
            }
            /// The relay URL as the sidecar reports it (normalized).
            fn norm(url: &str) -> String {
                url::Url::parse(url).unwrap().to_string()
            }
            fn writes(r: &Fixture) -> Vec<Event> {
                r.writes.lock().unwrap().clone()
            }
            async fn command(
                home: &home::MaxplayerHome,
                r: &[Fixture],
                line: &str,
            ) -> Result<serde_json::Value, String> {
                let mut o = crate::cli::parse(&args(line)).unwrap();
                o.relays = r.iter().map(|f| f.url.clone()).collect();
                execute_async(&o, home).await.map(|(body, _)| body)
            }
            fn relay_args(r: &[Fixture]) -> String {
                r.iter()
                    .map(|f| format!("--relay {}", f.url))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            fn rt() -> tokio::runtime::Runtime {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .unwrap()
            }

            #[tokio::test]
            async fn quorum_met_with_one_relay_down_reads_publishes_and_reports_degraded() {
                let s = setup("met");
                let r = three(roster(&s)).await;
                r[2].online.store(false, Ordering::SeqCst);
                let status = format!("status --buyer {} --json", s.buyer);
                let body = command(&s.home, &r, &status).await.unwrap();
                assert_eq!(body["members"][0]["membership"]["status"], "joined");
                let q = &body["relay_quorum"];
                assert_eq!(
                    (
                        q["required"].clone(),
                        q["total"].clone(),
                        q["degraded"].clone()
                    ),
                    (json!(2), json!(3), json!(true))
                );
                assert_eq!(q["missing"], json!([norm(&r[2].url)]));
                let w = q["warnings"][0].as_str().unwrap();
                assert!(
                    w.starts_with(&format!(
                        "degraded read: 2 of 3 challenge relays (quorum 2); missing: {} (",
                        norm(&r[2].url)
                    )),
                    "{w}"
                );
                // A publish accepted by 2 of 3 succeeds, is degraded, and is queued for the third.
                let body = command(&s.home, &r, &format!("leave --buyer {} --json", s.buyer))
                    .await
                    .unwrap();
                assert_eq!(
                    (
                        writes(&r[0]).len(),
                        writes(&r[1]).len(),
                        writes(&r[2]).len()
                    ),
                    (1, 1, 0)
                );
                assert_eq!(writes(&r[0])[0], writes(&r[1])[0]);
                assert!(
                    body["relay_quorum"]["warnings"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|w| w.as_str().unwrap().starts_with(&format!(
                            "degraded publish: 2 of 3 challenge relays (quorum 2); missing: {}",
                            norm(&r[2].url)
                        )))
                );
                let outbox = Outbox::load(&s.home);
                assert_eq!(outbox.entries.len(), 1);
                assert_eq!(outbox.entries[0].event, writes(&r[0])[0]);
                assert_eq!(
                    outbox.entries[0]
                        .missing
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>(),
                    vec![norm(&r[2].url)]
                );
                // Every relay up: not degraded.
                r[2].online.store(true, Ordering::SeqCst);
                let r2 = three(roster(&s)).await;
                let body = command(&s.home, &r2, &status).await.unwrap();
                assert_eq!(body["relay_quorum"]["degraded"], false);
                assert_eq!(body["relay_quorum"]["warnings"], json!([]));
            }

            /// The degraded warning reaches stderr (and stdout stays the JSON body).
            #[test]
            fn degraded_warning_and_resend_are_printed_on_stderr() {
                let rt = rt();
                let s = setup("stderr");
                let r = rt.block_on(three(roster(&s)));
                r[2].online.store(false, Ordering::SeqCst);
                let line = |cmd: &str| {
                    args(&format!(
                        "{cmd} --buyer {} --home {} {} --json",
                        s.buyer,
                        s.root.display(),
                        relay_args(&r)
                    ))
                };
                let (mut out, mut err) = (Vec::new(), Vec::new());
                assert_eq!(run(&line("leave"), &mut out, &mut err), 0);
                let err = String::from_utf8(err).unwrap();
                assert!(
                    err.contains(&format!(
                        "Warning: degraded read: 2 of 3 challenge relays (quorum 2); missing: {}",
                        norm(&r[2].url)
                    )),
                    "{err}"
                );
                assert!(err.contains(&format!("Warning: degraded publish: 2 of 3 challenge relays (quorum 2); missing: {}", norm(&r[2].url))), "{err}");
                let body: serde_json::Value = serde_json::from_slice(&out).unwrap();
                assert_eq!(body["relay_quorum"]["degraded"], true);
                r[2].online.store(true, Ordering::SeqCst);
                let (mut out, mut err) = (Vec::new(), Vec::new());
                assert_eq!(run(&line("status"), &mut out, &mut err), 0);
                let err = String::from_utf8(err).unwrap();
                assert!(
                    err.contains(&format!(
                        "Re-sent missed event {} -> {}",
                        writes(&r[0])[0].id.to_hex(),
                        norm(&r[2].url)
                    )),
                    "{err}"
                );
                assert!(!err.contains("Warning: degraded"), "{err}");
            }

            #[tokio::test]
            async fn below_quorum_read_and_publish_fail_closed_and_queue_nothing() {
                let s = setup("below");
                let r = three(roster(&s)).await;
                for f in &r[1..] {
                    f.online.store(false, Ordering::SeqCst);
                }
                let error = command(&s.home, &r, &format!("status --buyer {} --json", s.buyer))
                    .await
                    .unwrap_err();
                assert!(error.starts_with("challenge relay read incomplete; quorum not met (1 of 3 relays complete, need 2); failed: "), "{error}");
                let error = command(
                    &s.home,
                    &r,
                    &format!("join --buyer {} --minutes 90", s.buyer),
                )
                .await
                .unwrap_err();
                assert!(error.contains("quorum not met (1 of 3"), "{error}");
                let error = command(&s.home, &r, &format!("leave --buyer {}", s.buyer))
                    .await
                    .unwrap_err();
                assert!(error.contains("quorum not met (1 of 3"), "{error}");
                assert!(
                    r.iter().all(|f| writes(f).is_empty()),
                    "published below the read quorum"
                );
                // Every relay readable, two refuse the write: 1 of 3 acknowledgements.
                for f in &r[1..] {
                    f.online.store(true, Ordering::SeqCst);
                    f.accept.store(false, Ordering::SeqCst);
                }
                let error = command(&s.home, &r, &format!("leave --buyer {}", s.buyer))
                    .await
                    .unwrap_err();
                assert!(error.starts_with("challenge publish incomplete; quorum not met (1 of 3 relays accepted, need 2); failed: "), "{error}");
                assert!(
                    error.contains(&norm(&r[1].url))
                        && error.contains(&norm(&r[2].url))
                        && !error.contains(&norm(&r[0].url)),
                    "{error}"
                );
                assert!(
                    !Outbox::path(&s.home).exists(),
                    "a failed publish is never queued for re-send"
                );
                // An explicit --quorum 3 needs every relay.
                let mut o =
                    crate::cli::parse(&args(&format!("status --buyer {} --quorum 3", s.buyer)))
                        .unwrap();
                o.relays = r.iter().map(|f| f.url.clone()).collect();
                r[2].online.store(false, Ordering::SeqCst);
                let error = execute_async(&o, &s.home).await.unwrap_err();
                assert!(
                    error.starts_with("challenge relay read incomplete; failed: "),
                    "{error}"
                );
            }

            #[test]
            fn quorum_setting_defaults_to_two_and_must_be_a_majority() {
                let u = |n: u16| {
                    (0..n)
                        .map(|i| format!("ws://127.0.0.1:{}", 1000 + i))
                        .collect::<Vec<_>>()
                };
                assert_eq!(Relays::new(&[]).unwrap().quorum(), (2, 3), "default relays");
                for (n, q) in [(1, 1), (2, 2), (3, 2), (4, 3), (5, 3)] {
                    assert_eq!(
                        Relays::new(&u(n)).unwrap().quorum(),
                        (q, n as usize),
                        "{n} relays"
                    );
                }
                for q in [0, 1, 4] {
                    assert!(Relays::with_quorum(&u(3), Some(q)).is_err(), "quorum {q}");
                }
                for q in [2, 3] {
                    assert_eq!(
                        Relays::with_quorum(&u(3), Some(q)).unwrap().quorum(),
                        (q, 3)
                    );
                }
                assert_eq!(
                    crate::cli::parse(&args("status --buyer abc --quorum 3"))
                        .unwrap()
                        .quorum,
                    Some(3)
                );
                for bad in [
                    "status --buyer abc --quorum x",
                    "status --buyer abc --quorum 2 --quorum 3",
                    "status --buyer abc --quorum",
                ] {
                    assert!(crate::cli::parse(&args(bad)).is_err(), "{bad}");
                }
            }

            #[tokio::test]
            async fn resend_delivers_the_exact_signed_event_once_when_the_relay_returns() {
                let s = setup("resend");
                let r = three(roster(&s)).await;
                r[2].online.store(false, Ordering::SeqCst);
                command(&s.home, &r, &format!("leave --buyer {}", s.buyer))
                    .await
                    .unwrap();
                let sent = writes(&r[0])[0].clone();
                let c = norm(&r[2].url);
                let status = format!("status --buyer {} --json", s.buyer);
                // Still down: never tried (only relays that answered this run are), attempts unchanged.
                let body = command(&s.home, &r, &status).await.unwrap();
                assert_eq!(body["relay_quorum"]["resent"], json!([]));
                assert_eq!(Outbox::load(&s.home).entries[0].missing.get(&c), Some(&0));
                // Back, but refusing writes: one failed attempt is counted.
                r[2].online.store(true, Ordering::SeqCst);
                r[2].accept.store(false, Ordering::SeqCst);
                let body = command(&s.home, &r, &status).await.unwrap();
                assert_eq!(body["relay_quorum"]["resent"], json!([]));
                assert_eq!(writes(&r[2]).len(), 1);
                assert_eq!(Outbox::load(&s.home).entries[0].missing.get(&c), Some(&1));
                // Accepting: the very same signed event (id and signature), never re-signed.
                r[2].accept.store(true, Ordering::SeqCst);
                let body = command(&s.home, &r, &status).await.unwrap();
                assert_eq!(
                    body["relay_quorum"]["resent"],
                    json!([format!("{} -> {c}", sent.id.to_hex())])
                );
                let w = writes(&r[2]);
                assert_eq!(w.len(), 2);
                assert_eq!(w[1], sent);
                assert_eq!(w[1].sig, sent.sig);
                assert!(Outbox::load(&s.home).entries.is_empty());
                // Idempotent: delivered entries are gone, nothing is sent again.
                let body = command(&s.home, &r, &status).await.unwrap();
                assert_eq!(body["relay_quorum"]["resent"], json!([]));
                assert_eq!(writes(&r[2]).len(), 2);
                assert_eq!((writes(&r[0]).len(), writes(&r[1]).len()), (1, 1));
            }

            #[test]
            fn outbox_is_bounded_superseding_and_own_signed_only() {
                use std::os::unix::fs::PermissionsExt;
                let s = setup("outbox");
                let ev = |k: &Keys, d: &str, t: u64| {
                    EventBuilder::new(Kind::from(CHALLENGE_MEMBERSHIP_KIND), "")
                        .tags([Tag::identifier(d)])
                        .custom_created_at(Timestamp::from(t))
                        .sign_with_keys(k)
                        .unwrap()
                };
                let c = vec!["ws://c/".to_owned()];
                let mut o = Outbox::default();
                for i in 0..70u64 {
                    o.queue(&ev(&s.seller, &format!("d{i}"), 100), &c, 1000 + i);
                }
                assert_eq!(o.entries.len(), OUTBOX_MAX_ENTRIES);
                assert_eq!(o.entries[0].queued_at, 1006, "oldest dropped first");
                // A newer version of the same coordinate supersedes; an older one never replaces it.
                let mut o = Outbox::default();
                o.queue(&ev(&s.seller, "x", 100), &c, 1);
                o.queue(&ev(&s.seller, "x", 101), &c, 2);
                o.queue(&ev(&s.seller, "x", 99), &c, 3);
                assert_eq!(o.entries.len(), 1);
                assert_eq!(o.entries[0].event.created_at.as_secs(), 101);
                o.queue(&ev(&s.seller, "x", 102), &[], 4);
                assert!(
                    o.entries.is_empty(),
                    "delivered everywhere: the older entry is moot"
                );
                // Attempts and age are bounded.
                o.queue(&ev(&s.seller, "y", 100), &c, 10);
                *o.entries[0].missing.get_mut("ws://c/").unwrap() = OUTBOX_MAX_ATTEMPTS;
                o.prune(10);
                assert!(o.entries.is_empty());
                o.queue(&ev(&s.seller, "y", 100), &c, 10);
                o.prune(10 + OUTBOX_MAX_AGE_SECS + 1);
                assert!(o.entries.is_empty());
                // Round trip with a private file; foreign or tampered entries are dropped on load.
                o.queue(&ev(&s.seller, "z", 100), &c, 10);
                o.save(&s.home).unwrap();
                let mode = std::fs::metadata(Outbox::path(&s.home))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600);
                assert_eq!(Outbox::load(&s.home).entries, o.entries);
                let mut tampered = ev(&s.seller, "t", 100);
                tampered.content = "changed".into();
                let rows = json!([
                    {"event": ev(&s.buyer_keys, "f", 100), "missing": {"ws://c/": 0}, "queued_at": 10},
                    {"event": tampered, "missing": {"ws://c/": 0}, "queued_at": 10},
                ]);
                std::fs::write(Outbox::path(&s.home), rows.to_string()).unwrap();
                assert!(Outbox::load(&s.home).entries.is_empty());
            }

            /// The minutes-change challenge read stays an every-relay read: quorum met is not enough.
            #[tokio::test]
            async fn minutes_change_still_needs_every_relay_when_quorum_is_met() {
                let s = setup("minutes");
                let now = Timestamp::now().as_secs();
                let r = three(vec![
                    challenge_event(
                        &s.buyer_keys,
                        "day",
                        now - 100,
                        now + 3600,
                        "in_progress",
                        now - 30_000,
                    ),
                    decision(&s.buyer_keys, &s.seller, "accepted", now - 19_000),
                    membership(&s.seller, &s.buyer, "joined", now - 20_000, now - 20_000),
                ])
                .await;
                r[2].online.store(false, Ordering::SeqCst);
                let error = command(
                    &s.home,
                    &r,
                    &format!("join --buyer {} --minutes 120", s.buyer),
                )
                .await
                .unwrap_err();
                assert!(error.starts_with("could not read the buyer's challenge state reliably (challenge relay read incomplete; failed: "), "{error}");
                assert!(r.iter().all(|f| writes(f).is_empty()));
                // Every relay up: the ordinary refusal ("You're in today's challenge").
                r[2].online.store(true, Ordering::SeqCst);
                let error = command(
                    &s.home,
                    &r,
                    &format!("join --buyer {} --minutes 120", s.buyer),
                )
                .await
                .unwrap_err();
                assert!(error.starts_with("You're in today's challenge"), "{error}");
                assert!(r.iter().all(|f| writes(f).is_empty()));
            }

            /// Advisor r1 H2: the buyer's acceptance is only on the relay that is down. The quorum
            /// decision read then shows "not accepted"; that must not skip the every-relay check.
            #[tokio::test]
            async fn minutes_change_refused_when_acceptance_is_only_on_the_down_relay() {
                let s = setup("h2");
                let now = Timestamp::now().as_secs();
                let open = challenge_event(
                    &s.buyer_keys,
                    "day",
                    now - 100,
                    now + 3600,
                    "in_progress",
                    now - 30_000,
                );
                let joined = membership(&s.seller, &s.buyer, "joined", now - 20_000, now - 20_000);
                let accepted = decision(&s.buyer_keys, &s.seller, "accepted", now - 19_000);
                let r = vec![
                    relay_like(vec![open.clone(), joined.clone()], vec![]).await,
                    relay_like(vec![open.clone(), joined.clone()], vec![]).await,
                    relay_like(vec![open, joined, accepted], vec![]).await,
                ];
                r[2].online.store(false, Ordering::SeqCst);
                for change in ["--minutes 120", "--minutes 60 --note changed"] {
                    let error = command(&s.home, &r, &format!("join --buyer {} {change}", s.buyer))
                        .await
                        .unwrap_err();
                    assert!(error.starts_with("could not read the buyer's challenge state reliably (challenge relay read incomplete; failed: "), "{error}");
                }
                assert!(
                    r.iter().all(|f| writes(f).is_empty()),
                    "published while degraded"
                );
                // Relay back: the strict read sees the acceptance and the running challenge.
                r[2].online.store(true, Ordering::SeqCst);
                let error = command(
                    &s.home,
                    &r,
                    &format!("join --buyer {} --minutes 120", s.buyer),
                )
                .await
                .unwrap_err();
                assert!(error.starts_with("You're in today's challenge"), "{error}");
                assert!(r.iter().all(|f| writes(f).is_empty()));
                // Not degraded and genuinely not accepted (no decision anywhere): publishes, as before.
                let s2 = setup("h2-pending");
                let open = challenge_event(
                    &s2.buyer_keys,
                    "day",
                    now - 100,
                    now + 3600,
                    "in_progress",
                    now - 30_000,
                );
                let r = three(vec![
                    open,
                    membership(&s2.seller, &s2.buyer, "joined", now - 20_000, now - 20_000),
                ])
                .await;
                command(
                    &s2.home,
                    &r,
                    &format!("join --buyer {} --minutes 120", s2.buyer),
                )
                .await
                .unwrap();
                assert!(r.iter().all(|f| writes(f).len() == 1));
            }

            /// Closed-challenge refusal (exit 3) still works at quorum; a non-terminal update over an
            /// existing challenge while degraded needs every relay; completion does not.
            #[test]
            fn closed_refusal_and_degraded_updates_over_an_existing_challenge() {
                let rt = rt();
                let s = setup("publish");
                let buyer = s.seller.clone(); // this home publishes as the buyer here
                let now = Timestamp::now().as_secs();
                let open = challenge_event(
                    &buyer,
                    "today",
                    now - 60,
                    now + 3600,
                    "in_progress",
                    now - 30,
                );
                let close =
                    challenge_event(&buyer, "today", now - 60, now + 3600, "cancelled", now - 10);
                let goal = s.root.join("goal.md");
                std::fs::write(&goal, "Goal").unwrap();
                let iso = |t| Timestamp::from(t).to_human_datetime();
                let publish = |r: &[Fixture], status: &str| {
                    args(&format!(
                        "publish --home {} {} --id today --title today --goal-file {} --starts {} --ends {} --budget-sats 0 --status {status} --result done",
                        s.root.display(),
                        relay_args(r),
                        goal.display(),
                        iso(now - 60),
                        iso(now + 3600)
                    ))
                };
                let run_ = |a: Vec<String>| {
                    let (mut out, mut err) = (Vec::new(), Vec::new());
                    let code = run(&a, &mut out, &mut err);
                    (code, String::from_utf8(err).unwrap())
                };
                // The close is on 2 of 3 relays and the third is down: still refused with exit 3.
                let r = rt.block_on(three(vec![open.clone(), close]));
                r[2].online.store(false, Ordering::SeqCst);
                let (code, err) = run_(publish(&r, "in_progress"));
                assert_eq!(code, CLOSED_EXIT, "{err}");
                assert!(
                    err.contains("challenge: challenge closed (cancelled)"),
                    "{err}"
                );
                assert!(r.iter().all(|f| writes(f).is_empty()));
                // Open challenge, one relay down: an in_progress update could resurrect a close held
                // only by the missing relay, so it needs every relay.
                let r = rt.block_on(three(vec![open.clone()]));
                r[2].online.store(false, Ordering::SeqCst);
                let (code, err) = run_(publish(&r, "in_progress"));
                assert_eq!(code, 2, "{err}");
                assert!(err.contains("needs every relay"), "{err}");
                assert!(r.iter().all(|f| writes(f).is_empty()));
                // Completion is final either way: published at quorum, degraded.
                let (code, err) = run_(publish(&r, "completed"));
                assert_eq!(code, 0, "{err}");
                assert!(err.contains("Warning: degraded publish: 2 of 3"), "{err}");
                assert_eq!(
                    (
                        writes(&r[0]).len(),
                        writes(&r[1]).len(),
                        writes(&r[2]).len()
                    ),
                    (1, 1, 0)
                );
                // This home's own completion is still queued for the relay that missed it, so even
                // relays that never stored it cannot make an in_progress update overwrite it.
                let r = rt.block_on(three(vec![open.clone()]));
                let (code, err) = run_(publish(&r, "in_progress"));
                assert_eq!(code, CLOSED_EXIT, "{err}");
                assert!(r.iter().all(|f| writes(f).is_empty()));
                // Every relay up, nothing queued: the in_progress update publishes as before.
                std::fs::remove_file(Outbox::path(&s.home)).unwrap();
                let r = rt.block_on(three(vec![open]));
                let (code, err) = run_(publish(&r, "in_progress"));
                assert_eq!(code, 0, "{err}");
                assert!(r.iter().all(|f| writes(f).len() == 1));
            }

            /// The quorum's limit, stated as a test: a completed quorum read in which every answering
            /// relay omits an accepted event. The fixtures never serve writes, so after a degraded
            /// `leave` every relay that answers "omits" it. While this home still has the event
            /// queued for the missing relay, the read includes it; without that, the read shows the
            /// older `joined` (only an every-relay read, or an honest overlapping relay, sees it).
            #[tokio::test]
            async fn quorum_read_omitting_an_accepted_publish_still_sees_this_homes_queued_event() {
                let s = setup("omit");
                let r = three(roster(&s)).await;
                r[2].online.store(false, Ordering::SeqCst);
                command(&s.home, &r, &format!("leave --buyer {}", s.buyer))
                    .await
                    .unwrap();
                let status = format!("status --buyer {} --json", s.buyer);
                let body = command(&s.home, &r, &status).await.unwrap();
                assert_eq!(body["members"][0]["membership"]["status"], "left");
                std::fs::remove_file(Outbox::path(&s.home)).unwrap();
                let body = command(&s.home, &r, &status).await.unwrap();
                assert_eq!(
                    body["members"][0]["membership"]["status"], "joined",
                    "the omission this cannot detect"
                );
            }
        }
        // END quorum tests
    }
}
