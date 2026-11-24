//! `maxplayer swarm …`: swarms, swarm jobs, memberships and decisions on the home's relay
//! (docs/specs/swarm.md §6). Replaces the `maxplayer-swarm` sidecar.
//!
//! One relay: the home's `relay_url`. Reads must end in the relay's EOSE; a short read fails the
//! command instead of acting on a partial view. Readable output goes to stdout, hints and notes to
//! stderr; `--json` prints one JSON object on stdout (notes included as `notes`).

use std::io::Write;
use std::path::PathBuf;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
#[cfg(feature = "wallet")]
const RUNTIME_ERROR: i32 = 2;

const USAGE: &str = "\
Usage: maxplayer swarm <command> [--home <dir>] [--json]
  list [--all]                         swarms on this relay from every buyer (--all adds closed ones)
  show <swarm>                         one swarm: purpose, members with minutes, swarm jobs
  join <swarm> --minutes <1..480> [--note <text>]
                                       seller: offer this swarm N minutes a day
  leave <swarm>                        seller: leave this swarm
  create --name <name> --purpose-file <path> [--slug <slug>] [--private-tasks]
                                       buyer: create a swarm
  job create <swarm> --goal-file <path> --budget-sats <n> [--title <text>]
             [--starts <rfc3339>] [--ends <rfc3339>] [--slug <slug>]
                                       buyer: give one of your swarms a goal (default now to +24 h)
  job status <swarm job>               a swarm job, its eligible members and yours
  decide --seller <npub|hex> --accept|--deny [--swarm <swarm>]
                                       buyer: accept or deny a seller in all your swarms, or in one
  members [<swarm>]                    buyer: applicants and members, all your swarms or one

<swarm> and <swarm job> are the naddr1… addresses `list`, `show` and `create` print, or
<buyer npub|hex>/<slug>. Uses this home's key and relay_url ($MAXPLAYER_HOME or ~/.maxplayer).

join adds the swarm's buyer to [seller].accept_offers_only_from (entries are kept) and asks you to
restart the seller. You get tasks once the buyer accepts you (all its swarms, or this one). A
membership published while one of the swarm's jobs is running counts from its next job. While a
running swarm job counts your membership, join refuses to change minutes or note: republishing
would forfeit that job; change them after it ends. Never leave and rejoin to change minutes.
Exit codes: 0 done, 1 usage error, 2 refused or failed (nothing was published unless it says so).
";

const VALUE_FLAGS: &[&str] = &[
    "--home",
    "--minutes",
    "--note",
    "--name",
    "--purpose-file",
    "--slug",
    "--goal-file",
    "--budget-sats",
    "--title",
    "--starts",
    "--ends",
    "--seller",
    "--swarm",
];
const BOOL_FLAGS: &[&str] = &["--json", "--all", "--accept", "--deny", "--private-tasks"];

#[derive(Debug, Default, PartialEq, Eq)]
struct Opts {
    words: Vec<String>,
    values: std::collections::BTreeMap<&'static str, String>,
    flags: std::collections::BTreeSet<&'static str>,
}

impl Opts {
    fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
    fn required(&self, name: &str) -> Result<&str, String> {
        self.get(name).ok_or_else(|| format!("missing {name}"))
    }
    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if let Some(&name) = VALUE_FLAGS.iter().find(|f| **f == a) {
            let v = args
                .get(i + 1)
                .ok_or_else(|| format!("{a} needs a value"))?;
            if o.values.insert(name, v.clone()).is_some() {
                return Err(format!("{a} given twice"));
            }
            i += 2;
            continue;
        }
        if let Some(&name) = BOOL_FLAGS.iter().find(|f| **f == a) {
            if !o.flags.insert(name) {
                return Err(format!("{a} given twice"));
            }
        } else if a.starts_with("--") {
            return Err(format!("unknown option {a}"));
        } else {
            o.words.push(a.to_owned());
        }
        i += 1;
    }
    // Which options each command takes; anything else is a usage error, never silently ignored.
    let words: Vec<&str> = o.words.iter().map(String::as_str).collect();
    let allowed: &[&str] = match words.as_slice() {
        ["list"] => &["--all"],
        ["show", _] | ["leave", _] | ["job", "status", _] | ["members"] | ["members", _] => &[],
        ["join", _] => &["--minutes", "--note"],
        ["create"] => &["--name", "--purpose-file", "--slug", "--private-tasks"],
        ["job", "create", _] => &[
            "--goal-file",
            "--budget-sats",
            "--title",
            "--starts",
            "--ends",
            "--slug",
        ],
        ["decide"] => &["--seller", "--accept", "--deny", "--swarm"],
        _ => return Err("unknown swarm command".into()),
    };
    for name in o.values.keys().chain(o.flags.iter()) {
        if !matches!(*name, "--home" | "--json") && !allowed.contains(name) {
            return Err(format!(
                "{name} does not apply to `swarm {}`",
                o.words.join(" ")
            ));
        }
    }
    if words == ["decide"] && o.flag("--accept") == o.flag("--deny") {
        return Err("decide needs exactly one of --accept or --deny".into());
    }
    Ok(o)
}

/// Entry point for `maxplayer swarm …`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.is_empty() || crate::cli::is_help_request(args) {
        let _ = out.write_all(USAGE.as_bytes());
        return SUCCESS;
    }
    let opts = match parse(args) {
        Ok(opts) => opts,
        Err(message) => {
            let _ = writeln!(err, "swarm: {message}");
            let _ = err.write_all(USAGE.as_bytes());
            return USAGE_ERROR;
        }
    };
    #[cfg(not(feature = "wallet"))]
    {
        let _ = (opts, out);
        let _ = writeln!(
            err,
            "maxplayer swarm requires the wallet feature (rebuild with default features)"
        );
        USAGE_ERROR
    }
    #[cfg(feature = "wallet")]
    match wired::execute(&opts) {
        Ok(report) => {
            for note in &report.notes {
                let _ = writeln!(err, "{note}");
            }
            if opts.flag("--json") {
                let mut body = report.body;
                body["notes"] = serde_json::json!(report.notes);
                let _ = writeln!(out, "{body}");
            } else {
                let _ = out.write_all(report.human.as_bytes());
            }
            SUCCESS
        }
        Err(message) => {
            let _ = writeln!(err, "swarm: {message}");
            RUNTIME_ERROR
        }
    }
}

/// A swarm or swarm job slug derived from free text: `a-z`, `0-9` and single dashes.
fn slugify(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= max {
            break;
        }
    }
    slug.trim_end_matches('-').to_owned()
}

#[allow(dead_code)]
fn home_override(o: &Opts) -> Option<PathBuf> {
    o.get("--home").map(PathBuf::from)
}

#[cfg(feature = "wallet")]
mod wired {
    use super::*;
    use maxplayer_core::home::{self, MaxplayerHome};
    use maxplayer_core::swarm::*;
    use maxplayer_core::swarm_relay::{SwarmRelay, replacement_timestamp};
    use nostr_sdk::prelude::*;
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};

    /// Untrusted applicant sample size for `members`: never feeds eligibility.
    const APPLICANT_LIMIT: usize = 256;
    const DAY: u64 = 86_400;

    pub(super) struct Report {
        pub body: Value,
        pub human: String,
        pub notes: Vec<String>,
    }

    struct Ctx {
        home: MaxplayerHome,
        relay: SwarmRelay,
        me: String,
        now: u64,
        notes: Vec<String>,
    }

    pub(super) fn execute(o: &Opts) -> Result<Report, String> {
        let root = match home_override(o) {
            Some(path) => path,
            None => home::default_home_dir().map_err(|e| e.to_string())?,
        };
        let home = home::bootstrap(&root).map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("swarm runtime: {e}"))?;
        runtime.block_on(async {
            let relay = SwarmRelay::connect(&home).await?;
            let mut ctx = Ctx {
                me: relay.public_key(),
                relay,
                home,
                now: now_secs(),
                notes: Vec::new(),
            };
            let result = command(&mut ctx, o).await;
            let Ctx { relay, notes, .. } = ctx;
            relay.disconnect().await;
            let (body, human) = result?;
            Ok(Report { body, human, notes })
        })
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    async fn command(c: &mut Ctx, o: &Opts) -> Result<(Value, String), String> {
        let words: Vec<&str> = o.words.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["list"] => list(c, o.flag("--all")).await,
            ["show", s] => show(c, s).await,
            ["join", s] => join(c, o, s).await,
            ["leave", s] => leave(c, s).await,
            ["create"] => create(c, o).await,
            ["job", "create", s] => job_create(c, o, s).await,
            ["job", "status", j] => job_status(c, j).await,
            ["decide"] => decide(c, o).await,
            ["members"] => members(c, None).await,
            ["members", s] => members(c, Some(s)).await,
            _ => Err("unknown swarm command".into()),
        }
    }

    // ---- addresses and keys ------------------------------------------------------------------

    fn public_key(s: &str) -> Result<PublicKey, String> {
        if s.starts_with("npub1") {
            PublicKey::from_bech32(s).map_err(|_| ())
        } else {
            PublicKey::from_hex(s).map_err(|_| ())
        }
        .map_err(|_| format!("invalid public key {s}: expected npub or hex"))
    }

    fn npub(hex: &str) -> String {
        PublicKey::from_hex(hex)
            .ok()
            .and_then(|k| k.to_bech32().ok())
            .unwrap_or_else(|| hex.to_owned())
    }

    /// `(buyer hex, d)` from an naddr of `kind`, or `<npub|hex>/<slug>`.
    fn parse_address(s: &str, kind: u16) -> Result<(String, String), String> {
        let what = if kind == SWARM_KIND {
            "swarm"
        } else {
            "swarm job"
        };
        if s.starts_with("naddr1") {
            let n = Nip19Coordinate::from_bech32(s)
                .map_err(|_| format!("invalid {what} address {s}"))?;
            if n.coordinate.kind.as_u16() != kind {
                return Err(format!(
                    "{s} is not a {what} (kind {})",
                    n.coordinate.kind.as_u16()
                ));
            }
            if !valid_slug(&n.coordinate.identifier) {
                return Err(format!("invalid {what} address {s}"));
            }
            return Ok((
                n.coordinate.public_key.to_hex(),
                n.coordinate.identifier.clone(),
            ));
        }
        let (buyer, slug) = s.split_once('/').ok_or_else(|| {
            format!("invalid {what} address {s}: expected naddr1… or <buyer npub|hex>/<slug>")
        })?;
        if !valid_slug(slug) {
            return Err(format!("invalid {what} slug {slug}: 1-64 of a-z, 0-9, -"));
        }
        Ok((public_key(buyer)?.to_hex(), slug.to_owned()))
    }

    fn naddr(c: &Ctx, kind: u16, buyer: &str, d: &str) -> String {
        let Ok(pk) = PublicKey::from_hex(buyer) else {
            return format!("{buyer}/{d}");
        };
        let coordinate = Coordinate::new(Kind::from(kind), pk).identifier(d);
        let relays: Vec<RelayUrl> = RelayUrl::parse(&c.home.config.relay_url)
            .into_iter()
            .collect();
        Nip19Coordinate::new(coordinate, relays)
            .to_bech32()
            .unwrap_or_else(|_| format!("{}/{d}", npub(buyer)))
    }

    fn time(t: u64) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp(t as i64, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| t.to_string())
    }

    fn timestamp(s: &str) -> Result<u64, String> {
        let dt = chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|_| format!("invalid RFC3339 timestamp {s}"))?;
        u64::try_from(dt.timestamp()).map_err(|_| "timestamp must be after the unix epoch".into())
    }

    fn first_line(text: &str) -> &str {
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
    }

    fn tag_a() -> SingleLetterTag {
        SingleLetterTag::lowercase(Alphabet::A)
    }

    // ---- reads -------------------------------------------------------------------------------

    async fn find_swarm(c: &Ctx, buyer: &str, d: &str) -> Result<(Event, Swarm), String> {
        let events = c
            .relay
            .fetch(vec![
                Filter::new()
                    .kind(Kind::from(SWARM_KIND))
                    .author(public_key(buyer)?)
                    .identifier(d),
            ])
            .await?;
        latest_swarms(&events)
            .into_iter()
            .find(|(_, s)| s.buyer == buyer && s.id == d)
            .map(|(e, s)| (e.clone(), s))
            .ok_or_else(|| {
                format!(
                    "no swarm {} on {}",
                    naddr(c, SWARM_KIND, buyer, d),
                    c.relay.url()
                )
            })
    }

    async fn jobs(
        c: &Ctx,
        buyer: &str,
        swarm: Option<&str>,
    ) -> Result<Vec<(Event, SwarmJob)>, String> {
        let mut filter = Filter::new()
            .kind(Kind::from(SWARM_JOB_KIND))
            .author(public_key(buyer)?);
        if let Some(swarm) = swarm {
            filter = filter.custom_tag(tag_a(), swarm_address(buyer, swarm));
        }
        let events = c.relay.fetch(vec![filter]).await?;
        Ok(latest_swarm_jobs(&events)
            .into_iter()
            .filter(|(_, j)| j.buyer == buyer && swarm.is_none_or(|s| s == j.swarm))
            .map(|(e, j)| (e.clone(), j))
            .collect())
    }

    async fn decision_events(c: &Ctx, buyers: &[PublicKey]) -> Result<Vec<Event>, String> {
        c.relay
            .fetch_by_authors(buyers, || {
                Filter::new().kind(Kind::from(SWARM_DECISION_KIND))
            })
            .await
    }

    /// Memberships naming `buyer` (one swarm or all) by `authors`, plus this key's own.
    async fn membership_events(
        c: &Ctx,
        buyer: &str,
        swarm: Option<&str>,
        authors: &BTreeSet<String>,
    ) -> Result<Vec<Event>, String> {
        let filter = || {
            let f = Filter::new()
                .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
                .pubkey(PublicKey::from_hex(buyer).expect("hex"));
            match swarm {
                Some(s) => f.custom_tag(tag_a(), swarm_address(buyer, s)),
                None => f,
            }
        };
        let mut authors: BTreeSet<String> = authors.clone();
        authors.insert(c.me.clone());
        let keys: Vec<PublicKey> = authors
            .iter()
            .filter_map(|a| PublicKey::from_hex(a).ok())
            .collect();
        c.relay.fetch_by_authors(&keys, filter).await
    }

    fn memberships_of(
        events: &[Event],
        buyer: &str,
        swarm: Option<&str>,
    ) -> Vec<(Event, Membership)> {
        latest_memberships(events)
            .into_iter()
            .filter(|(_, m)| m.buyer == buyer && swarm.is_none_or(|s| s == m.swarm))
            .map(|(e, m)| (e.clone(), m))
            .collect()
    }

    async fn names(c: &Ctx, keys: impl IntoIterator<Item = String>) -> BTreeMap<String, String> {
        let keys: std::collections::HashSet<String> = keys.into_iter().collect();
        match maxplayer_core::profile::fetch_names_async(&c.home, &keys).await {
            Ok(names) => names
                .into_iter()
                .filter_map(|(k, v)| v.map(|v| (k, v)))
                .collect(),
            Err(_) => BTreeMap::new(),
        }
    }

    fn member_row(m: &Membership, e: &Event, d: &Decisions, eligible: Option<bool>) -> Value {
        let mut row = json!({
            "seller": m.seller,
            "npub": npub(&m.seller),
            "swarm": m.swarm,
            "minutes": m.minutes,
            "status": m.status,
            "since": m.since,
            "note": m.note,
            "event_id": e.id.to_hex(),
            "decision": d.buyer_wide(&m.seller),
            "swarm_decision": d.for_swarm(&m.seller, &m.swarm),
            "accepted": d.allows(&m.seller, &m.swarm),
        });
        if let Some(eligible) = eligible {
            row["eligible"] = json!(eligible);
        }
        row
    }

    fn member_line(row: &Value) -> String {
        let decision = match (row["decision"].as_str(), row["swarm_decision"].as_str()) {
            (None, None) => "undecided".to_owned(),
            (Some(w), None) => w.to_owned(),
            (None, Some(s)) => format!("{s} in this swarm"),
            (Some(w), Some(s)) => format!("{w}, {s} in this swarm"),
        };
        let mut line = format!(
            "  {} {} min/day, {}, {decision}",
            row["npub"].as_str().unwrap_or(""),
            row["minutes"],
            row["status"].as_str().unwrap_or(""),
        );
        if let Some(e) = row["eligible"].as_bool() {
            line.push_str(if e { ", eligible" } else { ", not eligible" });
        }
        line.push('\n');
        line
    }

    // ---- commands ----------------------------------------------------------------------------

    async fn list(c: &mut Ctx, all: bool) -> Result<(Value, String), String> {
        let events = c
            .relay
            .fetch(vec![Filter::new().kind(Kind::from(SWARM_KIND))])
            .await?;
        let mut swarms: Vec<(Event, Swarm)> = latest_swarms(&events)
            .into_iter()
            .filter(|(_, s)| all || s.status == "open")
            .map(|(e, s)| (e.clone(), s))
            .collect();
        swarms.sort_by_key(|(e, _)| std::cmp::Reverse(e.created_at));
        let buyers: BTreeSet<String> = swarms.iter().map(|(_, s)| s.buyer.clone()).collect();
        let buyer_keys: Vec<PublicKey> = buyers
            .iter()
            .filter_map(|b| PublicKey::from_hex(b).ok())
            .collect();
        let decisions = decision_events(c, &buyer_keys).await?;
        let job_events = c
            .relay
            .fetch_by_authors(&buyer_keys, || {
                Filter::new().kind(Kind::from(SWARM_JOB_KIND))
            })
            .await?;
        let jobs = latest_swarm_jobs(&job_events);
        let names = names(c, buyers.iter().cloned()).await;
        let mut rows = Vec::new();
        let mut human = String::new();
        let mut per_buyer: BTreeMap<String, (Decisions, Vec<(Event, Membership)>)> =
            BTreeMap::new();
        for buyer in &buyers {
            let d = Decisions::read(buyer, &decisions);
            let events = membership_events(c, buyer, None, &d.trusted(None)).await?;
            per_buyer.insert(buyer.clone(), (d, memberships_of(&events, buyer, None)));
        }
        for (_, s) in &swarms {
            let (d, memberships) = &per_buyer[&s.buyer];
            let members = memberships
                .iter()
                .filter(|(_, m)| {
                    m.swarm == s.id && m.status == "joined" && d.allows(&m.seller, &m.swarm)
                })
                .count();
            let mine = memberships
                .iter()
                .any(|(_, m)| m.swarm == s.id && m.seller == c.me && m.status == "joined");
            let open_jobs = jobs
                .iter()
                .filter(|(_, j)| j.swarm == s.id && j.buyer == s.buyer && j.is_open())
                .count();
            let address = naddr(c, SWARM_KIND, &s.buyer, &s.id);
            let name = names.get(&s.buyer).cloned();
            human.push_str(&format!(
                "{} ({}{}){}\n  {}\n  buyer {}{}\n  members {members}, open swarm jobs {open_jobs}, tasks {}\n  {address}\n",
                s.name,
                s.id,
                if s.status == "open" { "" } else { ", closed" },
                if mine { " [member]" } else { "" },
                first_line(&s.purpose),
                npub(&s.buyer),
                name.as_ref().map(|n| format!(" ({n})")).unwrap_or_default(),
                s.tasks,
            ));
            rows.push(json!({
                "swarm": s,
                "address": address,
                "purpose_line": first_line(&s.purpose),
                "buyer_npub": npub(&s.buyer),
                "buyer_name": name,
                "members": members,
                "open_jobs": open_jobs,
                "member": mine,
            }));
        }
        if rows.is_empty() {
            human.push_str(if all {
                "No swarms on this relay.\n"
            } else {
                "No open swarms on this relay (--all adds closed ones).\n"
            });
        }
        Ok((
            json!({"ok": true, "relay": c.relay.url(), "swarms": rows}),
            human,
        ))
    }

    async fn show(c: &mut Ctx, address: &str) -> Result<(Value, String), String> {
        let (buyer, d) = parse_address(address, SWARM_KIND)?;
        let (_, s) = find_swarm(c, &buyer, &d).await?;
        let decisions = decision_events(c, &[public_key(&buyer)?]).await?;
        let dec = Decisions::read(&buyer, &decisions);
        let events = membership_events(c, &buyer, Some(&d), &dec.trusted(Some(&d))).await?;
        let memberships = memberships_of(&events, &buyer, Some(&d));
        let jobs = jobs(c, &buyer, Some(&d)).await?;
        let mut all = events.clone();
        all.extend(decisions.iter().cloned());
        let members: Vec<Value> = memberships
            .iter()
            .map(|(e, m)| member_row(m, e, &dec, None))
            .collect();
        let job_rows: Vec<Value> = jobs
            .iter()
            .map(|(_, j)| {
                json!({"job": j, "address": naddr(c, SWARM_JOB_KIND, &j.buyer, &j.id),
                    "eligible": eligible_sellers(j, &all).len()})
            })
            .collect();
        let mut human = format!(
            "{} ({}, {}, tasks {})\nbuyer {}\n{}\n\n{}\n\nMembers ({}):\n",
            s.name,
            s.id,
            s.status,
            s.tasks,
            npub(&s.buyer),
            naddr(c, SWARM_KIND, &buyer, &d),
            s.purpose.trim(),
            members.len()
        );
        for row in &members {
            human.push_str(&member_line(row));
        }
        human.push_str(&format!("Swarm jobs ({}):\n", job_rows.len()));
        for (_, j) in &jobs {
            human.push_str(&format!(
                "  {} [{}] {} → {}, budget {} sat\n    {}\n",
                j.title,
                j.status,
                time(j.starts),
                time(j.ends),
                j.budget_sats,
                naddr(c, SWARM_JOB_KIND, &j.buyer, &j.id)
            ));
        }
        Ok((
            json!({"ok": true, "swarm": s, "address": naddr(c, SWARM_KIND, &buyer, &d), "members": members, "jobs": job_rows}),
            human,
        ))
    }

    /// This key's latest membership of `buyer`'s swarm `d`, and the events it was read from.
    async fn own_membership(
        c: &Ctx,
        buyer: &str,
        d: &str,
    ) -> Result<(Vec<Event>, Option<(Event, Membership)>), String> {
        let events = c
            .relay
            .fetch(vec![
                Filter::new()
                    .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
                    .author(public_key(&c.me)?)
                    .identifier(format!("{buyer}:{d}")),
            ])
            .await?;
        let current = memberships_of(&events, buyer, Some(d)).into_iter().next();
        Ok((events, current))
    }

    async fn join(c: &mut Ctx, o: &Opts, address: &str) -> Result<(Value, String), String> {
        let (buyer, d) = parse_address(address, SWARM_KIND)?;
        let minutes: u64 = o
            .required("--minutes")?
            .parse()
            .map_err(|_| "--minutes must be a whole number 1..480")?;
        if !(1..=MINUTES_MAX).contains(&minutes) {
            return Err("--minutes must be 1..480".into());
        }
        let (_, s) = find_swarm(c, &buyer, &d).await?;
        if s.status != "open" {
            return Err(format!("swarm {} is closed; nothing was published", s.name));
        }
        if s.tasks == "private" && !c.home.config.privacy.private_jobs {
            return Err(format!(
                "swarm {} sends private tasks, and this home has [privacy] private_jobs = false; turn it on to join. Nothing was published.",
                s.name
            ));
        }
        let (own, current) = own_membership(c, &buyer, &d).await?;
        let decisions = decision_events(c, &[public_key(&buyer)?]).await?;
        let dec = Decisions::read(&buyer, &decisions);
        let jobs: Vec<SwarmJob> = jobs(c, &buyer, Some(&d))
            .await?
            .into_iter()
            .map(|(_, j)| j)
            .collect();
        let joined = current.as_ref().filter(|(_, m)| m.status == "joined");
        let created = replacement_timestamp(
            c.now,
            &own,
            &c.me,
            SWARM_MEMBERSHIP_KIND,
            &format!("{buyer}:{d}"),
        )?;
        if let Some((event, m)) = joined {
            if m.minutes == minutes && o.get("--note").is_none_or(|n| n == m.note) {
                return Ok((
                    json!({"ok": true, "unchanged": true, "membership": m}),
                    format!(
                        "Already in {} with {minutes} min/day; nothing changed.\n",
                        s.name
                    ),
                ));
            }
            // Only a membership the buyer accepted counts; editing a pending one forfeits nothing.
            if dec.allows(&c.me, &d)
                && let Some(j) = blocking_job(&jobs, event, m, c.now, created)
            {
                return Err(format!(
                    "you're in a running swarm job of {} ({}, ends {}). Change your minutes or note after {} (15 minutes after it ends, for clock drift); republishing now would make you ineligible for it. Do not leave and rejoin either: that forfeits it too. Nothing was published.",
                    s.name,
                    j.title,
                    time(j.ends),
                    time(j.ends.saturating_add(CREATED_AT_TOLERANCE))
                ));
            }
        }
        let m = Membership {
            buyer: buyer.clone(),
            swarm: d.clone(),
            seller: c.me.clone(),
            since: joined.map(|(_, m)| m.since).unwrap_or(created),
            minutes,
            status: "joined".into(),
            note: o
                .get("--note")
                .map(str::to_owned)
                .or_else(|| joined.map(|(_, m)| m.note.clone()))
                .unwrap_or_default(),
        };
        let draft = m.to_event_draft().map_err(|e| e.to_string())?;
        let id = c.relay.publish(&c.home, draft, created).await?;
        if let Some(j) = running_job(&jobs, &buyer, &d, c.now) {
            c.notes.push(format!(
                "Swarm job \"{}\" is already running; this membership counts from the swarm's next job.",
                j.title
            ));
        }
        if !dec.allows(&c.me, &d) {
            c.notes.push(
                "The buyer has not accepted you in this swarm yet; you get tasks once they do."
                    .into(),
            );
        }
        let config_note = add_buyer_to_allowlist(&mut c.home, &buyer)?;
        c.notes.extend(config_note);
        if !c.home.config.sessions.enabled() {
            c.notes.push(
                "Hint: [sessions] enabled = false on this home, so every task is a new job. With sessions on (docker seat), a swarm job's tasks to you continue one agent conversation.".into(),
            );
        }
        Ok((
            json!({"ok": true, "event_id": id, "membership": m, "swarm": s}),
            format!("Joined {} with {minutes} min/day (event {id}).\n", s.name),
        ))
    }

    /// Append `buyer` to `[seller].accept_offers_only_from`, keeping every entry. The note to print.
    fn add_buyer_to_allowlist(
        home: &mut MaxplayerHome,
        buyer: &str,
    ) -> Result<Option<String>, String> {
        let Some(seller) = home.config.seller.as_ref() else {
            return Ok(Some(format!(
                "This home has no [seller] section. On the home your seller runs from, add {buyer} to [seller] accept_offers_only_from and restart the seller."
            )));
        };
        if seller.accept_offers_only_from.iter().any(|b| b == buyer) {
            return Ok(None);
        }
        home::save_config(home, |config| {
            if let Some(seller) = config.seller.as_mut()
                && !seller.accept_offers_only_from.iter().any(|b| b == buyer)
            {
                seller.accept_offers_only_from.push(buyer.to_owned());
            }
        })
        .map_err(|e| {
            format!("joined, but could not add the buyer to [seller] accept_offers_only_from: {e}")
        })?;
        Ok(Some(format!(
            "Added {buyer} to [seller] accept_offers_only_from. Restart the seller so it takes this buyer's offers."
        )))
    }

    async fn leave(c: &mut Ctx, address: &str) -> Result<(Value, String), String> {
        let (buyer, d) = parse_address(address, SWARM_KIND)?;
        let (own, current) = own_membership(c, &buyer, &d).await?;
        let (_, mut m) = current
            .ok_or("you have no membership of this swarm to leave; nothing was published")?;
        if m.status == "left" {
            return Ok((
                json!({"ok": true, "unchanged": true, "membership": m}),
                "Already left; nothing changed.\n".into(),
            ));
        }
        m.status = "left".into();
        if m.note.encode_utf16().count() > 4096 {
            m.note.clear();
        }
        let created = replacement_timestamp(
            c.now,
            &own,
            &c.me,
            SWARM_MEMBERSHIP_KIND,
            &format!("{buyer}:{d}"),
        )?;
        let id = c
            .relay
            .publish(
                &c.home,
                m.to_event_draft().map_err(|e| e.to_string())?,
                created,
            )
            .await?;
        Ok((
            json!({"ok": true, "event_id": id, "membership": m}),
            format!("Left {d} (event {id}).\n"),
        ))
    }

    fn read_text(path: &str, what: &str) -> Result<String, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("could not read {what} {path}: {e}"))?;
        if text.trim().is_empty() {
            return Err(format!("{what} {path} is empty"));
        }
        Ok(text)
    }

    async fn create(c: &mut Ctx, o: &Opts) -> Result<(Value, String), String> {
        let name = o.required("--name")?.trim().to_owned();
        let purpose = read_text(o.required("--purpose-file")?, "purpose file")?;
        let slug = match o.get("--slug") {
            Some(s) => s.to_owned(),
            None => slugify(&name, 64),
        };
        if !valid_slug(&slug) {
            return Err(format!(
                "invalid slug {slug:?}: 1-64 of a-z, 0-9, -; pass --slug"
            ));
        }
        let existing = c
            .relay
            .fetch(vec![
                Filter::new()
                    .kind(Kind::from(SWARM_KIND))
                    .author(public_key(&c.me)?)
                    .identifier(&slug),
            ])
            .await?;
        if !latest_swarms(&existing).is_empty() {
            return Err(format!(
                "you already have a swarm {slug}; pick another --slug. Nothing was published."
            ));
        }
        let s = Swarm {
            buyer: c.me.clone(),
            id: slug.clone(),
            name,
            purpose,
            status: "open".into(),
            tasks: if o.flag("--private-tasks") {
                "private"
            } else {
                "public"
            }
            .into(),
        };
        let draft = s.to_event_draft().map_err(|e| e.to_string())?;
        let id = c.relay.publish(&c.home, draft, c.now).await?;
        let address = naddr(c, SWARM_KIND, &c.me, &slug);
        Ok((
            json!({"ok": true, "event_id": id, "swarm": s, "address": address}),
            format!("Created swarm {} ({slug}).\n{address}\n", s.name),
        ))
    }

    async fn job_create(c: &mut Ctx, o: &Opts, address: &str) -> Result<(Value, String), String> {
        let (buyer, d) = parse_address(address, SWARM_KIND)?;
        if buyer != c.me {
            return Err("you can only create swarm jobs in your own swarms".into());
        }
        let (_, s) = find_swarm(c, &buyer, &d).await?;
        if s.status != "open" {
            return Err(format!("swarm {} is closed; nothing was published", s.name));
        }
        let goal = read_text(o.required("--goal-file")?, "goal file")?;
        let budget_sats: u64 = o
            .required("--budget-sats")?
            .parse()
            .map_err(|_| "--budget-sats must be a whole number of sats")?;
        let title = match o.get("--title") {
            Some(t) => t.trim().to_owned(),
            None => first_line(&goal)
                .trim_start_matches('#')
                .trim()
                .chars()
                .take(80)
                .collect(),
        };
        let starts = o
            .get("--starts")
            .map(timestamp)
            .transpose()?
            .unwrap_or(c.now);
        let ends = o
            .get("--ends")
            .map(timestamp)
            .transpose()?
            .unwrap_or(starts + DAY);
        let slug = match o.get("--slug") {
            Some(s) => s.to_owned(),
            None => format!("{}-{starts}", slugify(&title, 48))
                .trim_start_matches('-')
                .to_owned(),
        };
        if !valid_slug(&slug) {
            return Err(format!(
                "invalid slug {slug:?}: 1-64 of a-z, 0-9, -; pass --slug"
            ));
        }
        let existing = c
            .relay
            .fetch(vec![
                Filter::new()
                    .kind(Kind::from(SWARM_JOB_KIND))
                    .author(public_key(&c.me)?)
                    .identifier(&slug),
            ])
            .await?;
        if !latest_swarm_jobs(&existing).is_empty() {
            return Err(format!(
                "you already have a swarm job {slug}; pick another --slug. Nothing was published."
            ));
        }
        let j = SwarmJob {
            buyer: c.me.clone(),
            swarm: d,
            id: slug.clone(),
            title,
            goal,
            starts,
            ends,
            budget_sats,
            status: "scheduled".into(),
            update: None,
            result: None,
        };
        let draft = j.to_event_draft().map_err(|e| e.to_string())?;
        let id = c.relay.publish(&c.home, draft, c.now).await?;
        let address = naddr(c, SWARM_JOB_KIND, &c.me, &slug);
        c.notes.push("Status scheduled. Tasks are posted by this buyer's daemon swarm engine, which runs only with [swarm] enabled = true in config.toml (queue tasks with the swarm_job_tasks MCP tool; no planner yet).".into());
        Ok((
            json!({"ok": true, "event_id": id, "job": j, "address": address}),
            format!(
                "Created swarm job \"{}\" in {} ({} → {}, budget {} sat).\n{address}\n",
                j.title,
                s.name,
                time(j.starts),
                time(j.ends),
                j.budget_sats
            ),
        ))
    }

    async fn job_status(c: &mut Ctx, address: &str) -> Result<(Value, String), String> {
        let (buyer, d) = parse_address(address, SWARM_JOB_KIND)?;
        let events = c
            .relay
            .fetch(vec![
                Filter::new()
                    .kind(Kind::from(SWARM_JOB_KIND))
                    .author(public_key(&buyer)?)
                    .identifier(&d),
            ])
            .await?;
        let (event, j) = latest_swarm_jobs(&events)
            .into_iter()
            .find(|(_, j)| j.buyer == buyer && j.id == d)
            .map(|(e, j)| (e.clone(), j))
            .ok_or_else(|| format!("no swarm job {address} on {}", c.relay.url()))?;
        let decisions = decision_events(c, &[public_key(&buyer)?]).await?;
        let dec = Decisions::read(&buyer, &decisions);
        let mut all =
            membership_events(c, &buyer, Some(&j.swarm), &dec.trusted(Some(&j.swarm))).await?;
        let memberships = memberships_of(&all, &buyer, Some(&j.swarm));
        all.extend(decisions);
        let eligible: BTreeSet<String> = eligible_sellers(&j, &all)
            .into_iter()
            .map(|m| m.seller)
            .collect();
        let rows: Vec<Value> = memberships
            .iter()
            .filter(|(_, m)| m.seller == c.me || dec.allows(&m.seller, &m.swarm))
            .map(|(e, m)| member_row(m, e, &dec, Some(eligible.contains(&m.seller))))
            .collect();
        let mut human = format!(
            "{} [{}] in swarm {}\n{} → {}, budget {} sat\n",
            j.title,
            j.status,
            j.swarm,
            time(j.starts),
            time(j.ends),
            j.budget_sats
        );
        if let Some(u) = &j.update {
            human.push_str(&format!("update: {u}\n"));
        }
        if let Some(r) = &j.result {
            human.push_str(&format!("result: {r}\n"));
        }
        human.push_str(&format!(
            "Members ({}, {} eligible):\n",
            rows.len(),
            eligible.len()
        ));
        for row in &rows {
            human.push_str(&member_line(row));
        }
        if !memberships.iter().any(|(_, m)| m.seller == c.me) {
            human.push_str("You are not a member of this swarm.\n");
        }
        Ok((
            json!({"ok": true, "job": j, "job_event_id": event.id.to_hex(), "members": rows}),
            human,
        ))
    }

    async fn decide(c: &mut Ctx, o: &Opts) -> Result<(Value, String), String> {
        let seller = public_key(o.required("--seller")?)?.to_hex();
        let swarm = match o.get("--swarm") {
            Some(a) => {
                let (buyer, d) = parse_address(a, SWARM_KIND)?;
                if buyer != c.me {
                    return Err("--swarm must be one of your own swarms".into());
                }
                find_swarm(c, &buyer, &d).await?;
                Some(d)
            }
            None => None,
        };
        let membership_event_id = match &swarm {
            Some(d) => {
                let events = c
                    .relay
                    .fetch(vec![
                        Filter::new()
                            .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
                            .author(public_key(&seller)?)
                            .identifier(format!("{}:{d}", c.me)),
                    ])
                    .await?;
                memberships_of(&events, &c.me, Some(d))
                    .first()
                    .map(|(e, _)| e.id.to_hex())
            }
            None => None,
        };
        let decision = Decision {
            buyer: c.me.clone(),
            seller: seller.clone(),
            swarm: swarm.clone(),
            decision: if o.flag("--accept") {
                "accepted"
            } else {
                "denied"
            }
            .into(),
            membership_event_id,
            reason: String::new(),
        };
        let d_tag = match &swarm {
            Some(d) => format!("{seller}:{d}"),
            None => seller.clone(),
        };
        let own = c
            .relay
            .fetch(vec![
                Filter::new()
                    .kind(Kind::from(SWARM_DECISION_KIND))
                    .author(public_key(&c.me)?)
                    .identifier(&d_tag),
            ])
            .await?;
        let created = replacement_timestamp(c.now, &own, &c.me, SWARM_DECISION_KIND, &d_tag)?;
        let id = c
            .relay
            .publish(
                &c.home,
                decision.to_event_draft().map_err(|e| e.to_string())?,
                created,
            )
            .await?;
        let scope = match &swarm {
            Some(d) => format!("in swarm {d}"),
            None => "in all your swarms".into(),
        };
        Ok((
            json!({"ok": true, "event_id": id, "decision": decision}),
            format!(
                "{} {} {scope} (event {id}).\n",
                decision.decision,
                npub(&seller)
            ),
        ))
    }

    async fn members(c: &mut Ctx, address: Option<&str>) -> Result<(Value, String), String> {
        let (buyer, swarm) = match address {
            Some(a) => {
                let (buyer, d) = parse_address(a, SWARM_KIND)?;
                (buyer, Some(d))
            }
            None => (c.me.clone(), None),
        };
        let swarm = swarm.as_deref();
        let decisions = decision_events(c, &[public_key(&buyer)?]).await?;
        let dec = Decisions::read(&buyer, &decisions);
        let mut events = membership_events(c, &buyer, swarm, &dec.trusted(swarm)).await?;
        // Untrusted applicant sample: the newest memberships naming this buyer, so it can decide.
        // It never feeds eligibility, and only fills a coordinate the trusted reads did not return.
        let mut sample_filter = Filter::new()
            .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
            .pubkey(public_key(&buyer)?)
            .limit(APPLICANT_LIMIT);
        if let Some(s) = swarm {
            sample_filter = sample_filter.custom_tag(tag_a(), swarm_address(&buyer, s));
        }
        let sample = c.relay.fetch(vec![sample_filter]).await?;
        let incomplete = sample.len() >= APPLICANT_LIMIT;
        let trusted: BTreeSet<(PublicKey, Option<String>)> = events
            .iter()
            .map(|e| (e.pubkey, e.tags.identifier().map(str::to_owned)))
            .collect();
        events.extend(
            sample
                .into_iter()
                .filter(|e| !trusted.contains(&(e.pubkey, e.tags.identifier().map(str::to_owned)))),
        );
        let mut memberships = memberships_of(&events, &buyer, swarm);
        memberships.sort_by(|a, b| {
            a.1.swarm
                .cmp(&b.1.swarm)
                .then(b.0.created_at.cmp(&a.0.created_at))
        });
        let rows: Vec<Value> = memberships
            .iter()
            .map(|(e, m)| member_row(m, e, &dec, None))
            .collect();
        let mut human = String::new();
        let mut last = None;
        for ((_, m), row) in memberships.iter().zip(&rows) {
            if last != Some(&m.swarm) {
                human.push_str(&format!("{}:\n", m.swarm));
                last = Some(&m.swarm);
            }
            human.push_str(&member_line(row));
        }
        if rows.is_empty() {
            human.push_str("No memberships yet.\n");
        }
        if incomplete {
            c.notes.push(format!(
                "Showing the newest {APPLICANT_LIMIT} applicants; older undecided ones may be missing."
            ));
        }
        Ok((
            json!({"ok": true, "buyer": buyer, "members": rows, "applicants_incomplete": incomplete}),
            human,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parse_accepts_each_command_and_refuses_misplaced_options() {
        for ok in [
            &["list", "--all", "--json"][..],
            &["show", "x/y"],
            &["join", "x/y", "--minutes", "60", "--note", "hi"],
            &["leave", "x/y", "--home", "/tmp/h"],
            &[
                "create",
                "--name",
                "Math",
                "--purpose-file",
                "p.md",
                "--private-tasks",
            ],
            &[
                "job",
                "create",
                "x/y",
                "--goal-file",
                "g.md",
                "--budget-sats",
                "10",
            ],
            &["job", "status", "x/y"],
            &["decide", "--seller", "ab", "--accept", "--swarm", "x/y"],
            &["members"],
            &["members", "x/y"],
        ] {
            assert!(parse(&args(ok)).is_ok(), "{ok:?}");
        }
        for bad in [
            &["join", "x/y", "--accept"][..],
            &["list", "--minutes", "5"],
            &["decide", "--seller", "ab"],
            &["decide", "--seller", "ab", "--accept", "--deny"],
            &["join"],
            &["publish"],
            &["join", "x/y", "--minutes"],
            &["join", "x/y", "--minutes", "1", "--minutes", "2"],
            &["list", "--quorum", "2"],
            &["list", "--relay", "wss://x"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn help_prints_usage_to_stdout() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&args(&["--help"]), &mut out, &mut err), SUCCESS);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .starts_with("Usage: maxplayer swarm")
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(run(&args(&["publish"]), &mut out, &mut err), USAGE_ERROR);
        assert!(out.is_empty());
    }

    #[test]
    fn slugs_are_wire_valid() {
        assert_eq!(slugify("Help activists!", 64), "help-activists");
        assert_eq!(slugify("  Math -- Hard  ", 64), "math-hard");
        assert_eq!(slugify("ÄÖ", 64), "");
        assert_eq!(slugify(&"a".repeat(100), 48).len(), 48);
    }
}

#[cfg(all(test, feature = "wallet"))]
mod relay_tests;
