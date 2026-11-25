//! `maxplayer swarm …` against an in-process relay (docs/specs/swarm.md §12, PR 2).
use super::*;
use maxplayer_core::gateway::EventDraft;
use maxplayer_core::swarm::*;
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Net {
    rt: tokio::runtime::Runtime,
    _relay: LocalRelay,
    url: String,
}

fn net() -> Net {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (relay, url) = rt.block_on(async {
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.unwrap();
        let url = relay.url().await.to_string();
        (relay, url)
    });
    Net {
        rt,
        _relay: relay,
        url,
    }
}

struct Home {
    root: PathBuf,
    hex: String,
    keys: Keys,
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn home(net: &Net, extra: &str) -> Home {
    let root = std::env::temp_dir().join(format!(
        "maxplayer-swarm-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("config.toml"),
        format!("relay_url = '{}'\n{extra}", net.url),
    )
    .unwrap();
    let h = maxplayer_core::home::bootstrap(&root).unwrap();
    let keys = Keys::parse(&maxplayer_core::home::read_secret_key_hex(&h).unwrap()).unwrap();
    Home {
        hex: keys.public_key().to_hex(),
        root,
        keys,
    }
}

const SELLER_SECTION: &str = "[seller]\nagent_command = ['true']\nrate_sats = 1\ngit_remote = 'https://relay.example/git/x/y.git'\n";

struct Out {
    code: i32,
    out: String,
    err: String,
}
impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.out)
            .unwrap_or_else(|e| panic!("{e}: {} / {}", self.out, self.err))
    }
}

fn swarm(h: &Home, a: &[&str]) -> Out {
    let mut args = vec!["--home".to_owned(), h.root.display().to_string()];
    args.extend(a.iter().map(|s| s.to_string()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err);
    Out {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn ok(h: &Home, a: &[&str]) -> Value {
    let mut a = a.to_vec();
    a.push("--json");
    let o = swarm(h, &a);
    assert_eq!(o.code, 0, "{a:?}: {}", o.err);
    o.json()
}

fn file(h: &Home, name: &str, text: &str) -> String {
    let p = h.root.join(name);
    std::fs::write(&p, text).unwrap();
    p.display().to_string()
}

fn create(h: &Home, name: &str, extra: &[&str]) -> String {
    let purpose = file(h, "purpose.md", &format!("{name} purpose\nmore"));
    let mut a = vec!["create", "--name", name, "--purpose-file", &purpose];
    a.extend_from_slice(extra);
    ok(h, &a)["address"].as_str().unwrap().to_owned()
}

fn now() -> u64 {
    Timestamp::now().as_secs()
}

fn publish(net: &Net, keys: &Keys, draft: EventDraft, t: u64) {
    net.rt.block_on(async {
        let client = Client::new(keys.clone());
        client.add_relay(&net.url).await.unwrap();
        client.connect().await;
        let event = EventBuilder::new(Kind::from(draft.kind), draft.content)
            .tags(draft.tags.into_iter().map(|t| Tag::parse(t.0).unwrap()))
            .custom_created_at(Timestamp::from(t))
            .sign_with_keys(keys)
            .unwrap();
        client.send_event(&event).await.unwrap();
        client.disconnect().await;
    });
}

fn fetch(net: &Net, filter: Filter) -> Vec<Event> {
    net.rt.block_on(async {
        let client = Client::default();
        client.add_relay(&net.url).await.unwrap();
        client.connect().await;
        let events = client
            .fetch_events(filter, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        client.disconnect().await;
        events.into_iter().collect()
    })
}

fn membership(buyer: &Home, seller: &Home, swarm: &str, minutes: u64, since: u64) -> Membership {
    Membership {
        buyer: buyer.hex.clone(),
        swarm: swarm.into(),
        seller: seller.hex.clone(),
        since,
        minutes,
        status: "joined".into(),
        note: String::new(),
    }
}

fn accept(net: &Net, buyer: &Home, seller: &Home, t: u64) {
    let d = Decision {
        buyer: buyer.hex.clone(),
        seller: seller.hex.clone(),
        swarm: None,
        decision: "accepted".into(),
        membership_event_id: None,
        reason: String::new(),
    };
    publish(net, &buyer.keys, d.to_event_draft().unwrap(), t);
}

fn running_job(net: &Net, buyer: &Home, swarm: &str, starts: u64) {
    let j = SwarmJob {
        buyer: buyer.hex.clone(),
        swarm: swarm.into(),
        id: "today".into(),
        title: "Today".into(),
        goal: "Goal".into(),
        starts,
        ends: starts + 7200,
        budget_sats: 100,
        status: "in_progress".into(),
        update: None,
        result: None,
    };
    publish(net, &buyer.keys, j.to_event_draft().unwrap(), starts - 10);
}

#[test]
fn list_shows_swarms_of_two_buyers() {
    let net = net();
    let (a, b, c) = (home(&net, ""), home(&net, ""), home(&net, ""));
    let math = create(&a, "Math", &[]);
    create(&b, "Help activists", &["--private-tasks"]);
    let list = ok(&c, &["list"]);
    let swarms = list["swarms"].as_array().unwrap();
    assert_eq!(swarms.len(), 2, "{list}");
    let buyers: BTreeSet<&str> = swarms
        .iter()
        .map(|s| s["swarm"]["buyer"].as_str().unwrap())
        .collect();
    assert_eq!(buyers, BTreeSet::from([a.hex.as_str(), b.hex.as_str()]));
    let activists = swarms
        .iter()
        .find(|s| s["swarm"]["id"] == "help-activists")
        .unwrap();
    assert_eq!(activists["swarm"]["tasks"], "private");
    assert_eq!(activists["purpose_line"], "Help activists purpose");
    // Human output names both, and `show` resolves the printed address.
    let human = swarm(&c, &["list"]).out;
    assert!(
        human.contains("Math (math)") && human.contains("Help activists (help-activists)"),
        "{human}"
    );
    let shown = ok(&c, &["show", &math]);
    assert_eq!(shown["swarm"]["name"], "Math");
    // `<npub|hex>/<slug>` works too.
    let shown = ok(&c, &["show", &format!("{}/math", a.hex)]);
    assert_eq!(shown["swarm"]["buyer"], a.hex);
    // A second swarm with the same slug is refused.
    let purpose = file(&a, "p2.md", "again");
    let o = swarm(
        &a,
        &["create", "--name", "Math", "--purpose-file", &purpose],
    );
    assert_eq!(o.code, 2, "{}", o.err);
    assert!(o.err.contains("already have a swarm math"), "{}", o.err);
}

#[test]
fn join_publishes_per_swarm_membership_and_appends_the_buyer() {
    let net = net();
    let buyer = home(&net, "");
    let kept = "ef".repeat(32);
    let seller = home(
        &net,
        &format!("{SELLER_SECTION}accept_offers_only_from = ['{kept}']\n"),
    );
    let math = create(&buyer, "Math", &[]);
    let o = swarm(
        &seller,
        &["join", &math, "--minutes", "60", "--note", "number theory"],
    );
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(o.err.contains("Restart the seller"), "{}", o.err);
    assert!(o.err.contains("not accepted you"), "{}", o.err);
    let events = fetch(
        &net,
        Filter::new()
            .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
            .author(seller.keys.public_key()),
    );
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(
        e.tags.identifier(),
        Some(format!("{}:math", buyer.hex).as_str())
    );
    let m = parse_membership(e).unwrap();
    assert_eq!(
        (m.buyer.as_str(), m.swarm.as_str(), m.minutes),
        (buyer.hex.as_str(), "math", 60)
    );
    assert_eq!(m.note, "number theory");
    // The config keeps every entry and gains the buyer once.
    let h = maxplayer_core::home::bootstrap(&seller.root).unwrap();
    assert_eq!(
        h.config.seller.unwrap().accept_offers_only_from,
        vec![kept.clone(), buyer.hex.clone()]
    );
    // Same minutes, note omitted: a no-op that publishes nothing.
    let again = ok(&seller, &["join", &math, "--minutes", "60"]);
    assert_eq!(again["unchanged"], true);
    let h = maxplayer_core::home::bootstrap(&seller.root).unwrap();
    assert_eq!(h.config.seller.unwrap().accept_offers_only_from.len(), 2);
    // The buyer sees the applicant, accepts it for this swarm only, and then sees it accepted.
    let members = ok(&buyer, &["members"]);
    assert_eq!(members["members"][0]["seller"], seller.hex);
    assert_eq!(members["members"][0]["accepted"], false);
    ok(
        &buyer,
        &[
            "decide",
            "--seller",
            &seller.hex,
            "--accept",
            "--swarm",
            &math,
        ],
    );
    let members = ok(&buyer, &["members", &math]);
    assert_eq!(members["members"][0]["swarm_decision"], "accepted");
    assert_eq!(members["members"][0]["decision"], Value::Null);
    assert_eq!(members["members"][0]["accepted"], true);
    let list = ok(&seller, &["list"]);
    assert_eq!(list["swarms"][0]["members"], 1);
    assert_eq!(list["swarms"][0]["member"], true);
    // Leave publishes `left` on the same coordinate.
    ok(&seller, &["leave", &math]);
    let events = fetch(
        &net,
        Filter::new()
            .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
            .author(seller.keys.public_key()),
    );
    assert_eq!(events.len(), 1, "replaceable: one coordinate");
    assert_eq!(parse_membership(&events[0]).unwrap().status, "left");
    assert_eq!(ok(&seller, &["leave", &math])["unchanged"], true);
}

#[test]
fn join_without_a_seller_section_says_where_to_add_the_buyer() {
    let net = net();
    // Sessions are on by default; the hint is for a home that turned them off.
    let (buyer, seller) = (home(&net, ""), home(&net, "[sessions]\nenabled = false\n"));
    let math = create(&buyer, "Math", &[]);
    let o = swarm(&seller, &["join", &math, "--minutes", "30"]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(
        o.err.contains("no [seller] section") && o.err.contains(&buyer.hex),
        "{}",
        o.err
    );
    assert!(o.err.contains("[sessions] enabled = false"), "{}", o.err);
    // Default home (sessions unset = on): no hint.
    let other = home(&net, "");
    let o = swarm(&other, &["join", &math, "--minutes", "30"]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(!o.err.contains("[sessions] enabled = false"), "{}", o.err);
}

#[test]
fn minutes_change_refused_while_a_running_job_counts_the_membership() {
    let net = net();
    let buyer = home(&net, "");
    let seller = home(&net, SELLER_SECTION);
    let math = create(&buyer, "Math", &[]);
    let t = now();
    let m = membership(&buyer, &seller, "math", 60, t - 1000);
    publish(&net, &seller.keys, m.to_event_draft().unwrap(), t - 1000);
    accept(&net, &buyer, &seller, t - 1000);
    running_job(&net, &buyer, "math", t - 500);
    let o = swarm(&seller, &["join", &math, "--minutes", "90"]);
    assert_eq!(o.code, 2, "{}", o.out);
    assert!(
        o.err.contains("running swarm job") && o.err.contains("Nothing was published"),
        "{}",
        o.err
    );
    let events = fetch(
        &net,
        Filter::new()
            .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
            .author(seller.keys.public_key()),
    );
    assert_eq!(parse_membership(&events[0]).unwrap().minutes, 60);
    // The job counts the seller.
    let status = ok(&seller, &["job", "status", &format!("{}/today", buyer.hex)]);
    assert_eq!(status["members"][0]["eligible"], true, "{status}");
}

#[test]
fn join_during_a_running_job_counts_from_the_next_job() {
    let net = net();
    let buyer = home(&net, "");
    let seller = home(&net, SELLER_SECTION);
    let math = create(&buyer, "Math", &[]);
    let t = now();
    accept(&net, &buyer, &seller, t - 1000);
    running_job(&net, &buyer, "math", t - 500);
    let o = swarm(&seller, &["join", &math, "--minutes", "45"]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(
        o.err.contains("counts from the swarm's next job"),
        "{}",
        o.err
    );
    let status = ok(&seller, &["job", "status", &format!("{}/today", buyer.hex)]);
    assert_eq!(status["members"][0]["seller"], seller.hex);
    assert_eq!(status["members"][0]["eligible"], false, "{status}");
}

#[test]
fn closed_and_private_swarms_refuse_joins_that_cannot_work() {
    let net = net();
    let buyer = home(&net, "");
    let strict = home(
        &net,
        &format!("[privacy]\nprivate_jobs = false\n{SELLER_SECTION}"),
    );
    let private = create(&buyer, "Quiet", &["--private-tasks"]);
    let o = swarm(&strict, &["join", &private, "--minutes", "10"]);
    assert_eq!(o.code, 2);
    assert!(o.err.contains("private_jobs = false"), "{}", o.err);
    let closed = Swarm {
        buyer: buyer.hex.clone(),
        id: "old".into(),
        name: "Old".into(),
        purpose: "done".into(),
        status: "closed".into(),
        tasks: "public".into(),
    };
    publish(&net, &buyer.keys, closed.to_event_draft().unwrap(), now());
    let o = swarm(
        &strict,
        &["join", &format!("{}/old", buyer.hex), "--minutes", "10"],
    );
    assert_eq!(o.code, 2);
    assert!(o.err.contains("closed"), "{}", o.err);
    // Closed swarms are listed only with --all.
    assert_eq!(
        ok(&strict, &["list"])["swarms"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        ok(&strict, &["list", "--all"])["swarms"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        fetch(&net, Filter::new().kind(Kind::from(SWARM_MEMBERSHIP_KIND))).is_empty(),
        "nothing was published"
    );
}

#[test]
fn buyer_commands_only_act_on_the_buyers_own_swarms() {
    let net = net();
    let (a, b) = (home(&net, ""), home(&net, ""));
    let math = create(&a, "Math", &[]);
    let goal = file(&b, "goal.md", "# Find a counterexample\nDetails");
    let o = swarm(
        &b,
        &[
            "job",
            "create",
            &math,
            "--goal-file",
            &goal,
            "--budget-sats",
            "100",
        ],
    );
    assert_eq!(o.code, 2);
    assert!(o.err.contains("your own swarms"), "{}", o.err);
    let o = swarm(
        &b,
        &["decide", "--seller", &a.hex, "--deny", "--swarm", &math],
    );
    assert_eq!(o.code, 2);
    assert!(o.err.contains("your own swarms"), "{}", o.err);
    // The owner creates a swarm job: required `a`, default window now → +24 h, title from the goal.
    let goal = file(&a, "goal.md", "# Find a counterexample\nDetails");
    let created = ok(
        &a,
        &[
            "job",
            "create",
            &math,
            "--goal-file",
            &goal,
            "--budget-sats",
            "100",
        ],
    );
    let job = &created["job"];
    assert_eq!(job["title"], "Find a counterexample");
    assert_eq!(job["swarm"], "math");
    assert_eq!(
        job["ends"].as_u64().unwrap() - job["starts"].as_u64().unwrap(),
        86_400
    );
    let events = fetch(&net, Filter::new().kind(Kind::from(SWARM_JOB_KIND)));
    assert_eq!(events.len(), 1);
    let a_tag = events[0]
        .tags
        .iter()
        .find(|t| t.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::A)))
        .unwrap();
    assert_eq!(
        a_tag.content(),
        Some(format!("30344:{}:math", a.hex).as_str())
    );
    let status = ok(&b, &["job", "status", created["address"].as_str().unwrap()]);
    assert_eq!(status["job"]["status"], "scheduled");
    // A buyer-wide decision has no swarm.
    let d = ok(&a, &["decide", "--seller", &b.hex, "--accept"]);
    assert_eq!(d["decision"]["swarm"], Value::Null);
    let events = fetch(&net, Filter::new().kind(Kind::from(SWARM_DECISION_KIND)));
    assert_eq!(events[0].tags.identifier(), Some(b.hex.as_str()));
}
