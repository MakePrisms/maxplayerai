//! The swarm engine against a stub seller and stub relay (docs/specs/swarm.md §12, PR 4).
use super::*;
use crate::gateway::nostr::event_builder;
use crate::heartbeat::HeartbeatDraft;
use nostr_sdk::prelude::{JsonUtil, Keys, Timestamp};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const DAY: u64 = 86_400;
/// 01:00 UTC: every test runs inside one UTC day.
const T0: u64 = 1_800_000_000 - (1_800_000_000 % DAY) + 3_600;
const MINT: &str = "https://mint.example";

fn sign(keys: &Keys, draft: &EventDraft, at: u64) -> Event {
    event_builder(draft)
        .unwrap()
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(keys)
        .unwrap()
}

/// A relay plus sellers: swarm events as published, a fresh heartbeat per seller on every read,
/// and a seller that is awarded every post at once and delivers when the test says so.
#[derive(Default)]
struct Stub {
    now: AtomicU64,
    events: StdMutex<Vec<Event>>,
    sellers: StdMutex<Vec<(Keys, u64)>>,
    /// Every post attempt, failed ones included.
    posts: StdMutex<Vec<Value>>,
    jobs: StdMutex<BTreeMap<String, TaskView>>,
    turns: StdMutex<BTreeMap<String, TaskView>>,
    /// `(session job id, follow-up id, turn)`.
    replies: StdMutex<Vec<(String, String, u32)>>,
    /// The `deadline_secs` each reply asked for, in order.
    reply_secs: StdMutex<Vec<u64>>,
    closes: StdMutex<Vec<String>>,
    published: StdMutex<Vec<EventDraft>>,
    session_claims: AtomicBool,
    refuse_reply: AtomicBool,
    fail_post: AtomicBool,
    next: AtomicU64,
    /// `(slots, free_slots)` every seller's beat states; `None` states none (an older seller).
    seats: StdMutex<Option<(usize, usize)>>,
}

impl Stub {
    fn id(&self) -> String {
        format!("{:064x}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }
    fn deliver(&self, job_id: &str) {
        self.jobs.lock().unwrap().get_mut(job_id).unwrap().progress = Progress::Settled {
            answer: Some(format!("answer {job_id}")),
            context_reset: false,
        };
    }
    fn deliver_turn(&self, followup: &str) {
        self.turns
            .lock()
            .unwrap()
            .get_mut(followup)
            .unwrap()
            .progress = Progress::Settled {
            answer: Some(format!("turn answer {followup}")),
            context_reset: false,
        };
    }
    fn posts(&self) -> Vec<Value> {
        self.posts.lock().unwrap().clone()
    }
    fn job_ids(&self) -> Vec<String> {
        self.jobs.lock().unwrap().keys().cloned().collect()
    }
}

#[async_trait::async_trait]
impl SwarmIo for Stub {
    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
    async fn read(&self, _since: u64) -> Result<Vec<Event>, String> {
        let mut out = self.events.lock().unwrap().clone();
        for (keys, rate) in self.sellers.lock().unwrap().iter() {
            let mut beat = HeartbeatDraft::new(true, 0, *rate, vec![MINT.into()]);
            if let Some((total, free)) = *self.seats.lock().unwrap() {
                beat = beat.with_slots(crate::heartbeat::SeatSlots::new(total, free));
            }
            let beat = beat.to_event_draft();
            out.push(sign(keys, &beat, self.now()));
        }
        Ok(out)
    }
    async fn publish(&self, draft: EventDraft, _created_at: u64) -> Result<String, String> {
        self.published.lock().unwrap().push(draft);
        Ok(self.id())
    }
    async fn post(&self, params: Value) -> Result<Option<String>, String> {
        self.posts.lock().unwrap().push(params);
        if self.fail_post.load(Ordering::SeqCst) {
            return Err("relay down".into());
        }
        let id = self.id();
        self.jobs.lock().unwrap().insert(
            id.clone(),
            TaskView {
                awarded_at: Some(self.now()),
                session: self.session_claims.load(Ordering::SeqCst),
                progress: Progress::Pending,
            },
        );
        Ok(Some(id))
    }
    async fn task(&self, job_id: &str) -> Result<TaskView, String> {
        self.jobs
            .lock()
            .unwrap()
            .get(job_id)
            .cloned()
            .ok_or_else(|| "unknown job".into())
    }
    async fn reply(
        &self,
        job_id: &str,
        _text: &str,
        secs: u64,
        _cap: u64,
    ) -> Result<(String, u32), String> {
        self.reply_secs.lock().unwrap().push(secs);
        if self.refuse_reply.load(Ordering::SeqCst) {
            return Err("seller does not support sessions; post a new job".into());
        }
        let turn = 2 + self
            .replies
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.0 == job_id)
            .count() as u32;
        let fid = self.id();
        self.turns.lock().unwrap().insert(
            fid.clone(),
            TaskView {
                awarded_at: None,
                session: true,
                progress: Progress::Pending,
            },
        );
        self.replies
            .lock()
            .unwrap()
            .push((job_id.to_owned(), fid.clone(), turn));
        Ok((fid, turn))
    }
    async fn turn(&self, _job_id: &str, followup: &str) -> Result<TaskView, String> {
        self.turns
            .lock()
            .unwrap()
            .get(followup)
            .cloned()
            .ok_or_else(|| "unknown turn".into())
    }
    async fn close_session(&self, job_id: &str, _seller: &str) -> Result<(), String> {
        self.closes.lock().unwrap().push(job_id.to_owned());
        Ok(())
    }
}

struct Fx {
    dir: tempfile::TempDir,
    buyer: Keys,
    sellers: Vec<Keys>,
    stub: Stub,
}

impl Fx {
    fn new(sellers: usize, rate: u64) -> Self {
        let fx = Self {
            dir: tempfile::tempdir().unwrap(),
            buyer: Keys::generate(),
            sellers: (0..sellers).map(|_| Keys::generate()).collect(),
            stub: Stub::default(),
        };
        *fx.stub.sellers.lock().unwrap() = fx.sellers.iter().map(|k| (k.clone(), rate)).collect();
        fx.stub.now.store(T0, Ordering::SeqCst);
        fx
    }
    fn me(&self) -> String {
        self.buyer.public_key().to_hex()
    }
    fn seller(&self, i: usize) -> String {
        self.sellers[i].public_key().to_hex()
    }
    fn engine(&self, sessions: bool) -> Engine {
        Engine {
            store: BuyerStore::open(self.dir.path().join("buyer.sqlite")).unwrap(),
            buyer: self.me(),
            sessions,
        }
    }
    fn push(&self, e: Event) {
        self.stub.events.lock().unwrap().push(e);
    }
    fn swarm(&self, d: &str, tasks: &str) {
        let s = Swarm {
            buyer: self.me(),
            id: d.into(),
            name: d.into(),
            purpose: "p".into(),
            status: "open".into(),
            tasks: tasks.into(),
        };
        self.push(sign(&self.buyer, &s.to_event_draft().unwrap(), T0 - 1_000));
    }
    /// Seller `i` joins `swarm` before any job starts; the buyer accepts it buyer-wide.
    fn member(&self, i: usize, swarm: &str) {
        let m = Membership {
            buyer: self.me(),
            swarm: swarm.into(),
            seller: self.seller(i),
            since: T0 - 500,
            minutes: 120,
            status: "joined".into(),
            note: String::new(),
        };
        self.push(sign(
            &self.sellers[i],
            &m.to_event_draft().unwrap(),
            T0 - 500,
        ));
        let d = Decision {
            buyer: self.me(),
            seller: self.seller(i),
            swarm: None,
            decision: "accepted".into(),
            membership_event_id: None,
            reason: String::new(),
        };
        self.push(sign(&self.buyer, &d.to_event_draft().unwrap(), T0 - 400));
    }
    fn job(&self, d: &str, swarm: &str, budget: u64) {
        let j = SwarmJob {
            buyer: self.me(),
            swarm: swarm.into(),
            id: d.into(),
            title: d.into(),
            goal: "goal".into(),
            starts: T0,
            ends: T0 + 6 * 3_600,
            budget_sats: budget,
            status: "scheduled".into(),
            update: None,
            result: None,
        };
        self.push(sign(&self.buyer, &j.to_event_draft().unwrap(), T0 - 100));
    }
    fn at(&self, t: u64) {
        self.stub.now.store(t, Ordering::SeqCst);
    }
    fn queue(&self, e: &Engine, job: &str, text: &str, fresh: bool) -> i64 {
        let t = NewTask {
            text: text.into(),
            duration_secs: MIN_SESSION_TURN_SECS,
            max_sats: 100,
            fresh,
            ..NewTask::default()
        };
        e.store
            .queue_swarm_task(job, &t, "agent", T0 as i64)
            .unwrap()
    }
    /// One swarm `math` with job `math-1`, sellers `0..n` members.
    fn math(sellers: usize, rate: u64, budget: u64, tasks: &str) -> Self {
        let fx = Self::new(sellers, rate);
        fx.swarm("math", tasks);
        for i in 0..sellers {
            fx.member(i, "math");
        }
        fx.job("math-1", "math", budget);
        fx
    }
}

fn task(e: &Engine, id: i64) -> TaskRow {
    e.store.swarm_task(id).unwrap().unwrap()
}

/// §12: fan-out of N tasks over M members, one task per member at a time, all collected.
#[tokio::test]
async fn fan_out_of_n_tasks_over_m_members() {
    let fx = Fx::math(3, 10, 10_000, "public");
    let e = fx.engine(false);
    let ids: Vec<i64> = (0..7)
        .map(|n| fx.queue(&e, "math-1", &format!("task {n}"), false))
        .collect();
    let mut now = T0;
    for _ in 0..6 {
        fx.at(now);
        e.tick(&fx.stub).await.unwrap();
        let running: Vec<String> = e
            .store
            .swarm_tasks("math-1")
            .unwrap()
            .into_iter()
            .filter(|t| t.state == "posted")
            .filter_map(|t| t.seller)
            .collect();
        let distinct: BTreeSet<&String> = running.iter().collect();
        assert_eq!(
            distinct.len(),
            running.len(),
            "one running task per member: {running:?}"
        );
        assert!(running.len() <= 3);
        for j in fx.stub.job_ids() {
            fx.stub.deliver(&j);
        }
        now += 120;
    }
    let posts = fx.stub.posts();
    assert_eq!(posts.len(), 7, "each task posted exactly once");
    let prefix = crate::swarm_minutes::task_prefix(&fx.me(), "math-1");
    let sellers: BTreeSet<String> = (0..3).map(|i| fx.seller(i)).collect();
    let mut requests = BTreeSet::new();
    for p in &posts {
        assert!(p["task"].as_str().unwrap().starts_with(&prefix), "{p}");
        assert_eq!(p["visibility"], "public");
        assert_eq!(p["output"], "text/plain");
        assert_eq!(
            (p["amount_sats"].as_u64(), p["max_sats"].as_u64()),
            (Some(10), Some(10))
        );
        assert!(sellers.contains(p["seller_pubkey"].as_str().unwrap()));
        assert!(requests.insert(p["request_id"].as_str().unwrap().to_owned()));
    }
    for id in ids {
        let t = task(&e, id);
        assert_eq!(t.state, "collected", "{t:?}");
        assert!(t.answer.unwrap().starts_with("answer "));
    }
    // The swarm job went in_progress on the relay, with a tally in `update`.
    let published = fx.stub.published.lock().unwrap().clone();
    let first = published.first().expect("status published");
    assert!(first.tags.iter().any(|t| t.0 == ["status", "in_progress"]));
    assert!(first.tags.iter().any(|t| t.first() == Some("update")));
}

/// §12: the swarm job's budget is never exceeded, counting held and spent prices.
#[tokio::test]
async fn budget_never_exceeded() {
    let fx = Fx::math(3, 100, 250, "public");
    let e = fx.engine(false);
    for n in 0..5 {
        fx.queue(&e, "math-1", &format!("task {n}"), false);
    }
    for round in 0..4 {
        fx.at(T0 + round * 120);
        e.tick(&fx.stub).await.unwrap();
        let committed: u64 = fx
            .stub
            .posts()
            .iter()
            .map(|p| p["amount_sats"].as_u64().unwrap())
            .sum();
        assert!(
            committed <= 250,
            "round {round}: {committed} sats committed of 250"
        );
        for j in fx.stub.job_ids() {
            fx.stub.deliver(&j);
        }
    }
    assert_eq!(fx.stub.posts().len(), 2);
    let job = e.store.swarm_job("math-1").unwrap().unwrap();
    assert!(job.idle_reason.unwrap().contains("would exceed the budget"));
}

/// §12: a restart resumes from the store: a task claimed but not posted is re-posted with the
/// identical request, running tasks keep their sellers busy and settle after the restart.
#[tokio::test]
async fn restart_resumes_from_the_store() {
    let fx = Fx::math(2, 10, 10_000, "public");
    let e = fx.engine(false);
    for n in 0..3 {
        fx.queue(&e, "math-1", &format!("task {n}"), false);
    }
    fx.stub.fail_post.store(true, Ordering::SeqCst);
    assert!(e.tick(&fx.stub).await.is_err());
    let failed = fx.stub.posts();
    assert_eq!(failed.len(), 2);
    drop(e);
    fx.stub.fail_post.store(false, Ordering::SeqCst);
    fx.at(T0 + 30);
    let e = fx.engine(false);
    e.tick(&fx.stub).await.unwrap();
    let posts = fx.stub.posts();
    assert_eq!(
        posts.len(),
        4,
        "the two claimed tasks are re-posted, the third waits: {posts:?}"
    );
    assert_eq!(
        posts[2..].to_vec(),
        failed,
        "identical requests (same request_id)"
    );
    drop(e);
    for j in fx.stub.job_ids() {
        fx.stub.deliver(&j);
    }
    fx.at(T0 + 90);
    let e = fx.engine(false);
    e.tick(&fx.stub).await.unwrap();
    let states: Vec<String> = e
        .store
        .swarm_tasks("math-1")
        .unwrap()
        .into_iter()
        .map(|t| t.state)
        .collect();
    assert_eq!(
        states.iter().filter(|s| *s == "collected").count(),
        2,
        "{states:?}"
    );
    assert_eq!(
        fx.stub.posts().len(),
        5,
        "the freed seller took the third task"
    );
}

/// §12: an expired task frees its seller (deadline + grace, no delivery).
#[tokio::test]
async fn expired_task_frees_its_seller() {
    let fx = Fx::math(1, 10, 10_000, "public");
    let e = fx.engine(false);
    let a = fx.queue(&e, "math-1", "a", false);
    let b = fx.queue(&e, "math-1", "b", false);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(
        (task(&e, a).state.as_str(), task(&e, b).state.as_str()),
        ("posted", "queued")
    );
    fx.at(T0 + MIN_SESSION_TURN_SECS + EXPIRED_GRACE_SECS);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(task(&e, a).state, "posted", "not before deadline + grace");
    fx.at(T0 + MIN_SESSION_TURN_SECS + EXPIRED_GRACE_SECS + 1);
    e.tick(&fx.stub).await.unwrap();
    let ta = task(&e, a);
    assert_eq!(ta.state, "failed");
    assert!(ta.outcome.unwrap().starts_with("expired"));
    assert!(
        !ta.released,
        "an awarded task's funds stay held until settled or released"
    );
    assert_eq!(task(&e, b).state, "posted");
    assert_eq!(task(&e, b).seller, Some(fx.seller(0)));
}

/// §1.14 (Josip 18 Nov): a member of two of this buyer's swarms runs one task at a time across
/// both, never one from each at once; the next goes out once the running one is collected.
#[tokio::test]
async fn member_of_two_swarms_runs_one_task_across_both() {
    let fx = Fx::new(1, 10);
    fx.swarm("math", "public");
    fx.swarm("physics", "public");
    fx.member(0, "math");
    fx.member(0, "physics");
    fx.job("math-1", "math", 10_000);
    fx.job("phys-1", "physics", 10_000);
    let e = fx.engine(false);
    let m1 = fx.queue(&e, "math-1", "m1", false);
    let m2 = fx.queue(&e, "math-1", "m2", false);
    let p1 = fx.queue(&e, "phys-1", "p1", false);
    e.tick(&fx.stub).await.unwrap();
    let states = [m1, m2, p1].map(|id| task(&e, id).state);
    assert_eq!(states, ["posted", "queued", "queued"].map(String::from));
    assert_eq!(fx.stub.posts().len(), 1, "one task across both swarms");
    for job in ["math-1", "phys-1"] {
        let why = e
            .store
            .swarm_job(job)
            .unwrap()
            .unwrap()
            .idle_reason
            .unwrap();
        assert!(why.contains("1 busy"), "{job}: {why}");
    }
    // Later ticks while m1 still runs: nothing more, from either swarm or a new job.
    fx.job("math-2", "math", 10_000);
    fx.at(T0 + 60);
    e.tick(&fx.stub).await.unwrap();
    let m3 = fx.queue(&e, "math-2", "m3", false);
    fx.at(T0 + 120);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(
        task(&e, p1).state,
        "queued",
        "busy in math, so not sent physics"
    );
    assert_eq!(task(&e, m3).state, "queued", "busy across a swarm's jobs");
    assert_eq!(fx.stub.posts().len(), 1);
    // m1 collected: the seller is free again and gets exactly one next task.
    let job_m1 = fx.stub.job_ids()[0].clone();
    fx.stub.deliver(&job_m1);
    fx.at(T0 + 300);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(task(&e, m1).state, "collected");
    let running = [m2, m3, p1]
        .iter()
        .filter(|&&id| task(&e, id).state == "posted")
        .count();
    assert_eq!(running, 1, "one next task, not one per swarm");
    assert_eq!(fx.stub.posts().len(), 2);
}

/// §1.14 with free seats: a seller whose fresh beat states two free seats gets one task from
/// each of two swarms at once — still one per swarm — and the third swarm's waits until a newer
/// beat shows a seat.
#[tokio::test]
async fn a_seller_with_free_seats_runs_a_task_from_each_swarm() {
    let fx = Fx::new(1, 10);
    *fx.stub.seats.lock().unwrap() = Some((2, 2));
    for (swarm, job) in [("math", "math-1"), ("physics", "phys-1"), ("bio", "bio-1")] {
        fx.swarm(swarm, "public");
        fx.member(0, swarm);
        fx.job(job, swarm, 10_000);
    }
    let e = fx.engine(false);
    let m1 = fx.queue(&e, "math-1", "m1", false);
    let m2 = fx.queue(&e, "math-1", "m2", false);
    let p1 = fx.queue(&e, "phys-1", "p1", false);
    let b1 = fx.queue(&e, "bio-1", "b1", false);
    let firsts = [("math-1", m1), ("phys-1", p1), ("bio-1", b1)];
    e.tick(&fx.stub).await.unwrap();
    let posted = |e: &Engine| {
        firsts
            .iter()
            .filter(|(_, id)| task(e, *id).state == "posted")
            .count()
    };
    assert_eq!(posted(&e), 2, "two seats: one task each from two swarms");
    assert_eq!(
        task(&e, m2).state,
        "queued",
        "still one running task per swarm"
    );
    assert_eq!(fx.stub.posts().len(), 2);
    let (waiting, _) = firsts
        .iter()
        .find(|(_, id)| task(&e, *id).state == "queued")
        .unwrap();
    let members = e.store.swarm_job(waiting).unwrap().unwrap().members_json;
    assert!(members.contains("\"seats_left\":0"), "{members}");

    // The seller claimed both: its next beat (newer than both awards) says no seat is free.
    *fx.stub.seats.lock().unwrap() = Some((2, 0));
    fx.at(T0 + 60);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(posted(&e), 2, "free_slots=0: the third swarm waits");
    // A newer beat shows a seat (the operator raised `slots`): the third swarm's task goes out.
    *fx.stub.seats.lock().unwrap() = Some((3, 1));
    fx.at(T0 + 120);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(
        posted(&e),
        3,
        "a beat newer than the awards shows a free seat"
    );
    assert_eq!(fx.stub.posts().len(), 3);
}

/// A beat stating `free_slots=0` keeps the seller idle even with nothing running from this buyer;
/// one free seat taken by this buyer's own unclaimed offer leaves none.
#[tokio::test]
async fn a_full_seat_or_an_own_unclaimed_offer_leaves_no_seat() {
    let fx = Fx::new(1, 10);
    *fx.stub.seats.lock().unwrap() = Some((2, 0));
    for (swarm, job) in [("math", "math-1"), ("physics", "phys-1")] {
        fx.swarm(swarm, "public");
        fx.member(0, swarm);
        fx.job(job, swarm, 10_000);
    }
    let e = fx.engine(false);
    let m1 = fx.queue(&e, "math-1", "m1", false);
    let p1 = fx.queue(&e, "phys-1", "p1", false);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(fx.stub.posts().len(), 0, "free_slots=0: nothing posted");
    let why = e
        .store
        .swarm_job("math-1")
        .unwrap()
        .unwrap()
        .idle_reason
        .unwrap();
    assert!(why.contains("1 busy"), "{why}");

    *fx.stub.seats.lock().unwrap() = Some((2, 1));
    fx.at(T0 + 60);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(fx.stub.posts().len(), 1, "one free seat: one task");
    // Unawarded (the stub has not awarded it yet): the beat cannot reflect it.
    for v in fx.stub.jobs.lock().unwrap().values_mut() {
        v.awarded_at = None;
    }
    fx.at(T0 + 120);
    e.tick(&fx.stub).await.unwrap();
    let running = [m1, p1]
        .iter()
        .filter(|&&id| task(&e, id).state == "posted")
        .count();
    assert_eq!(
        running, 1,
        "this buyer's unclaimed offer holds the one free seat"
    );
}

/// §12, §7.5: a private swarm's offers carry no task text or swarm link in public events, its
/// minutes come from the store, and the published tally matches the store.
#[tokio::test]
async fn private_swarm_hides_tasks_and_tally_matches_the_store() {
    use crate::private_content::{builders, wire};
    let fx = Fx::math(1, 10, 10_000, "private");
    fx.stub.session_claims.store(true, Ordering::SeqCst);
    let e = fx.engine(true);
    let a = fx.queue(&e, "math-1", "Prove the secret lemma", false);
    let b = fx.queue(&e, "math-1", "second", false);
    e.tick(&fx.stub).await.unwrap();
    let post = fx.stub.posts()[0].clone();
    assert_eq!(post["visibility"], "private");
    assert_eq!(post["output_category"], "text");
    assert_eq!(
        post["task"], "Prove the secret lemma",
        "no public [swarm:…] link"
    );
    // The private offer the post path signs carries neither the text nor a swarm link publicly.
    let draft = crate::gateway::OfferDraft::new(
        post["task"].as_str().unwrap(),
        "text/plain",
        10,
        T0 + 600,
        fx.seller(0),
    );
    let service = Keys::generate().public_key().to_hex();
    let job = "97".repeat(32);
    let host = wire::HostPolicy {
        git_prefix: "https://git.example/jobs/".into(),
        accepted_mints: vec![MINT.into()],
    };
    let options = builders::OfferOptions {
        visibility: wire::Visibility::Private,
        category: wire::Output::Text,
        service: &service,
        job_id: &job,
        attachments: vec![],
        contribution: None,
    };
    let offer = builders::prepare_offer(&fx.buyer, &draft, options, &host)
        .unwrap()
        .event
        .as_json();
    for secret in ["secret lemma", "[swarm:", "math-1"] {
        assert!(
            !offer.contains(secret),
            "public offer leaks {secret:?}: {offer}"
        );
    }
    let job_id = fx.stub.job_ids()[0].clone();
    // The seller raises its rate before turn 2: the turn is still priced like its session's job.
    for (_, rate) in fx.stub.sellers.lock().unwrap().iter_mut() {
        *rate = 25;
    }
    fx.stub.deliver(&job_id);
    fx.at(T0 + 300);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(task(&e, a).state, "collected");
    assert_eq!(
        task(&e, b).price_sats,
        Some(10),
        "turn 2 at the session's price"
    );
    // §7.6: a private swarm's second task to a session-capable seller is a `reply` turn on the
    // private job (encrypted by `reply`), not a second public offer.
    assert_eq!(e.store.swarm_sessions("math-1").unwrap().len(), 1);
    let replies = fx.stub.replies.lock().unwrap().clone();
    assert_eq!(replies.len(), 1, "second task goes out as a session turn");
    assert_eq!((replies[0].0.as_str(), replies[0].2), (job_id.as_str(), 2));
    assert_eq!(fx.stub.posts().len(), 1, "no second offer");
    let events = fx.stub.read(0).await.unwrap();
    let world = e.world(&events, T0 + 300).unwrap();
    let row = e.store.swarm_job("math-1").unwrap().unwrap();
    let tally: Value = serde_json::from_str(&e.tally(&row, &world).unwrap()).unwrap();
    let member = &tally["members"][fx.seller(0)];
    assert_eq!(member["tasks_done"], 1);
    // Award T0 → collect T0+300 (5 min), plus the second task running since T0+300 (0 min).
    assert_eq!(member["minutes_today"].as_f64(), Some(5.0));
    let published = fx.stub.published.lock().unwrap().clone();
    let last = published.last().unwrap();
    let update = last
        .tags
        .iter()
        .find(|t| t.first() == Some("update"))
        .unwrap()
        .0[1]
        .clone();
    assert_eq!(serde_json::from_str::<Value>(&update).unwrap(), tally);
}

/// Seller 0 of `math-1` collected task A at T0+120 with a session-capable claim.
async fn session_open(fx: &Fx, e: &Engine) -> (i64, String) {
    let a = fx.queue(e, "math-1", "a", false);
    e.tick(&fx.stub).await.unwrap();
    let job_a = fx.stub.job_ids()[0].clone();
    fx.stub.deliver(&job_a);
    fx.at(T0 + 120);
    (a, job_a)
}

/// §12, §7.6: the second task to a session-capable seller in one swarm job is a `reply` turn, and
/// its minutes count from the follow-up.
#[tokio::test]
async fn second_task_is_a_reply_turn_counted_from_the_followup() {
    let fx = Fx::math(1, 10, 10_000, "public");
    fx.stub.session_claims.store(true, Ordering::SeqCst);
    let e = fx.engine(true);
    let (a, job_a) = session_open(&fx, &e).await;
    let b = fx.queue(&e, "math-1", "b", false);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(task(&e, a).state, "collected");
    assert_eq!(fx.stub.posts().len(), 1, "no second job");
    let replies = fx.stub.replies.lock().unwrap().clone();
    assert_eq!(replies.len(), 1);
    assert_eq!((replies[0].0.as_str(), replies[0].2), (job_a.as_str(), 2));
    let tb = task(&e, b);
    assert_eq!(
        (tb.state.as_str(), tb.turn, tb.session_job_id.as_deref()),
        ("posted", Some(2), Some(job_a.as_str()))
    );
    assert_eq!(tb.followup_id.as_deref(), Some(replies[0].1.as_str()));
    assert_eq!(
        tb.price_sats,
        Some(10),
        "a turn is priced like its session's job"
    );
    fx.at(T0 + 420);
    let world = e.world(&fx.stub.read(0).await.unwrap(), T0 + 420).unwrap();
    // Turn 1: award T0 → collect T0+120. Turn 2: follow-up T0+120 → now T0+420.
    assert_eq!(world.used_secs["math"][&fx.seller(0)], 120 + 300);
    fx.stub.deliver_turn(&replies[0].1);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(task(&e, b).state, "collected");
    let s = e
        .store
        .swarm_session("math-1", &fx.seller(0))
        .unwrap()
        .unwrap();
    assert_eq!((s.last_turn, s.state.as_str()), (2, "open"));
}

/// §12, §7.6: a `fresh` task, a non-session claim and a dead session each produce a new one-off job.
#[tokio::test]
async fn fresh_non_session_and_dead_session_each_post_a_one_off_job() {
    // fresh
    let fx = Fx::math(1, 10, 10_000, "public");
    fx.stub.session_claims.store(true, Ordering::SeqCst);
    let e = fx.engine(true);
    session_open(&fx, &e).await;
    fx.queue(&e, "math-1", "fresh one", true);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(
        (fx.stub.posts().len(), fx.stub.replies.lock().unwrap().len()),
        (2, 0),
        "fresh"
    );
    // a claim that did not advertise sessions
    let fx = Fx::math(1, 10, 10_000, "public");
    let e = fx.engine(true);
    session_open(&fx, &e).await;
    fx.queue(&e, "math-1", "b", false);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(
        (fx.stub.posts().len(), fx.stub.replies.lock().unwrap().len()),
        (2, 0),
        "non-session claim"
    );
    assert!(e.store.swarm_sessions("math-1").unwrap().is_empty());
    // a dead session: the reply is refused, the task goes back to the queue and out as a new job
    let fx = Fx::math(1, 10, 10_000, "public");
    fx.stub.session_claims.store(true, Ordering::SeqCst);
    fx.stub.refuse_reply.store(true, Ordering::SeqCst);
    let e = fx.engine(true);
    session_open(&fx, &e).await;
    let b = fx.queue(&e, "math-1", "b", false);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(task(&e, b).state, "queued");
    assert!(task(&e, b).outcome.unwrap().contains("requeued"));
    assert_eq!(
        e.store
            .swarm_session("math-1", &fx.seller(0))
            .unwrap()
            .unwrap()
            .state,
        "dead"
    );
    fx.at(T0 + 180);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(fx.stub.posts().len(), 2, "dead session");
    let tb = task(&e, b);
    assert_eq!((tb.state.as_str(), tb.turn), ("posted", None));
}

/// §7.6: a task shorter than [`MIN_SESSION_TURN_SECS`] to a member with a live session goes out as a
/// one-off job (the seller could not start it as a turn); a 3600 s task is still a session turn.
#[tokio::test]
async fn a_short_task_skips_the_live_session_and_a_long_one_uses_it() {
    let fx = Fx::math(1, 10, 10_000, "public");
    fx.stub.session_claims.store(true, Ordering::SeqCst);
    let e = fx.engine(true);
    session_open(&fx, &e).await;
    let short = NewTask {
        text: "short".into(),
        duration_secs: 600,
        max_sats: 100,
        ..NewTask::default()
    };
    let b = e
        .store
        .queue_swarm_task("math-1", &short, "agent", T0 as i64)
        .unwrap();
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(
        (fx.stub.posts().len(), fx.stub.replies.lock().unwrap().len()),
        (2, 0)
    );
    assert_eq!(
        (task(&e, b).state.as_str(), task(&e, b).turn),
        ("posted", None)
    );
    fx.stub.deliver(task(&e, b).job_id.as_deref().unwrap());
    fx.at(T0 + 240);
    let long = NewTask {
        text: "long".into(),
        duration_secs: 3_600,
        ..short
    };
    let c = e
        .store
        .queue_swarm_task("math-1", &long, "agent", T0 as i64)
        .unwrap();
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(task(&e, b).state, "collected");
    assert_eq!(
        (fx.stub.posts().len(), fx.stub.replies.lock().unwrap().len()),
        (2, 1)
    );
    assert_eq!(
        (task(&e, c).state.as_str(), task(&e, c).turn),
        ("posted", Some(2))
    );
}

/// §12, §7.6, §8: closing the swarm job stops posting, lets the in-flight turn settle, then closes
/// its sessions.
#[tokio::test]
async fn closing_the_swarm_job_closes_its_sessions() {
    let fx = Fx::math(1, 10, 10_000, "public");
    fx.stub.session_claims.store(true, Ordering::SeqCst);
    let e = fx.engine(true);
    let (_, job_a) = session_open(&fx, &e).await;
    let b = fx.queue(&e, "math-1", "b", false);
    e.tick(&fx.stub).await.unwrap();
    let c = fx.queue(&e, "math-1", "c", false);
    // swarm_job_close: status + drop the queue (the RPC's two store writes).
    e.store
        .set_swarm_job_status("math-1", "completed", Some("done"), (T0 + 130) as i64)
        .unwrap();
    e.store
        .drop_queued_swarm_tasks("math-1", (T0 + 130) as i64)
        .unwrap();
    fx.at(T0 + 200);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(task(&e, c).state, "dropped");
    assert!(
        fx.stub.closes.lock().unwrap().is_empty(),
        "the running turn settles first"
    );
    let last = fx.stub.published.lock().unwrap().last().cloned().unwrap();
    assert!(last.tags.iter().any(|t| t.0 == ["status", "completed"]));
    assert!(last.tags.iter().any(|t| t.0 == ["result", "done"]));
    let fid = task(&e, b).followup_id.unwrap();
    fx.stub.deliver_turn(&fid);
    fx.at(T0 + 260);
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(
        task(&e, b).state,
        "collected",
        "in-flight tasks still settle"
    );
    assert_eq!(*fx.stub.closes.lock().unwrap(), vec![job_a]);
    assert_eq!(
        e.store
            .swarm_session("math-1", &fx.seller(0))
            .unwrap()
            .unwrap()
            .state,
        "closed"
    );
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(fx.stub.closes.lock().unwrap().len(), 1, "closed once");
    assert_eq!(fx.stub.posts().len(), 1);
}

/// A v9 store opened by this build gains the swarm tables and stamps v10; a fresh store is empty.
#[test]
fn store_v9_upgrades_to_v10_additively() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("buyer.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE buyer_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO buyer_meta VALUES ('schema_version', '9');",
        )
        .unwrap();
    let store = BuyerStore::open(&path).unwrap();
    assert_eq!(store.health().unwrap().schema_version, 10);
    assert!(store.swarm_jobs(None, false).unwrap().is_empty());
    let id = store
        .queue_swarm_task(
            "j",
            &NewTask {
                text: "t".into(),
                duration_secs: 300,
                ..NewTask::default()
            },
            "agent",
            1,
        )
        .unwrap();
    assert_eq!(store.swarm_task(id).unwrap().unwrap().state, "queued");
}

#[test]
fn own_d_accepts_own_addresses_only() {
    let me = "ab".repeat(32);
    let other = "cd".repeat(32);
    assert_eq!(own_d("math", SWARM_KIND, &me).unwrap(), "math");
    assert_eq!(
        own_d(&format!("{me}/math"), SWARM_KIND, &me).unwrap(),
        "math"
    );
    assert_eq!(
        own_d(&format!("30344:{me}:math"), SWARM_KIND, &me).unwrap(),
        "math"
    );
    assert!(own_d(&format!("{other}/math"), SWARM_KIND, &me).is_err());
    assert!(own_d("Math!", SWARM_KIND, &me).is_err());
}

/// §12 PR 4 default-off: with `[swarm]` unset the daemon starts no engine and answers every
/// `swarm_*` method exactly as any unknown method; with it on, the same call reaches the engine.
#[tokio::test]
async fn default_off_daemon_starts_no_engine_and_knows_no_swarm_method() {
    use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.unwrap();
    for enabled in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut home = crate::home::bootstrap(root.path()).unwrap();
        home.config.relay_url = relay.url().await.to_string();
        assert!(!home.config.swarm.enabled, "default off");
        home.config.swarm.enabled = enabled;
        let (_lock, context, _socket) = bootstrap(home).await.unwrap();
        assert!(
            engine_for(&context, String::new()).sessions,
            "default [sessions]: the engine sends session turns"
        );
        assert_eq!(
            spawn(context.clone()),
            enabled,
            "engine started iff enabled"
        );
        for method in METHODS {
            let r = dispatch(
                &context,
                protocol::Request {
                    method: (*method).into(),
                    params: json!({"swarm_job": "x"}),
                    id: json!(1),
                },
            )
            .await;
            let error = r.error.expect("no swarm state in a fresh home");
            if enabled {
                assert_ne!(
                    error.code, CODE_METHOD_NOT_FOUND,
                    "{method}: {}",
                    error.message
                );
            } else {
                assert_eq!(error.code, CODE_METHOD_NOT_FOUND, "{method}");
                assert_eq!(error.message, format!("unknown method: {method}"));
            }
        }
        assert!(context.store.swarm_jobs(None, false).unwrap().is_empty());
    }
}

/// `swarm_job_status` says where a running task stands and how old each seller's announcement is,
/// so a check-in can tell "no claim yet" (and a seat that has gone quiet) from "awarded, working".
#[tokio::test]
async fn status_shows_each_running_task_stage_and_each_seller_announcement_age() {
    let fx = Fx::math(1, 10, 10_000, "public");
    let e = fx.engine(false);
    let id = fx.queue(&e, "math-1", "t1", false);
    fx.at(T0);
    e.tick(&fx.stub).await.unwrap();
    let posted = task(&e, id);
    assert_eq!(posted.state, "posted");
    assert_eq!(
        posted.awarded_at_unix, None,
        "harness check: the award is read on the next tick"
    );
    assert_eq!(local_award(&e.store, &posted), LocalAward::default());
    let view = running_view(&posted, LocalAward::default(), T0 + 40);
    assert_eq!(view["stage"], "awaiting_claim", "{view}");
    assert_eq!(view["stage_secs"], 40, "{view}");

    let members = e.store.swarm_job("math-1").unwrap().unwrap().members_json;
    let members = members_view(&members, T0 + 45);
    let hb = &members[0]["heartbeat"];
    assert_eq!(hb["announced_at_unix"], T0, "{members}");
    assert_eq!(hb["observed_at_unix"], T0, "the tick at T0 took the snapshot: {members}");
    assert_eq!(hb["announced_secs_ago"], 0, "aged as of the snapshot: {members}");
    assert_eq!(hb["checked_secs_ago"], 45, "{members}");

    // Before the engine copies the award over, the buyer's own tables already know: a pinned
    // award attempt reads "awarding", the committed award row reads "awarded" from its own time.
    let job_id = posted.job_id.clone().expect("harness check: posted with a job id");
    let attempt = crate::buyer::store::AwardAttempt {
        job_id: job_id.clone(),
        claim_id: "c".repeat(64),
        seller_pubkey: "s".repeat(64),
        award_event_id: "a".repeat(64),
        event_json: "{}".into(),
        amount_sats: 10,
        quoted_mints_json: "[]".into(),
        offer_deadline_unix: (T0 + 600) as i64,
        send_count: 0,
        relay_url: "wss://relay.invalid".into(),
        state: crate::buyer::store::AttemptState::Pending,
        detail: None,
    };
    e.store.begin_award_attempt(&attempt, (T0 + 10) as i64).unwrap();
    let local = local_award(&e.store, &posted);
    assert_eq!(local, LocalAward { awarded_at: None, awarding: true });
    let view = running_view(&posted, local, T0 + 40);
    assert_eq!(view["stage"], "awarding", "{view}");
    e.store
        .record_award(&job_id, &attempt.claim_id, &attempt.award_event_id, &attempt.seller_pubkey, 10, (T0 + 20) as i64)
        .unwrap();
    let local = local_award(&e.store, &posted);
    assert_eq!(local.awarded_at, Some(T0 + 20));
    let view = running_view(&posted, local, T0 + 90);
    assert_eq!(view["stage"], "awarded", "{view}");
    assert_eq!(view["stage_secs"], 70, "from the award row's time: {view}");
    assert_eq!(view["awarded_at_unix"], T0 + 20, "{view}");

    // Once the task row carries the award, it wins, and the stage is timed from it — not from the
    // post (T0) — so a wrong timestamp would show here.
    e.store.set_swarm_task_awarded(id, T0 + 30).unwrap();
    let awarded = task(&e, id);
    assert_eq!(local_award(&e.store, &awarded), LocalAward::default(), "no store read once synced");
    let view = running_view(&awarded, LocalAward::default(), T0 + 90);
    assert_eq!(view["stage"], "awarded", "{view}");
    assert_eq!(view["stage_secs"], 60, "{view}");
}

/// Every stage, the timestamp each is measured from, and the edges (no timestamp, a clock behind).
#[tokio::test]
async fn running_view_names_every_stage_from_the_right_timestamp() {
    let fx = Fx::math(1, 10, 10_000, "public");
    let e = fx.engine(false);
    let id = fx.queue(&e, "math-1", "t1", false);
    fx.at(T0);
    e.tick(&fx.stub).await.unwrap();
    let base = TaskRow { awarded_at_unix: None, posted_at_unix: Some(T0), ..task(&e, id) };
    let none = LocalAward::default();
    let at = |t: &TaskRow, now: u64| running_view(t, none, now);

    let posting = TaskRow { job_id: None, ..base.clone() };
    assert_eq!(at(&posting, T0 + 7)["stage"], "posting");
    assert_eq!(at(&posting, T0 + 7)["stage_secs"], 7);

    let pending = TaskRow {
        turn: Some(2),
        session_job_id: Some("j".repeat(64)),
        followup_id: None,
        ..base.clone()
    };
    assert_eq!(at(&pending, T0 + 35)["stage"], "turn_pending");
    assert_eq!(at(&pending, T0 + 35)["stage_secs"], 35, "timed from the claim, not null");
    let sent = TaskRow { followup_id: Some("f".repeat(64)), ..pending.clone() };
    assert_eq!(at(&sent, T0 + 50)["stage"], "turn_sent");
    assert_eq!(at(&sent, T0 + 50)["stage_secs"], 50);
    // A session turn ignores any award fields.
    let sent_awarded = TaskRow { awarded_at_unix: Some(T0 + 1), ..sent.clone() };
    assert_eq!(at(&sent_awarded, T0 + 50)["stage"], "turn_sent");
    // A turn number without a session job is not a session turn (as in settle_one).
    let malformed = TaskRow { session_job_id: None, ..pending.clone() };
    assert_eq!(at(&malformed, T0 + 5)["stage"], "awaiting_claim");

    // No timestamp: no duration. A clock behind the stamp: zero, never an underflow.
    let unstamped = TaskRow { posted_at_unix: None, ..base.clone() };
    assert!(at(&unstamped, T0 + 5)["stage_secs"].is_null());
    assert_eq!(at(&base, T0 - 30)["stage_secs"], 0);
}

/// The members snapshot is aged as of when it was taken, so a snapshot that stopped updating (job
/// closed, relay unreadable) never makes a healthy seller look quiet.
#[test]
fn members_view_ages_announcements_as_of_the_snapshot() {
    let snapshot = format!(
        r#"[{{"seller":"a","heartbeat":{{"accepting":true,"announced_at_unix":{a},"observed_at_unix":{o}}}}},
            {{"seller":"b","heartbeat":null}},
            {{"seller":"c","heartbeat":{{"accepting":true,"announced_at_unix":{f},"observed_at_unix":{o}}}}}]"#,
        a = T0 - 120,
        f = T0 + 60,
        o = T0
    );
    // An hour after the last snapshot: the ad was 120 s old when seen; the snapshot is an hour old.
    let view = members_view(&snapshot, T0 + 3_600);
    assert_eq!(view[0]["heartbeat"]["announced_secs_ago"], 120, "{view}");
    assert_eq!(view[0]["heartbeat"]["checked_secs_ago"], 3_600, "{view}");
    assert!(view[1]["heartbeat"].is_null(), "no announcement stays null: {view}");
    assert_eq!(view[2]["heartbeat"]["announced_secs_ago"], 0, "future-dated: zero, no underflow");
    // A snapshot from a build without the fields is shown as it is.
    let old = members_view(r#"[{"seller":"s","heartbeat":{"accepting":true,"announced_at_unix":5}}]"#, T0);
    assert!(old[0]["heartbeat"].get("announced_secs_ago").is_none(), "{old}");
}

/// A task queued with no `duration_secs` (as the swarm procedure queues them)
/// to run until the swarm job ends.
fn until_end(text: &str) -> NewTask {
    NewTask {
        text: text.into(),
        max_sats: 100,
        ..NewTask::default()
    }
}

/// A task with no `duration_secs` gets an offer deadline at the swarm job's `ends`, so the deadline
/// a seller reads in the offer matches the job window it is working in (a fixed 900 s slot gave
/// members ~9 of the ~50 minutes the job still had). The member's minutes left cap it.
#[tokio::test]
async fn a_task_without_duration_runs_until_the_job_ends() {
    // `Fx::job` ends at T0 + 6 h; `Fx::member` offers 120 minutes a day.
    let ends = T0 + 6 * 3_600;
    let fx = Fx::math(1, 10, 10_000, "public");
    let e = fx.engine(false);
    fx.at(ends - 50 * 60);
    let a = e
        .store
        .queue_swarm_task("math-1", &until_end("round 2"), "agent", T0 as i64)
        .unwrap();
    e.tick(&fx.stub).await.unwrap();
    let posts = fx.stub.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0]["deadline_unix"].as_u64(), Some(ends), "{}", posts[0]);
    assert_eq!(task(&e, a).deadline_unix, Some(ends));

    // Far from the end, the member's 120 minutes left bind instead.
    let fx = Fx::math(1, 10, 10_000, "public");
    let e = fx.engine(false);
    fx.at(T0);
    e.store
        .queue_swarm_task("math-1", &until_end("round 1"), "agent", T0 as i64)
        .unwrap();
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(fx.stub.posts()[0]["deadline_unix"].as_u64(), Some(T0 + 120 * 60));

    // An explicit duration is still honoured as given.
    let fx = Fx::math(1, 10, 10_000, "public");
    let e = fx.engine(false);
    fx.at(T0);
    let fixed = NewTask {
        duration_secs: 900,
        ..until_end("fixed")
    };
    e.store
        .queue_swarm_task("math-1", &fixed, "agent", T0 as i64)
        .unwrap();
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(fx.stub.posts()[0]["deadline_unix"].as_u64(), Some(T0 + 900));
}

/// A later round sent as a session turn gets the same window: its follow-up deadline runs to the
/// swarm job's end, not a fixed slot.
#[tokio::test]
async fn a_session_turn_without_duration_runs_until_the_job_ends() {
    let ends = T0 + 6 * 3_600;
    let fx = Fx::math(1, 10, 10_000, "public");
    fx.stub.session_claims.store(true, Ordering::SeqCst);
    let e = fx.engine(true);
    session_open(&fx, &e).await;
    let now = ends - 50 * 60;
    fx.at(now);
    let b = e
        .store
        .queue_swarm_task("math-1", &until_end("round 2"), "agent", T0 as i64)
        .unwrap();
    e.tick(&fx.stub).await.unwrap();
    assert_eq!(fx.stub.posts().len(), 1, "a turn, not a second job");
    assert_eq!(task(&e, b).turn, Some(2));
    assert_eq!(task(&e, b).deadline_unix, Some(ends));
    assert_eq!(*fx.stub.reply_secs.lock().unwrap(), vec![ends - now]);
}

/// Omitting `duration_secs` is accepted (it means "until the job ends"); a short explicit one is not.
#[test]
fn check_task_accepts_an_omitted_duration_and_refuses_a_short_one() {
    let t: NewTask = serde_json::from_value(json!({"text": "t", "max_sats": 1})).unwrap();
    assert_eq!(t.duration_secs, 0);
    check_task(&t).unwrap();
    let short = NewTask {
        duration_secs: MIN_TASK_SECS - 1,
        ..t
    };
    assert!(check_task(&short).unwrap_err().contains("omitted"));
}
