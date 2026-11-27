//! The swarm engine in the buyer daemon (docs/specs/swarm.md §7, §8), without a planner (§9.4,
//! check-in mode): the buyer's agent queues tasks over MCP; every 5 s the engine settles posted
//! tasks, reads swarms, memberships, decisions, heartbeats and task job events from the relay,
//! moves swarm job status, closes finished sessions and dispatches queued tasks to free eligible
//! members. Started only when `[swarm] enabled = true`; with it off nothing here runs and the
//! daemon answers every `swarm_*` method as unknown.
//!
//! Money stays on the existing paths: a task is an ordinary `post_job` (auto-award, delivery
//! watcher settle) or a session `reply` turn. The engine never awards, reserves or pays itself; it
//! only decides what to post, to whom, at which price, within the swarm job's budget.
use super::store::swarm::{NewTask, SwarmJobRow, SwarmRow, TaskRow};
use super::*;
use crate::gateway::{EventDraft, TagSpec};
use crate::heartbeat::{ParsedHeartbeat, SELLER_HEARTBEAT_D, ServingState};
use crate::kinds::{
    JOB_ACCEPT_KIND, JOB_AWARD_KIND, JOB_CLAIM_KIND, JOB_OFFER_KIND, JOB_RESULT_KIND,
    SELLER_HEARTBEAT_KIND, SESSION_FOLLOWUP_KIND, SWARM_DECISION_KIND, SWARM_JOB_KIND, SWARM_KIND,
    SWARM_MEMBERSHIP_KIND,
};
use crate::swarm::{
    Decision, Decisions, Membership, Swarm, SwarmJob, eligible_sellers, latest_memberships,
    latest_swarm_jobs, latest_swarms, swarm_address, valid_hex, valid_slug,
};
use crate::swarm_minutes::{self, TaskFact};
use crate::swarm_relay::{SwarmRelay, replacement_timestamp};
use nostr_sdk::Event;
use std::collections::BTreeSet;

/// One engine pass every 5 s (§7.2).
pub(crate) const TICK: Duration = Duration::from_secs(5);
/// A running swarm job republishes its tally at most this often (§7.2 step 3).
pub(crate) const UPDATE_EVERY_SECS: u64 = 180;
/// Shortest task the engine posts (§7.3).
pub(crate) const MIN_TASK_SECS: u64 = 300;
/// Shortest task sent as a session turn (§7.6); shorter ones go out as one-off jobs. The buyer cannot
/// see the seller's `wrap_up_secs`, and a seller on the default needs 720 s left to start a turn.
pub(crate) const MIN_SESSION_TURN_SECS: u64 = 900;
/// A posted task with no delivery this long after its deadline is `failed` and frees its seller.
/// The seller stops executing by deadline + 420 s (container kill) plus teardown; the runner used
/// the same 600 s (`scripts/challenges/swarm.mjs` EXPIRED_SLOT_GRACE_SECONDS).
pub(crate) const EXPIRED_GRACE_SECS: u64 = 600;
/// A heartbeat older than this is no evidence of a free slot: the seller gets no new task.
pub(crate) const HEARTBEAT_FRESH_SECS: u64 = 1_800;
/// A late delivery of a failed task is still collected within the buyer's pay window.
const PAY_WINDOW_SECS: u64 = 7 * 86_400;
const ANSWER_MAX_CHARS: usize = 50_000;
const RESULTS_PAGE: usize = 10;

/// What a posted task's job (or turn) has come to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    Pending,
    Settled {
        answer: Option<String>,
        context_reset: bool,
    },
    /// Over without a delivery to pay. `released`: no funds are held for it any more.
    Failed {
        reason: String,
        released: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskView {
    pub awarded_at: Option<u64>,
    /// The awarded claim advertised `["session","1"]` (read once the task settled).
    pub session: bool,
    pub progress: Progress,
}

/// Everything the engine does outside its store. The daemon implements it over the existing
/// post/reply/settle paths ([`DaemonIo`]); tests implement it with a stub seller.
#[async_trait::async_trait]
pub(crate) trait SwarmIo: Send + Sync {
    fn now(&self) -> u64;
    /// This buyer's swarms, swarm jobs and decisions; memberships naming it by trusted authors;
    /// those members' heartbeats; its swarm task offers since `since` and their job events.
    async fn read(&self, since: u64) -> Result<Vec<Event>, String>;
    async fn publish(&self, draft: EventDraft, created_at: u64) -> Result<String, String>;
    /// `post_job` (the MCP path, with its `request_id`). `Ok(None)`: still preparing.
    async fn post(&self, params: Value) -> Result<Option<String>, String>;
    async fn task(&self, job_id: &str) -> Result<TaskView, String>;
    /// A session `reply` turn; the follow-up id and turn number.
    async fn reply(
        &self,
        job_id: &str,
        text: &str,
        deadline_secs: u64,
        cap_sats: u64,
    ) -> Result<(String, u32), String>;
    async fn turn(&self, job_id: &str, followup_id: &str) -> Result<TaskView, String>;
    async fn close_session(&self, job_id: &str, seller: &str) -> Result<(), String>;
}

/// What the relay and the store say about this buyer's swarms at one tick.
#[derive(Debug, Default)]
pub(crate) struct World {
    pub swarms: BTreeMap<String, Swarm>,
    /// Eligible members per open swarm job `d` (§4).
    pub eligible: BTreeMap<String, Vec<Membership>>,
    /// Seconds used today, per swarm `d`, per seller (§4, §7.4).
    pub used_secs: BTreeMap<String, BTreeMap<String, u64>>,
    /// Fresh heartbeats per seller.
    pub heartbeats: BTreeMap<String, ParsedHeartbeat>,
    /// `created_at` of each seller's heartbeat in [`Self::heartbeats`].
    pub beat_at: BTreeMap<String, u64>,
}

pub(crate) struct Engine {
    pub store: BuyerStore,
    pub buyer: String,
    /// `[sessions] enabled` on this buyer: `reply` refuses otherwise, so no turn is attempted.
    pub sessions: bool,
}

fn es(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The price a member is posted at (§7.3): its rate. A rate-0 seat that takes no payment gets a
/// free job; a rate-0 seat that does not is never posted below 1 sat (it would not claim).
pub(crate) fn seller_price(hb: &ParsedHeartbeat) -> u64 {
    match hb.rate_sats {
        0 if hb.takes_no_payment => 0,
        0 => 1,
        rate => rate,
    }
}

/// Seats `seller` has left for this buyer per its heartbeat (§1.14, §7.3): the beat's `free_slots`
/// minus this buyer's running tasks to it that the beat may not reflect yet. `None` when the beat
/// states no seats (a seller built before `slots`/`free_slots`): the caller falls back to one
/// running task per seller across all of this buyer's swarms.
///
/// A one-off task holds its seat from the seller's claim, so the beat reflects it only when the beat
/// is newer than its award (the claim precedes the award); a session turn holds its seat from
/// receipt, so only when the beat is newer than its post. Every other running task — unclaimed,
/// claimed but unawarded, or awarded after the beat — is subtracted. That can count a seat twice
/// (a claim the beat already shows), never zero times: the engine may wait one beat too long, but
/// never posts an offer the seller has no seat for by its own statement.
pub(crate) fn seats_left(hb: &ParsedHeartbeat, beat_at: u64, running: &[&TaskRow]) -> Option<u32> {
    let seats = hb.slots?;
    let unreflected = running
        .iter()
        .filter(|t| {
            let since = match t.turn {
                Some(n) if n >= 2 => t.posted_at_unix,
                _ => t.awarded_at_unix,
            };
            since.is_none_or(|at| at >= beat_at)
        })
        .count();
    Some(
        seats
            .free
            .saturating_sub(u32::try_from(unreflected).unwrap_or(u32::MAX)),
    )
}

/// Is `t`, past its deadline, no longer running on its seller? Yes when the seller's beat was sent
/// after the deadline and states every seat free: a seat still executing `t` would count it as
/// taken. Without such a beat (none since the deadline, a busy seat, or a beat stating no seats)
/// the seller stays busy until deadline + [`EXPIRED_GRACE_SECS`], the latest its container can run.
/// One free seat of several is not enough: the others may still be running `t`.
pub(crate) fn idle_since_deadline(
    t: &TaskRow,
    hb: Option<&ParsedHeartbeat>,
    beat_at: u64,
    now: u64,
) -> bool {
    let (Some(deadline), Some(seats)) = (t.deadline_unix, hb.and_then(|h| h.slots)) else {
        return false;
    };
    now > deadline && beat_at > deadline && seats.total > 0 && seats.free >= seats.total
}

/// The heartbeat shows a seat that can take work now.
fn ready(hb: &ParsedHeartbeat) -> bool {
    hb.accepting && hb.serving_state != ServingState::NotServing
}

/// Does this task hold (or has it spent) budget of its swarm job?
fn holds_budget(t: &TaskRow) -> bool {
    match t.state.as_str() {
        "posted" | "collected" => true,
        "failed" => !t.released,
        _ => false,
    }
}

/// Seconds a task gives its seller when posted at `now` to a member with `left` seconds of its
/// minutes left today. A task with an explicit `duration_secs` gets exactly that; one without
/// (`0`) runs until the swarm job's `ends`, capped by the member's minutes, so the offer's deadline
/// matches the job window the task belongs to instead of a short fixed slot.
/// Whether `task`, running `secs`, goes to a member's live session as a turn (§7.6): not marked
/// fresh, and long enough for the seller to start it as a turn. Pricing and posting both ask this,
/// so a task is never priced as a turn and then posted as a new job.
fn rides_session(task: &TaskRow, secs: u64) -> bool {
    !task.fresh && secs >= MIN_SESSION_TURN_SECS
}

pub(crate) fn task_secs(t: &TaskRow, ends: u64, left: u64, now: u64) -> u64 {
    if t.duration_secs == 0 {
        ends.saturating_sub(now).min(left)
    } else {
        t.duration_secs
    }
}

/// The `post_job` arguments of a one-off task (§7.3; private tasks §7.5).
#[allow(clippy::too_many_arguments)]
pub(crate) fn post_params(
    buyer: &str,
    job: &SwarmJobRow,
    swarm: &Swarm,
    task: &TaskRow,
    seller: &str,
    price: u64,
    deadline: u64,
    now: u64,
) -> Value {
    let private = swarm.tasks == "private";
    let text = if private {
        task.text.clone()
    } else {
        format!("{}{}", swarm_minutes::task_prefix(buyer, &job.d), task.text)
    };
    let mut p = json!({
        "task": text,
        "output": "text/plain",
        "visibility": if private { "private" } else { "public" },
        "seller_pubkey": seller,
        "amount_sats": price,
        "max_sats": price,
        "deadline_unix": deadline,
        // Deterministic per attempt: a retry of this post is the identical request.
        "request_id": format!("swarm-{}-{}-{now}", job.d, task.id),
    });
    if private {
        p["output_category"] = json!("text");
    }
    if price == 0 {
        p["payment"] = json!("none");
    }
    p
}

/// A minutes fact for one of the engine's own tasks, from the store (§4): turn 1 from its award,
/// a later turn from its follow-up, ended by its collect, capped at its deadline. The store is the
/// only source for private tasks, which carry no public link.
fn store_fact(t: &TaskRow, swarm: &str) -> Option<TaskFact> {
    if !matches!(t.state.as_str(), "posted" | "collected" | "failed") {
        return None;
    }
    let (id, start, turn) = match t.turn {
        Some(n) if n >= 2 => (t.followup_id.clone()?, t.posted_at_unix?, n),
        _ => (t.job_id.clone()?, t.awarded_at_unix?, 1),
    };
    Some(TaskFact {
        id,
        job: t.job.clone(),
        swarm: swarm.to_owned(),
        seller: t.seller.clone()?,
        turn,
        start,
        deadline: t.deadline_unix?,
        accepted_at: if t.state == "collected" {
            t.collected_at_unix
        } else {
            None
        },
        backdated: false,
    })
}

fn wire_job(buyer: &str, j: &SwarmJobRow) -> SwarmJob {
    SwarmJob {
        buyer: buyer.to_owned(),
        swarm: j.swarm.clone(),
        id: j.d.clone(),
        title: j.title.clone(),
        goal: j.goal.clone(),
        starts: j.starts,
        ends: j.ends,
        budget_sats: j.budget_sats,
        status: j.status.clone(),
        update: j.published_update.clone(),
        result: j.result.clone(),
    }
}

fn heartbeat_of(e: &Event) -> Option<ParsedHeartbeat> {
    e.verify().ok()?;
    let draft = EventDraft::new(
        e.kind.as_u16(),
        e.tags
            .iter()
            .map(|t| TagSpec(t.as_slice().to_vec()))
            .collect(),
        &e.content,
    );
    crate::heartbeat::parse_heartbeat(&draft).ok()
}

/// Why queued tasks went nowhere this tick, for `swarm_job_status.idle_reason`.
#[derive(Default)]
struct Why {
    busy: usize,
    no_heartbeat: usize,
    pin: usize,
    minutes: usize,
    price: usize,
    budget: usize,
}

impl Engine {
    /// One pass (§7.2): settle, read, status, (planner: PR 5), dispatch.
    pub(crate) async fn tick<I: SwarmIo + ?Sized>(&self, io: &I) -> Result<(), String> {
        let now = io.now();
        let mut errors = self.settle(io, now).await;
        let since = swarm_minutes::day_start(now).saturating_sub(swarm_minutes::DAY_SECS);
        let events = io.read(since).await?;
        let world = self.world(&events, now)?;
        errors.extend(self.statuses(io, &world, &events, now).await?);
        errors.extend(self.close_sessions(io).await?);
        errors.extend(self.dispatch(io, &world, now).await?);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Seconds each seller used today in `swarm`: the relay facts (the board's rule) and, for every
    /// task the relay does not show (private tasks, unpublished events), the store's.
    fn used_secs(
        &self,
        relay: &[TaskFact],
        swarm: &str,
        now: u64,
    ) -> Result<BTreeMap<String, u64>, String> {
        let mut by_id: BTreeMap<String, TaskFact> = relay
            .iter()
            .filter(|f| f.swarm == swarm)
            .map(|f| (f.id.clone(), f.clone()))
            .collect();
        for t in self.store.swarm_tasks_of_swarm(swarm).map_err(es)? {
            if let Some(f) = store_fact(&t, swarm) {
                by_id.entry(f.id.clone()).or_insert(f);
            }
        }
        let facts: Vec<TaskFact> = by_id.into_values().collect();
        Ok(swarm_minutes::used_secs_today(&facts, swarm, now))
    }

    pub(crate) fn world(&self, events: &[Event], now: u64) -> Result<World, String> {
        let buyer = self.buyer.as_str();
        let mine = || events.iter().filter(|e| e.pubkey.to_hex() == buyer);
        let mut world = World::default();
        for (_, s) in latest_swarms(mine()) {
            self.store
                .put_swarm(
                    &SwarmRow {
                        d: s.id.clone(),
                        name: s.name.clone(),
                        purpose: s.purpose.clone(),
                        status: s.status.clone(),
                        tasks: s.tasks.clone(),
                    },
                    now as i64,
                )
                .map_err(es)?;
            world.swarms.insert(s.id.clone(), s);
        }
        let relay_jobs: Vec<SwarmJob> = latest_swarm_jobs(mine())
            .into_iter()
            .map(|(_, j)| j)
            .collect();
        // A swarm job created elsewhere (`maxplayer swarm job create`) is adopted while open.
        for j in &relay_jobs {
            if j.is_open() && j.ends > now && self.store.swarm_job(&j.id).map_err(es)?.is_none() {
                let row = SwarmJobRow {
                    d: j.id.clone(),
                    swarm: j.swarm.clone(),
                    title: j.title.clone(),
                    goal: j.goal.clone(),
                    starts: j.starts,
                    ends: j.ends,
                    budget_sats: j.budget_sats,
                    status: j.status.clone(),
                    result: j.result.clone(),
                    steering: Vec::new(),
                    idle_reason: None,
                    members_json: "[]".into(),
                    published_status: Some(j.status.clone()),
                    published_update: j.update.clone(),
                    last_update_unix: 0,
                };
                self.store.insert_swarm_job(&row, now as i64).map_err(es)?;
            }
        }
        for j in self.store.swarm_jobs(None, true).map_err(es)? {
            let eligible = eligible_sellers(&wire_job(buyer, &j), events);
            world.eligible.insert(j.d.clone(), eligible);
        }
        let relay_facts = swarm_minutes::task_facts(events, buyer, &relay_jobs);
        let mut swarms: BTreeSet<String> = world.swarms.keys().cloned().collect();
        swarms.extend(
            self.store
                .swarm_rows()
                .map_err(es)?
                .into_iter()
                .map(|s| s.d),
        );
        for s in swarms {
            let used = self.used_secs(&relay_facts, &s, now)?;
            world.used_secs.insert(s, used);
        }
        let mut newest: BTreeMap<String, u64> = BTreeMap::new();
        for e in events
            .iter()
            .filter(|e| e.kind.as_u16() == SELLER_HEARTBEAT_KIND)
        {
            let at = e.created_at.as_secs();
            let seller = e.pubkey.to_hex();
            if at.saturating_add(HEARTBEAT_FRESH_SECS) < now
                || newest.get(&seller).is_some_and(|n| *n >= at)
            {
                continue;
            }
            if let Some(hb) = heartbeat_of(e) {
                newest.insert(seller.clone(), at);
                world.beat_at.insert(seller.clone(), at);
                world.heartbeats.insert(seller, hb);
            }
        }
        Ok(world)
    }

    // ---- 1. settle ---------------------------------------------------------------------------

    async fn settle<I: SwarmIo + ?Sized>(&self, io: &I, now: u64) -> Vec<String> {
        let tasks = match self.store.swarm_tasks_live() {
            Ok(tasks) => tasks,
            Err(e) => return vec![e.to_string()],
        };
        let mut errors = Vec::new();
        for t in tasks {
            if let Err(e) = self.settle_one(io, &t, now).await {
                errors.push(format!("task {}: {e}", t.id));
            }
        }
        errors
    }

    fn finish(
        &self,
        t: &TaskRow,
        state: &str,
        outcome: &str,
        answer: Option<String>,
        released: bool,
        now: u64,
    ) -> Result<(), String> {
        let answer = answer.map(|a| a.chars().take(ANSWER_MAX_CHARS).collect::<String>());
        self.store
            .finish_swarm_task(t.id, state, outcome, answer.as_deref(), released, now)
            .map_err(es)
    }

    fn session_dead(&self, t: &TaskRow, now: u64) -> Result<(), String> {
        let (Some(seller), Some(job_id)) = (&t.seller, &t.session_job_id) else {
            return Ok(());
        };
        let last = self
            .store
            .swarm_session(&t.job, seller)
            .map_err(es)?
            .map_or(1, |s| s.last_turn);
        self.store
            .put_swarm_session(&t.job, seller, job_id, last, "dead", now)
            .map_err(es)
    }

    async fn settle_one<I: SwarmIo + ?Sized>(
        &self,
        io: &I,
        t: &TaskRow,
        now: u64,
    ) -> Result<(), String> {
        let deadline = t.deadline_unix.unwrap_or(0);
        let turn = t.turn.filter(|n| *n >= 2).zip(t.session_job_id.clone());
        if t.state == "failed" {
            // A late delivery is still collected (and paid by the watcher) inside the pay window.
            if now > deadline + PAY_WINDOW_SECS {
                return Ok(());
            }
            let view = match (&turn, &t.followup_id, &t.job_id) {
                (Some((_, job_id)), Some(fid), _) => io.turn(job_id, fid).await?,
                (None, _, Some(job_id)) => io.task(job_id).await?,
                _ => return Ok(()),
            };
            match view.progress {
                Progress::Settled { answer, .. } => self.finish(
                    t,
                    "collected",
                    "collected (delivered after it was failed)",
                    answer,
                    false,
                    now,
                )?,
                Progress::Failed { released: true, .. } => {
                    let outcome = t.outcome.clone().unwrap_or_default();
                    self.store
                        .set_swarm_task_outcome(t.id, &outcome, true, now)
                        .map_err(es)?;
                }
                _ => {}
            }
            return Ok(());
        }
        if let Some((n, job_id)) = turn {
            let Some(fid) = &t.followup_id else {
                if now > deadline {
                    self.finish(
                        t,
                        "failed",
                        "the follow-up was not sent before its deadline",
                        None,
                        false,
                        now,
                    )?;
                    return self.session_dead(t, now);
                }
                let job = self
                    .store
                    .swarm_job(&t.job)
                    .map_err(es)?
                    .ok_or("swarm job gone")?;
                return self.reply(io, t, &job, now).await;
            };
            let view = io.turn(&job_id, fid).await?;
            return match view.progress {
                Progress::Settled {
                    answer,
                    context_reset,
                } => {
                    self.finish(t, "collected", "collected", answer, false, now)?;
                    let seller = t.seller.as_deref().unwrap_or_default();
                    let state = if context_reset {
                        "context_reset"
                    } else {
                        "open"
                    };
                    self.store
                        .put_swarm_session(&t.job, seller, &job_id, n, state, now)
                        .map_err(es)
                }
                Progress::Failed { reason, released } => {
                    self.finish(t, "failed", &reason, None, released, now)?;
                    self.session_dead(t, now)
                }
                Progress::Pending if now > deadline + EXPIRED_GRACE_SECS => {
                    let outcome = format!(
                        "expired: no delivery {EXPIRED_GRACE_SECS}s after the turn's deadline"
                    );
                    self.finish(t, "failed", &outcome, None, false, now)?;
                    self.session_dead(t, now)
                }
                Progress::Pending => Ok(()),
            };
        }
        let Some(job_id) = &t.job_id else {
            if now > deadline {
                let why = t.outcome.clone().unwrap_or_else(|| "no job id".into());
                return self.finish(
                    t,
                    "failed",
                    &format!("not posted before its deadline ({why})"),
                    None,
                    true,
                    now,
                );
            }
            let params: Value =
                serde_json::from_str(t.params_json.as_deref().unwrap_or("null")).map_err(es)?;
            return self.post(io, t.id, params, now).await;
        };
        let view = io.task(job_id).await?;
        if let (Some(at), None) = (view.awarded_at, t.awarded_at_unix) {
            self.store.set_swarm_task_awarded(t.id, at).map_err(es)?;
        }
        match view.progress {
            Progress::Settled { answer, .. } => {
                let outcome = if answer.is_some() {
                    "collected".to_owned()
                } else {
                    format!(
                        "collected (no inline answer; git delivery in the buyer store, read_delivery {job_id})"
                    )
                };
                self.finish(t, "collected", &outcome, answer, false, now)?;
                let seller = t.seller.as_deref().unwrap_or_default();
                // One session per seller per swarm job (§7.6): a live one is kept.
                let live = self
                    .store
                    .swarm_session(&t.job, seller)
                    .map_err(es)?
                    .is_some_and(|s| s.state != "dead");
                if view.session && self.sessions && !live {
                    self.store
                        .put_swarm_session(&t.job, seller, job_id, 1, "open", now)
                        .map_err(es)?;
                }
                Ok(())
            }
            Progress::Failed { reason, released } => {
                self.finish(t, "failed", &reason, None, released, now)
            }
            Progress::Pending if now > deadline + EXPIRED_GRACE_SECS => {
                let outcome = format!(
                    "expired: no delivery {EXPIRED_GRACE_SECS}s after its deadline; seller freed"
                );
                // Never awarded: nothing was reserved. Awarded: funds stay held until settled or released.
                self.finish(t, "failed", &outcome, None, view.awarded_at.is_none(), now)
            }
            Progress::Pending => Ok(()),
        }
    }

    async fn post<I: SwarmIo + ?Sized>(
        &self,
        io: &I,
        id: i64,
        params: Value,
        now: u64,
    ) -> Result<(), String> {
        match io.post(params).await {
            Ok(Some(job_id)) => self.store.set_swarm_task_job(id, &job_id, now).map_err(es),
            Ok(None) => self
                .store
                .set_swarm_task_outcome(id, "preparing", false, now)
                .map_err(es),
            Err(e) => {
                self.store
                    .set_swarm_task_outcome(id, &format!("post failed: {e}"), false, now)
                    .map_err(es)?;
                Err(format!("post failed: {e}"))
            }
        }
    }

    async fn reply<I: SwarmIo + ?Sized>(
        &self,
        io: &I,
        t: &TaskRow,
        job: &SwarmJobRow,
        now: u64,
    ) -> Result<(), String> {
        let (Some(job_id), Some(turn)) = (&t.session_job_id, t.turn) else {
            return Err("not a session turn".into());
        };
        let secs = t.deadline_unix.unwrap_or(now).saturating_sub(now).max(60);
        // The session cap is the swarm job's budget: it never binds before the budget does (§7.6).
        match io.reply(job_id, &t.text, secs, job.budget_sats).await {
            Ok((fid, n)) => self
                .store
                .set_swarm_task_followup(t.id, &fid, n, now)
                .map_err(es),
            Err(e) => {
                let kept = self
                    .store
                    .session_turns(job_id)
                    .map_err(es)?
                    .into_iter()
                    .filter(|x| x.turn >= turn && matches!(x.state.as_str(), "pending" | "sent"))
                    .max_by_key(|x| x.turn);
                match kept {
                    // Published: that follow-up is this task's turn.
                    Some(x) if x.state == "sent" => self
                        .store
                        .set_swarm_task_followup(t.id, &x.followup_id, x.turn, now)
                        .map_err(es),
                    // Signed and kept, not yet published: the next tick re-sends the same bytes.
                    Some(_) => self
                        .store
                        .set_swarm_task_outcome(t.id, &format!("reply pending: {e}"), false, now)
                        .map_err(es),
                    // Refused before anything was signed: the session is dead, the task goes back
                    // to the queue and is posted as a new one-off job (§7.6).
                    None => {
                        self.store.requeue_swarm_task(t.id, now).map_err(es)?;
                        self.store
                            .set_swarm_task_outcome(
                                t.id,
                                &format!("session turn refused ({e}); requeued as a one-off task"),
                                false,
                                now,
                            )
                            .map_err(es)?;
                        self.session_dead(t, now)
                    }
                }
            }
        }
    }

    // ---- 3. swarm job status -----------------------------------------------------------------

    fn out_of_minutes(&self, j: &SwarmJobRow, world: &World, now: u64) -> bool {
        let members = world.eligible.get(&j.d).map(Vec::as_slice).unwrap_or(&[]);
        // Minutes come back at the next UTC midnight: only a job ending before then is done.
        let next_day = swarm_minutes::day_start(now) + swarm_minutes::DAY_SECS;
        !members.is_empty()
            && j.ends <= next_day
            && members.iter().all(|m| {
                let used = world
                    .used_secs
                    .get(&j.swarm)
                    .and_then(|u| u.get(&m.seller))
                    .copied()
                    .unwrap_or(0);
                (m.minutes * 60).saturating_sub(used) < MIN_TASK_SECS
            })
    }

    /// The buyer-signed tally in the swarm job's `update` (§4, §7.5): per member, tasks done in
    /// this swarm job and minutes used today in its swarm.
    pub(crate) fn tally(&self, j: &SwarmJobRow, world: &World) -> Result<String, String> {
        let tasks = self.store.swarm_tasks(&j.d).map_err(es)?;
        let mut sellers: BTreeSet<String> = world
            .eligible
            .get(&j.d)
            .into_iter()
            .flatten()
            .map(|m| m.seller.clone())
            .collect();
        sellers.extend(
            tasks
                .iter()
                .filter(|t| holds_budget(t))
                .filter_map(|t| t.seller.clone()),
        );
        let used = world.used_secs.get(&j.swarm);
        let members: serde_json::Map<String, Value> = sellers
            .into_iter()
            .map(|s| {
                let done = tasks
                    .iter()
                    .filter(|t| t.state == "collected" && t.seller.as_deref() == Some(s.as_str()))
                    .count();
                let secs = used.and_then(|u| u.get(&s)).copied().unwrap_or(0);
                let v = json!({"tasks_done": done, "minutes_today": secs as f64 / 60.0});
                (s, v)
            })
            .collect();
        Ok(json!({ "members": members }).to_string())
    }

    async fn statuses<I: SwarmIo + ?Sized>(
        &self,
        io: &I,
        world: &World,
        events: &[Event],
        now: u64,
    ) -> Result<Vec<String>, String> {
        let mut errors = Vec::new();
        for j in self.store.swarm_jobs(None, false).map_err(es)? {
            let mut status = j.status.clone();
            if j.is_open() {
                if status == "scheduled" && now >= j.starts {
                    status = "in_progress".into();
                }
                if now >= j.ends || (status == "in_progress" && self.out_of_minutes(&j, world, now))
                {
                    status = "completed".into();
                }
                if status != j.status {
                    self.store
                        .set_swarm_job_status(&j.d, &status, None, now as i64)
                        .map_err(es)?;
                    if !matches!(status.as_str(), "scheduled" | "in_progress") {
                        self.store
                            .drop_queued_swarm_tasks(&j.d, now as i64)
                            .map_err(es)?;
                    }
                }
            }
            let changed = j.published_status.as_deref() != Some(status.as_str());
            let running = status == "in_progress";
            if !changed && !(running && now >= j.last_update_unix + UPDATE_EVERY_SECS) {
                continue;
            }
            let tally = self.tally(&j, world)?;
            if !changed && j.published_update.as_deref() == Some(tally.as_str()) {
                continue;
            }
            let mut job = wire_job(&self.buyer, &j);
            job.status = status.clone();
            job.update = Some(tally.clone());
            let draft = job.to_event_draft().map_err(es)?;
            let created = replacement_timestamp(now, events, &self.buyer, SWARM_JOB_KIND, &j.d)?;
            match io.publish(draft, created).await {
                Ok(_) => self
                    .store
                    .set_swarm_job_published(&j.d, &status, Some(&tally), now)
                    .map_err(es)?,
                Err(e) => errors.push(format!("swarm job {}: {e}", j.d)),
            }
        }
        Ok(errors)
    }

    /// Close the sessions of swarm jobs that ended, once their in-flight turn settled (§7.6).
    async fn close_sessions<I: SwarmIo + ?Sized>(&self, io: &I) -> Result<Vec<String>, String> {
        let mut errors = Vec::new();
        for s in self.store.swarm_sessions_to_close().map_err(es)? {
            let running = self
                .store
                .swarm_tasks(&s.job)
                .map_err(es)?
                .iter()
                .any(|t| t.state == "posted" && t.seller.as_deref() == Some(s.seller.as_str()));
            if running {
                continue;
            }
            match io.close_session(&s.job_id, &s.seller).await {
                Ok(()) => self
                    .store
                    .put_swarm_session(
                        &s.job,
                        &s.seller,
                        &s.job_id,
                        s.last_turn,
                        "closed",
                        io.now(),
                    )
                    .map_err(es)?,
                Err(e) => errors.push(format!("close session {}: {e}", s.job_id)),
            }
        }
        Ok(errors)
    }

    // ---- 5. dispatch -------------------------------------------------------------------------

    async fn dispatch<I: SwarmIo + ?Sized>(
        &self,
        io: &I,
        world: &World,
        now: u64,
    ) -> Result<Vec<String>, String> {
        let mut errors = Vec::new();
        // Seats across ALL of this buyer's swarms (§1.14): a seller takes another task while its
        // fresh heartbeat shows a free seat after this buyer's running tasks it may not reflect
        // ([`seats_left`]); one running task per swarm per seller still (§7.3: a session runs one
        // turn at a time). A seller whose beat states no seats falls back to one running task
        // across all swarms (Josip 18 Nov). Built once and shared by every job this tick.
        let swarm_of: BTreeMap<String, String> = self
            .store
            .swarm_jobs(None, false)
            .map_err(es)?
            .into_iter()
            .map(|j| (j.d, j.swarm))
            .collect();
        let running: Vec<TaskRow> = self
            .store
            .swarm_tasks_live()
            .map_err(es)?
            .into_iter()
            .filter(|t| t.state == "posted" && t.seller.is_some())
            .collect();
        // A task past its deadline stops holding its seller once the seller's own beat, sent after
        // that deadline, shows every seat free ([`idle_since_deadline`]). It stays `posted` (a late
        // delivery is still collected) but no longer blocks the next task for the rest of the grace.
        let running: Vec<TaskRow> = running
            .into_iter()
            .filter(|t| {
                let Some(seller) = t.seller.as_deref() else {
                    return true;
                };
                let beat_at = world.beat_at.get(seller).copied().unwrap_or(0);
                !idle_since_deadline(t, world.heartbeats.get(seller), beat_at, now)
            })
            .collect();
        // (swarm, seller) pairs with a running task.
        let mut in_swarm: BTreeSet<(String, String)> = running
            .iter()
            .filter_map(|t| Some((swarm_of.get(&t.job)?.clone(), t.seller.clone()?)))
            .collect();
        let mut any_running: BTreeSet<String> =
            running.iter().filter_map(|t| t.seller.clone()).collect();
        // Seats left per seller with a seat-stating beat; tasks sent this tick come off it.
        let mut seats: BTreeMap<String, u32> = BTreeMap::new();
        for (seller, hb) in &world.heartbeats {
            let mine: Vec<&TaskRow> = running
                .iter()
                .filter(|t| t.seller.as_deref() == Some(seller.as_str()))
                .collect();
            let at = world.beat_at.get(seller).copied().unwrap_or(0);
            if let Some(left) = seats_left(hb, at, &mine) {
                seats.insert(seller.clone(), left);
            }
        }
        for j in self.store.swarm_jobs(None, true).map_err(es)? {
            if j.status != "in_progress" {
                continue;
            }
            let Some(swarm) = world.swarms.get(&j.swarm).filter(|s| s.status == "open") else {
                self.store
                    .set_swarm_job_tick(
                        &j.d,
                        Some("its swarm is closed or not on the relay; nothing is posted"),
                        None,
                        now as i64,
                    )
                    .map_err(es)?;
                continue;
            };
            let tasks = self.store.swarm_tasks(&j.d).map_err(es)?;
            let mut reserved: u64 = tasks
                .iter()
                .filter(|t| holds_budget(t))
                .map(|t| t.price_sats.unwrap_or(0))
                .sum();
            let mut queued: Vec<TaskRow> =
                tasks.into_iter().filter(|t| t.state == "queued").collect();
            let members = world.eligible.get(&j.d).cloned().unwrap_or_default();
            let used = world.used_secs.get(&j.swarm);
            let mut why = Why::default();
            let mut rows = Vec::new();
            for m in &members {
                let busy = in_swarm.contains(&(j.swarm.clone(), m.seller.clone()))
                    || match seats.get(&m.seller) {
                        Some(left) => *left == 0,
                        None => any_running.contains(&m.seller),
                    };
                let used_secs = used.and_then(|u| u.get(&m.seller)).copied().unwrap_or(0);
                let left = (m.minutes * 60).saturating_sub(used_secs);
                let hb = world.heartbeats.get(&m.seller);
                rows.push(json!({
                    "seller": m.seller,
                    "minutes_offered": m.minutes,
                    "minutes_used_today": used_secs as f64 / 60.0,
                    "minutes_left": left / 60,
                    "busy": busy,
                    "seats_left": seats.get(&m.seller),
                    // `observed_at_unix`: when this snapshot read the announcement. The snapshot is
                    // rewritten only on a tick that read the relay for a running job, so status ages
                    // the announcement as of THIS instant, never against a later clock.
                    "heartbeat": hb.map(|h| json!({"accepting": h.accepting, "rate_sats": h.rate_sats, "takes_no_payment": h.takes_no_payment, "slots": h.slots.map(|s| s.total), "free_slots": h.slots.map(|s| s.free), "announced_at_unix": world.beat_at.get(&m.seller), "observed_at_unix": now})),
                }));
                if busy {
                    why.busy += 1;
                    continue;
                }
                let Some(hb) = hb.filter(|h| ready(h)) else {
                    why.no_heartbeat += 1;
                    continue;
                };
                let rate = seller_price(hb);
                // A session turn is priced like the session's job (session-jobs §8.1).
                let session_price = self.session_price(&j, &m.seller)?;
                // Only a task that `send` will really post as a turn takes the session's price; a
                // shorter one goes out as a new job, so it is priced at the member's current rate.
                let price_of = |t: &TaskRow, secs: u64| match session_price {
                    Some(p) if rides_session(t, secs) => p,
                    _ => rate,
                };
                let mut first_miss = None;
                let pick = queued.iter().position(|t| {
                    let secs = task_secs(t, j.ends, left, now);
                    let price = price_of(t, secs);
                    let miss = if t.seller_pin.as_deref().is_some_and(|p| p != m.seller) {
                        Some(0)
                    } else if secs < MIN_TASK_SECS || secs > left || now + secs > j.ends {
                        Some(1)
                    } else if price > t.max_sats {
                        Some(2)
                    } else if reserved + price > j.budget_sats {
                        Some(3)
                    } else {
                        None
                    };
                    first_miss = first_miss.or(miss);
                    miss.is_none()
                });
                let Some(i) = pick else {
                    match first_miss {
                        Some(0) => why.pin += 1,
                        Some(1) => why.minutes += 1,
                        Some(2) => why.price += 1,
                        Some(3) => why.budget += 1,
                        _ => {}
                    }
                    continue;
                };
                let task = queued.remove(i);
                let secs = task_secs(&task, j.ends, left, now);
                let price = price_of(&task, secs);
                match self.send(io, &j, swarm, &task, &m.seller, price, secs, now).await {
                    Ok(true) => {
                        in_swarm.insert((j.swarm.clone(), m.seller.clone()));
                        any_running.insert(m.seller.clone());
                        if let Some(left) = seats.get_mut(&m.seller) {
                            *left = left.saturating_sub(1);
                        }
                        reserved += price;
                        // Busy in this swarm now: the rest of its queue waits for this member.
                        why.busy += 1;
                    }
                    Ok(false) => {}
                    Err(e) => errors.push(format!("task {}: {e}", task.id)),
                }
            }
            let idle = if members.is_empty() {
                Some("no eligible members: sellers must join the swarm before the job starts and be accepted".to_owned())
            } else if !queued.is_empty() {
                Some(format!(
                    "{} queued task(s) fit no free eligible member: {} busy, {} without a fresh accepting heartbeat, {} pinned elsewhere, {} without the minutes left (or under {MIN_TASK_SECS}s, or past the job's end), {} rated above max_sats, {} would exceed the budget",
                    queued.len(),
                    why.busy,
                    why.no_heartbeat,
                    why.pin,
                    why.minutes,
                    why.price,
                    why.budget
                ))
            } else {
                None
            };
            let rows = Value::Array(rows).to_string();
            self.store
                .set_swarm_job_tick(&j.d, idle.as_deref(), Some(&rows), now as i64)
                .map_err(es)?;
        }
        Ok(errors)
    }

    /// The price of `seller`'s live session in `job`, when its next task would be a turn.
    fn session_price(&self, job: &SwarmJobRow, seller: &str) -> Result<Option<u64>, String> {
        if !self.sessions {
            return Ok(None);
        }
        let Some(s) = self.store.swarm_session(&job.d, seller).map_err(es)? else {
            return Ok(None);
        };
        if !matches!(s.state.as_str(), "open" | "context_reset") {
            return Ok(None);
        }
        Ok(self
            .store
            .swarm_tasks(&job.d)
            .map_err(es)?
            .iter()
            .find(|t| t.job_id.as_deref() == Some(s.job_id.as_str()))
            .and_then(|t| t.price_sats))
    }

    /// Post `task` to `seller`: a session turn when a live session exists (§7.6), else a new job.
    /// `Ok(false)`: the task was no longer queued (dropped or replaced meanwhile).
    #[allow(clippy::too_many_arguments)]
    async fn send<I: SwarmIo + ?Sized>(
        &self,
        io: &I,
        job: &SwarmJobRow,
        swarm: &Swarm,
        task: &TaskRow,
        seller: &str,
        price: u64,
        secs: u64,
        now: u64,
    ) -> Result<bool, String> {
        let deadline = now + secs;
        // Private swarms use session turns too: `reply` encrypts a private job's follow-up like its
        // offer (session-jobs §2.1), proven live on free and paid private jobs (§7.6 last bullet).
        let session = if self.sessions && rides_session(task, secs) {
            self.store
                .swarm_session(&job.d, seller)
                .map_err(es)?
                .filter(|s| matches!(s.state.as_str(), "open" | "context_reset"))
        } else {
            None
        };
        if let Some(s) = session {
            let next = s.last_turn + 1;
            if !self
                .store
                .claim_swarm_task(
                    task.id,
                    seller,
                    price,
                    deadline,
                    now,
                    None,
                    Some((s.job_id.as_str(), next)),
                )
                .map_err(es)?
            {
                return Ok(false);
            }
            let t = self
                .store
                .swarm_task(task.id)
                .map_err(es)?
                .ok_or("task gone")?;
            self.reply(io, &t, job, now).await?;
            return Ok(true);
        }
        let params = post_params(&self.buyer, job, swarm, task, seller, price, deadline, now);
        if !self
            .store
            .claim_swarm_task(
                task.id,
                seller,
                price,
                deadline,
                now,
                Some(&params.to_string()),
                None,
            )
            .map_err(es)?
        {
            return Ok(false);
        }
        self.post(io, task.id, params, now).await?;
        Ok(true)
    }
}

// ---- the daemon's I/O ------------------------------------------------------------------------

/// [`SwarmIo`] over the daemon: the existing `post_job`/`reply` RPC paths, the buyer store's
/// settle state, and one long-lived connection to the home's relay for swarm reads and writes.
pub(super) struct DaemonIo {
    context: Arc<BuyerContext>,
    buyer: String,
    relay: Mutex<Option<SwarmRelay>>,
}

impl DaemonIo {
    pub(super) fn new(context: Arc<BuyerContext>, buyer: String) -> Self {
        Self {
            context,
            buyer,
            relay: Mutex::new(None),
        }
    }

    async fn connection(&self) -> Result<tokio::sync::MutexGuard<'_, Option<SwarmRelay>>, String> {
        let mut guard = self.relay.lock().await;
        if guard.is_none() {
            *guard = Some(SwarmRelay::connect(&self.context.home).await?);
        }
        Ok(guard)
    }

    async fn drop_connection(guard: &mut Option<SwarmRelay>) {
        if let Some(relay) = guard.take() {
            relay.disconnect().await;
        }
    }
}

fn swarm_task_offer(e: &Event) -> bool {
    e.tags.iter().any(|t| {
        let t = t.as_slice();
        t.first().map(String::as_str) == Some("i")
            && t.get(1).is_some_and(|v| v.starts_with("[swarm:"))
    })
}

async fn read_events(r: &SwarmRelay, buyer: &str, since: u64) -> Result<Vec<Event>, String> {
    use nostr_sdk::prelude::{EventId, Filter, Kind, PublicKey, Timestamp};
    let me = PublicKey::from_hex(buyer).map_err(es)?;
    let mut events = r
        .fetch(vec![
            Filter::new()
                .kinds([SWARM_KIND, SWARM_JOB_KIND, SWARM_DECISION_KIND].map(Kind::from))
                .author(me),
        ])
        .await?;
    let trusted: Vec<PublicKey> = Decisions::read(buyer, &events)
        .trusted(None)
        .iter()
        .filter_map(|k| PublicKey::from_hex(k).ok())
        .collect();
    let memberships = r
        .fetch_by_authors(&trusted, || {
            Filter::new()
                .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
                .pubkey(me)
        })
        .await?;
    let members: Vec<PublicKey> = memberships
        .iter()
        .map(|e| e.pubkey)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let beats = r
        .fetch_by_authors(&members, || {
            Filter::new()
                .kind(Kind::from(SELLER_HEARTBEAT_KIND))
                .identifier(SELLER_HEARTBEAT_D)
        })
        .await?;
    let offers: Vec<Event> = r
        .fetch(vec![
            Filter::new()
                .kind(Kind::from(JOB_OFFER_KIND))
                .author(me)
                .since(Timestamp::from(since)),
        ])
        .await?
        .into_iter()
        .filter(swarm_task_offer)
        .collect();
    let ids: Vec<EventId> = offers.iter().map(|e| e.id).collect();
    let mut related = Vec::new();
    for chunk in ids.chunks(crate::swarm_relay::AUTHOR_CHUNK) {
        related.extend(
            r.fetch(vec![
                Filter::new()
                    .kinds(
                        [
                            JOB_CLAIM_KIND,
                            JOB_RESULT_KIND,
                            JOB_AWARD_KIND,
                            JOB_ACCEPT_KIND,
                            SESSION_FOLLOWUP_KIND,
                        ]
                        .map(Kind::from),
                    )
                    .events(chunk.iter().copied()),
            ])
            .await?,
        );
    }
    events.extend(memberships);
    events.extend(beats);
    events.extend(offers);
    events.extend(related);
    Ok(events)
}

#[async_trait::async_trait]
impl SwarmIo for DaemonIo {
    fn now(&self) -> u64 {
        now_unix().max(0) as u64
    }

    async fn read(&self, since: u64) -> Result<Vec<Event>, String> {
        let mut guard = self.connection().await?;
        let result = read_events(guard.as_ref().expect("connected"), &self.buyer, since).await;
        if result.is_err() {
            Self::drop_connection(&mut guard).await;
        }
        result
    }

    async fn publish(&self, draft: EventDraft, created_at: u64) -> Result<String, String> {
        let mut guard = self.connection().await?;
        let result = guard
            .as_ref()
            .expect("connected")
            .publish(&self.context.home, draft, created_at)
            .await;
        if result.is_err() {
            Self::drop_connection(&mut guard).await;
        }
        result
    }

    async fn post(&self, params: Value) -> Result<Option<String>, String> {
        let response = preparations::start(&self.context, Value::Null, params).await;
        if let Some(error) = response.error {
            return Err(error.message);
        }
        let body = response.result.unwrap_or_default();
        if body["status"] == "preparing" {
            return Ok(None);
        }
        if let Some(refused) = body["publish_refused"].as_str() {
            return Err(format!("the relay refused the offer: {refused}"));
        }
        body["job_id"]
            .as_str()
            .map(|id| Some(id.to_owned()))
            .ok_or_else(|| "post_job returned no job_id".into())
    }

    async fn task(&self, job_id: &str) -> Result<TaskView, String> {
        use super::reservations::ReservationState;
        let c = &self.context;
        let award = c.store.award_record(job_id).map_err(es)?;
        let awarded_at = award.as_ref().map(|a| a.awarded_at_unix.max(0) as u64);
        let reservation = c.store.reservation(job_id).map_err(es)?.map(|(s, _)| s);
        let free = collect::free_collect_record_path(&c.home, job_id).is_file();
        if let Some(award) = &award
            && (free || reservation == Some(ReservationState::Spent))
        {
            let bind = job_lifecycle::load_accepted_bind(&c.home, job_id).map_err(es)?;
            let keys = buyer_keys(&c.home)?;
            let view = job_lifecycle::fetch_job_view_async(
                &c.home,
                &keys,
                job_id,
                RELAY_TIMEOUT,
                self.now(),
            )
            .await
            .map_err(es)?;
            let session = view
                .claims
                .iter()
                .any(|cl| cl.claim_id == award.claim_id && cl.session);
            return Ok(TaskView {
                awarded_at,
                session,
                progress: Progress::Settled {
                    answer: bind.and_then(|b| b.inline_answer),
                    context_reset: false,
                },
            });
        }
        if reservation == Some(ReservationState::Released) {
            return Ok(TaskView {
                awarded_at,
                session: false,
                progress: Progress::Failed {
                    reason: "released: the job ended with nothing to pay".into(),
                    released: true,
                },
            });
        }
        if award.is_none()
            && let Some((state, reason)) = c.store.pending_award_state(job_id).map_err(es)?
            && state == "parked"
        {
            return Ok(TaskView {
                awarded_at,
                session: false,
                progress: Progress::Failed {
                    reason: format!("not awarded: {}", reason.unwrap_or_default()),
                    released: reservation != Some(ReservationState::Reserved),
                },
            });
        }
        Ok(TaskView {
            awarded_at,
            session: false,
            progress: Progress::Pending,
        })
    }

    async fn reply(
        &self,
        job_id: &str,
        text: &str,
        deadline_secs: u64,
        cap_sats: u64,
    ) -> Result<(String, u32), String> {
        let params = json!({"job_id": job_id, "text": text, "deadline_secs": deadline_secs, "max_session_sats": cap_sats});
        let response = session_reply::reply(&self.context, Value::Null, params).await;
        if let Some(error) = response.error {
            return Err(error.message);
        }
        let body = response.result.unwrap_or_default();
        let fid = body["followup_id"]
            .as_str()
            .ok_or("reply returned no followup_id")?;
        let turn = body["turn"].as_u64().ok_or("reply returned no turn")?;
        Ok((fid.to_owned(), turn as u32))
    }

    async fn turn(&self, job_id: &str, followup_id: &str) -> Result<TaskView, String> {
        let c = &self.context;
        let row = c
            .store
            .turn(followup_id)
            .map_err(es)?
            .ok_or("unknown follow-up")?;
        let progress = match row.state.as_str() {
            "collected" => {
                let bind = job_lifecycle::load_accepted_bind(&c.home, followup_id).map_err(es)?;
                let keys = buyer_keys(&c.home)?;
                let view = job_lifecycle::fetch_job_view_async(
                    &c.home,
                    &keys,
                    job_id,
                    RELAY_TIMEOUT,
                    self.now(),
                )
                .await
                .map_err(es)?;
                let context_reset = view.results.iter().any(|r| {
                    r.session.reply.as_deref() == Some(followup_id) && r.session.context_reset
                });
                Progress::Settled {
                    answer: bind.and_then(|b| b.inline_answer),
                    context_reset,
                }
            }
            "refused" | "no_response" => Progress::Failed {
                reason: format!("turn {}: {}", row.turn, row.state),
                released: true,
            },
            _ => Progress::Pending,
        };
        Ok(TaskView {
            awarded_at: None,
            session: true,
            progress,
        })
    }

    async fn close_session(&self, job_id: &str, seller: &str) -> Result<(), String> {
        session_reply::send_close(&self.context, job_id, seller).await
    }
}

/// The engine [`spawn`] runs for this daemon.
fn engine_for(context: &BuyerContext, buyer: String) -> Engine {
    Engine {
        store: context.store.clone(),
        buyer,
        sessions: context.home.config.sessions.enabled(),
    }
}

/// Start the engine when `[swarm] enabled`; `false` (nothing started) otherwise.
pub(super) fn spawn(context: Arc<BuyerContext>) -> bool {
    if !context.home.config.swarm.enabled {
        return false;
    }
    tokio::spawn(async move {
        let buyer = match buyer_keys(&context.home) {
            Ok(keys) => keys.public_key().to_hex(),
            Err(e) => {
                crate::opline!("buyer: swarm engine not started: {e}");
                return;
            }
        };
        let engine = engine_for(&context, buyer.clone());
        let io = DaemonIo::new(context.clone(), buyer);
        crate::opline!("buyer: swarm engine started (no planner: check-in mode)");
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last: Option<String> = None;
        loop {
            ticker.tick().await;
            match engine.tick(&io).await {
                Ok(()) => last = None,
                Err(e) if last.as_deref() != Some(e.as_str()) => {
                    crate::opline!(
                        "buyer: swarm engine: {e}; retrying every {}s",
                        TICK.as_secs()
                    );
                    last = Some(e);
                }
                Err(_) => {}
            }
        }
    });
    true
}

// ---- RPC (§8) --------------------------------------------------------------------------------

pub(super) const METHODS: &[&str] = &[
    "swarm_create",
    "swarm_list",
    "swarm_members",
    "swarm_decide",
    "swarm_job_create",
    "swarm_job_status",
    "swarm_job_tasks",
    "swarm_job_steer",
    "swarm_job_close",
];

pub(super) const NO_PLANNER: &str = "No planner is configured (check-in mode): only tasks you queue \
     (swarm_job_create tasks, swarm_job_tasks add) are dispatched, and results wait for your next \
     swarm_job_status.";

pub(super) async fn rpc(
    context: &Arc<BuyerContext>,
    method: &str,
    id: Value,
    params: Value,
) -> Response {
    match rpc_inner(context, method, params).await {
        Ok(body) => Response::ok(id, body),
        Err(e) => Response::err(id, CODE_REFUSED, e),
    }
}

fn args<T: serde::de::DeserializeOwned>(method: &str, params: Value) -> Result<T, String> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| format!("{method} params: {e}"))
}

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

fn naddr(home: &MaxplayerHome, kind: u16, buyer: &str, d: &str) -> String {
    use nostr_sdk::prelude::*;
    let Ok(pk) = PublicKey::from_hex(buyer) else {
        return format!("{buyer}/{d}");
    };
    let coordinate = Coordinate::new(Kind::from(kind), pk).identifier(d);
    let relays: Vec<RelayUrl> = RelayUrl::parse(&home.config.relay_url)
        .into_iter()
        .collect();
    Nip19Coordinate::new(coordinate, relays)
        .to_bech32()
        .unwrap_or_else(|_| format!("{buyer}/{d}"))
}

/// The `d` of one of THIS buyer's swarms (`kind` 30344) or swarm jobs (30341): an `naddr1…`,
/// `<buyer npub|hex>/<slug>`, `<kind>:<buyer hex>:<slug>`, or a bare slug.
pub(super) fn own_d(raw: &str, kind: u16, me: &str) -> Result<String, String> {
    use nostr_sdk::prelude::*;
    let what = if kind == SWARM_KIND {
        "swarm"
    } else {
        "swarm job"
    };
    let raw = raw.trim();
    let key = |s: &str| -> Result<String, String> {
        if s.starts_with("npub1") {
            PublicKey::from_bech32(s)
                .map(|k| k.to_hex())
                .map_err(|_| format!("invalid key {s}"))
        } else if valid_hex(s) {
            Ok(s.to_owned())
        } else {
            Err(format!("invalid key {s}"))
        }
    };
    let (buyer, d) = if raw.starts_with("naddr1") {
        let n = Nip19Coordinate::from_bech32(raw)
            .map_err(|_| format!("invalid {what} address {raw}"))?;
        if n.coordinate.kind.as_u16() != kind {
            return Err(format!("{raw} is not a {what}"));
        }
        (
            n.coordinate.public_key.to_hex(),
            n.coordinate.identifier.clone(),
        )
    } else if let Some((b, d)) = raw.split_once('/') {
        (key(b)?, d.to_owned())
    } else if let Some(rest) = raw.strip_prefix(&format!("{kind}:")) {
        let (b, d) = rest
            .split_once(':')
            .ok_or_else(|| format!("invalid {what} address {raw}"))?;
        (key(b)?, d.to_owned())
    } else {
        (me.to_owned(), raw.to_owned())
    };
    if buyer != me {
        return Err(format!(
            "{raw} belongs to another buyer: this daemon runs only its own {what}s"
        ));
    }
    if !valid_slug(&d) {
        return Err(format!("invalid {what} slug {d:?}: 1-64 of a-z, 0-9, -"));
    }
    Ok(d)
}

fn check_task(t: &NewTask) -> Result<(), String> {
    if t.text.trim().is_empty() {
        return Err("a task needs non-empty text".into());
    }
    // 0 (omitted) = until the swarm job ends (`task_secs`).
    if t.duration_secs != 0 && t.duration_secs < MIN_TASK_SECS {
        return Err(format!(
            "a task's duration_secs must be at least {MIN_TASK_SECS} (5 min), or omitted to run until the swarm job ends"
        ));
    }
    if t.seller.as_deref().is_some_and(|s| !valid_hex(s)) {
        return Err("a task's seller pin must be a 64-hex public key".into());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateParams {
    name: String,
    purpose: String,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    tasks: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MembersParams {
    #[serde(default)]
    swarm: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideParams {
    seller: String,
    accept: bool,
    #[serde(default)]
    swarm: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobCreateParams {
    swarm: String,
    goal: String,
    budget_sats: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    starts: Option<u64>,
    #[serde(default)]
    ends: Option<u64>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    tasks: Vec<NewTask>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobStatusParams {
    swarm_job: String,
    #[serde(default)]
    results_page: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    id: i64,
    task: NewTask,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobTasksParams {
    swarm_job: String,
    #[serde(default)]
    add: Vec<NewTask>,
    #[serde(default)]
    drop: Vec<i64>,
    #[serde(default)]
    replace: Vec<Replacement>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobSteerParams {
    swarm_job: String,
    note: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobCloseParams {
    swarm_job: String,
    status: String,
    #[serde(default)]
    result: Option<String>,
}

async fn rpc_inner(
    context: &Arc<BuyerContext>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let me = buyer_keys(&context.home)?.public_key().to_hex();
    let now = now_unix().max(0) as u64;
    let store = &context.store;
    match method {
        "swarm_job_status" => {
            let p: JobStatusParams = args(method, params)?;
            job_status(
                context,
                &me,
                &own_d(&p.swarm_job, SWARM_JOB_KIND, &me)?,
                p.results_page.unwrap_or(0),
            )
        }
        "swarm_job_tasks" => {
            let p: JobTasksParams = args(method, params)?;
            let d = own_d(&p.swarm_job, SWARM_JOB_KIND, &me)?;
            let job = store
                .swarm_job(&d)
                .map_err(es)?
                .ok_or_else(|| format!("no swarm job {d} on this buyer"))?;
            for t in p.add.iter().chain(p.replace.iter().map(|r| &r.task)) {
                check_task(t)?;
            }
            if !job.is_open() && (!p.add.is_empty() || !p.replace.is_empty()) {
                return Err(format!(
                    "swarm job {d} is {}; nothing was queued",
                    job.status
                ));
            }
            let mut refused = Vec::new();
            for id in &p.drop {
                if !store
                    .edit_swarm_task(&d, *id, None, now as i64)
                    .map_err(es)?
                {
                    refused.push(json!({"id": id, "reason": "not queued in this swarm job"}));
                }
            }
            for r in &p.replace {
                if !store
                    .edit_swarm_task(&d, r.id, Some(&r.task), now as i64)
                    .map_err(es)?
                {
                    refused.push(json!({"id": r.id, "reason": "not queued in this swarm job"}));
                }
            }
            let mut added = Vec::new();
            for t in &p.add {
                added.push(
                    store
                        .queue_swarm_task(&d, t, "agent", now as i64)
                        .map_err(es)?,
                );
            }
            let queue: Vec<TaskRow> = store
                .swarm_tasks(&d)
                .map_err(es)?
                .into_iter()
                .filter(|t| t.state == "queued")
                .collect();
            Ok(
                json!({"swarm_job": d, "added": added, "refused": refused, "queue": queue.iter().map(queued_view).collect::<Vec<_>>()}),
            )
        }
        "swarm_job_steer" => {
            let p: JobSteerParams = args(method, params)?;
            let d = own_d(&p.swarm_job, SWARM_JOB_KIND, &me)?;
            store
                .swarm_job(&d)
                .map_err(es)?
                .ok_or_else(|| format!("no swarm job {d} on this buyer"))?;
            if p.note.trim().is_empty() {
                return Err("note must not be empty".into());
            }
            let n = store.add_swarm_steer(&d, &p.note, now as i64).map_err(es)?;
            Ok(json!({"swarm_job": d, "notes": n, "note": format!("Stored. {NO_PLANNER}")}))
        }
        "swarm_job_close" => {
            let p: JobCloseParams = args(method, params)?;
            let d = own_d(&p.swarm_job, SWARM_JOB_KIND, &me)?;
            if !matches!(p.status.as_str(), "completed" | "cancelled") {
                return Err("status must be completed or cancelled".into());
            }
            let job = store
                .swarm_job(&d)
                .map_err(es)?
                .ok_or_else(|| format!("no swarm job {d} on this buyer"))?;
            if !job.is_open() {
                return Err(format!("swarm job {d} is already {}", job.status));
            }
            store
                .set_swarm_job_status(&d, &p.status, p.result.as_deref(), now as i64)
                .map_err(es)?;
            let dropped = store.drop_queued_swarm_tasks(&d, now as i64).map_err(es)?;
            let in_flight = store
                .swarm_tasks(&d)
                .map_err(es)?
                .iter()
                .filter(|t| t.state == "posted")
                .count();
            Ok(
                json!({"swarm_job": d, "status": p.status, "dropped_queued": dropped, "in_flight": in_flight,
                "note": "No new task is posted. In-flight tasks still settle and pay; the swarm job event and session closes follow within one engine tick."}),
            )
        }
        _ => rpc_relay(context, &me, method, params, now).await,
    }
}

fn queued_view(t: &TaskRow) -> Value {
    json!({"id": t.id, "text": t.text, "duration_secs": t.duration_secs, "max_sats": t.max_sats,
        "seller": t.seller_pin, "fresh": t.fresh, "workstream": t.workstream, "created_by": t.created_by,
        "note": t.outcome})
}

/// What the buyer's own award tables say about a one-off task, read when the engine has not copied
/// the award onto the task yet (it does so on its next tick, and only after a relay read succeeds).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LocalAward {
    /// The committed award (`awards` row).
    awarded_at: Option<u64>,
    /// A signed award is pinned and not refused (`award_attempts` pending or confirmed): a seller
    /// claimed and its price is reserved, but the award row is not written yet.
    awarding: bool,
}

fn local_award(store: &BuyerStore, t: &TaskRow) -> LocalAward {
    let session_turn = t.turn.is_some_and(|n| n >= 2);
    let (false, None, Some(job_id)) = (session_turn, t.awarded_at_unix, t.job_id.as_deref()) else {
        return LocalAward::default();
    };
    if let Ok(Some(award)) = store.award_record(job_id) {
        return LocalAward { awarded_at: u64::try_from(award.awarded_at_unix).ok(), awarding: false };
    }
    let awarding = matches!(
        store.award_attempt(job_id),
        Ok(Some(attempt)) if attempt.state != super::store::AttemptState::Refused
    );
    LocalAward { awarded_at: None, awarding }
}

/// Where a running task stands, so a check-in can tell a task no seller has claimed yet from one
/// that was awarded and is being worked on or settled (an award parked for lack of funds ends the
/// task as failed, with the reason). A one-off task's award comes from the task row, or from the
/// buyer store (`local`) before the engine has copied it over; a session turn needs no award.
fn running_view(t: &TaskRow, local: LocalAward, now: u64) -> Value {
    // The same test `settle_one` uses, so view and settlement never disagree on a malformed row.
    let session_turn = t.turn.is_some_and(|n| n >= 2) && t.session_job_id.is_some();
    let awarded_at = t.awarded_at_unix.or(local.awarded_at);
    let (stage, since) = match (session_turn, awarded_at) {
        (true, _) if t.followup_id.is_none() => ("turn_pending", t.posted_at_unix),
        (true, _) => ("turn_sent", t.posted_at_unix),
        (false, Some(at)) => ("awarded", Some(at)),
        (false, None) if t.job_id.is_none() => ("posting", t.posted_at_unix),
        (false, None) if local.awarding => ("awarding", t.posted_at_unix),
        (false, None) => ("awaiting_claim", t.posted_at_unix),
    };
    json!({"id": t.id, "seller": t.seller, "job_id": t.job_id.as_ref().or(t.session_job_id.as_ref()),
        "turn": t.turn.unwrap_or(1), "followup_id": t.followup_id, "price_sats": t.price_sats,
        "deadline_unix": t.deadline_unix, "awarded_at_unix": awarded_at, "stage": stage,
        "stage_secs": since.map(|at| now.saturating_sub(at)), "note": t.outcome})
}

/// The members snapshot of the last tick. Each fresh announcement gets its age AS OF THAT SNAPSHOT
/// (`announced_secs_ago`: a seat whose announcement was minutes old is likely to claim late or not
/// at all) and the snapshot's own age (`checked_secs_ago`). The snapshot stops updating once the
/// job is no longer running or the buyer cannot read the relay, so aging the announcement against
/// `now` would blame a healthy seller for the buyer's stale view.
fn members_view(members_json: &str, now: u64) -> Value {
    let mut members: Value = serde_json::from_str(members_json).unwrap_or(json!([]));
    for m in members.as_array_mut().into_iter().flatten() {
        let hb = &mut m["heartbeat"];
        let (Some(at), Some(checked)) =
            (hb["announced_at_unix"].as_u64(), hb["observed_at_unix"].as_u64())
        else {
            continue;
        };
        hb["announced_secs_ago"] = json!(checked.saturating_sub(at));
        hb["checked_secs_ago"] = json!(now.saturating_sub(checked));
    }
    members
}

fn job_status(context: &BuyerContext, me: &str, d: &str, page: usize) -> Result<Value, String> {
    let store = &context.store;
    let j = store
        .swarm_job(d)
        .map_err(es)?
        .ok_or_else(|| format!("no swarm job {d} on this buyer"))?;
    let tasks = store.swarm_tasks(d).map_err(es)?;
    let spent: u64 = tasks
        .iter()
        .filter(|t| t.state == "collected")
        .map(|t| t.price_sats.unwrap_or(0))
        .sum();
    let committed: u64 = tasks
        .iter()
        .filter(|t| holds_budget(t) && t.state != "collected")
        .map(|t| t.price_sats.unwrap_or(0))
        .sum();
    let queue: Vec<Value> = tasks
        .iter()
        .filter(|t| t.state == "queued")
        .map(queued_view)
        .collect();
    let now = now_unix().max(0) as u64;
    let running: Vec<Value> = tasks
        .iter()
        .filter(|t| t.state == "posted")
        .map(|t| running_view(t, local_award(store, t), now))
        .collect();
    let mut done: Vec<&TaskRow> = tasks
        .iter()
        .filter(|t| matches!(t.state.as_str(), "collected" | "failed"))
        .collect();
    done.sort_by_key(|t| std::cmp::Reverse((t.collected_at_unix.or(t.posted_at_unix), t.id)));
    let pages = done.len().div_ceil(RESULTS_PAGE).max(1);
    let results: Vec<Value> = done
        .iter()
        .skip(page * RESULTS_PAGE)
        .take(RESULTS_PAGE)
        .map(|t| json!({"id": t.id, "state": t.state, "seller": t.seller, "job_id": t.job_id.as_ref().or(t.session_job_id.as_ref()),
            "turn": t.turn.unwrap_or(1), "text": t.text, "outcome": t.outcome, "answer": t.answer,
            "price_sats": t.price_sats, "collected_at_unix": t.collected_at_unix}))
        .collect();
    let sessions = store.swarm_sessions(d).map_err(es)?;
    let members = members_view(&j.members_json, now);
    Ok(json!({
        "swarm_job": d,
        "address": format!("{SWARM_JOB_KIND}:{me}:{d}"),
        "naddr": naddr(&context.home, SWARM_JOB_KIND, me, d),
        "swarm": j.swarm,
        "title": j.title,
        "goal": j.goal,
        "status": j.status,
        "starts": j.starts,
        "ends": j.ends,
        "result": j.result,
        "budget": {"budget_sats": j.budget_sats, "committed_sats": committed, "spent_sats": spent,
            "headroom_sats": j.budget_sats.saturating_sub(committed + spent)},
        "members": members,
        "queue": queue,
        "running": running,
        "results": results,
        "results_page": page,
        "results_pages": pages,
        "sessions": sessions,
        "steering": j.steering,
        "planner": {"configured": false, "mode": "check-in", "note": NO_PLANNER},
        "idle_reason": j.idle_reason,
        "engine": if context.home.config.swarm.enabled { "on" } else { "off" },
    }))
}

async fn rpc_relay(
    context: &Arc<BuyerContext>,
    me: &str,
    method: &str,
    params: Value,
    now: u64,
) -> Result<Value, String> {
    if !METHODS.contains(&method) {
        return Err(format!("unknown swarm method {method}"));
    }
    let relay = SwarmRelay::connect(&context.home).await?;
    let result = rpc_with_relay(context, &relay, me, method, params, now).await;
    relay.disconnect().await;
    result
}

async fn rpc_with_relay(
    context: &Arc<BuyerContext>,
    relay: &SwarmRelay,
    me: &str,
    method: &str,
    params: Value,
    now: u64,
) -> Result<Value, String> {
    use nostr_sdk::prelude::{Filter, Kind, PublicKey};
    let home = &context.home;
    let store = &context.store;
    let me_pk = PublicKey::from_hex(me).map_err(es)?;
    let own = |kind: u16, d: &str| {
        Filter::new()
            .kind(Kind::from(kind))
            .author(me_pk)
            .identifier(d)
    };
    match method {
        "swarm_create" => {
            let p: CreateParams = args(method, params)?;
            let slug = p.slug.clone().unwrap_or_else(|| slugify(&p.name, 64));
            if !valid_slug(&slug) {
                return Err(format!(
                    "invalid slug {slug:?}: 1-64 of a-z, 0-9, -; pass slug"
                ));
            }
            if !latest_swarms(&relay.fetch(vec![own(SWARM_KIND, &slug)]).await?).is_empty() {
                return Err(format!(
                    "you already have a swarm {slug}; pick another slug. Nothing was published."
                ));
            }
            let s = Swarm {
                buyer: me.to_owned(),
                id: slug.clone(),
                name: p.name.trim().to_owned(),
                purpose: p.purpose,
                status: "open".into(),
                tasks: p.tasks.unwrap_or_else(|| "public".into()),
            };
            let draft = s.to_event_draft().map_err(es)?;
            let event_id = relay.publish(home, draft, now).await?;
            store
                .put_swarm(
                    &SwarmRow {
                        d: slug.clone(),
                        name: s.name.clone(),
                        purpose: s.purpose.clone(),
                        status: s.status.clone(),
                        tasks: s.tasks.clone(),
                    },
                    now as i64,
                )
                .map_err(es)?;
            Ok(
                json!({"swarm": s, "address": swarm_address(me, &slug), "naddr": naddr(home, SWARM_KIND, me, &slug), "event_id": event_id,
                "next": "Sellers join with `maxplayer swarm join <naddr> --minutes N`; accept them with swarm_decide, then create a swarm job."}),
            )
        }
        "swarm_list" => {
            let _: Empty = args(method, params)?;
            let events = relay
                .fetch(vec![
                    Filter::new()
                        .kinds([SWARM_KIND, SWARM_JOB_KIND, SWARM_DECISION_KIND].map(Kind::from))
                        .author(me_pk),
                ])
                .await?;
            let decisions = Decisions::read(me, &events);
            let trusted: Vec<PublicKey> = decisions
                .trusted(None)
                .iter()
                .filter_map(|k| PublicKey::from_hex(k).ok())
                .collect();
            let memberships = relay
                .fetch_by_authors(&trusted, || {
                    Filter::new()
                        .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
                        .pubkey(me_pk)
                })
                .await?;
            let jobs: Vec<SwarmJob> = latest_swarm_jobs(&events)
                .into_iter()
                .map(|(_, j)| j)
                .collect();
            let members: Vec<Membership> = latest_memberships(&memberships)
                .into_iter()
                .map(|(_, m)| m)
                .collect();
            let swarms: Vec<Value> = latest_swarms(&events)
                .into_iter()
                .map(|(_, s)| {
                    let count = members.iter().filter(|m| m.buyer == me && m.swarm == s.id && m.status == "joined" && decisions.allows(&m.seller, &s.id)).count();
                    let open: Vec<Value> = jobs.iter().filter(|j| j.swarm == s.id && j.is_open())
                        .map(|j| json!({"swarm_job": j.id, "title": j.title, "status": j.status, "starts": j.starts, "ends": j.ends})).collect();
                    json!({"swarm": s.id, "name": s.name, "purpose": s.purpose.lines().find(|l| !l.trim().is_empty()).unwrap_or(""),
                        "status": s.status, "tasks": s.tasks, "members": count, "open_jobs": open,
                        "address": swarm_address(me, &s.id), "naddr": naddr(home, SWARM_KIND, me, &s.id)})
                })
                .collect();
            Ok(json!({"buyer": me, "swarms": swarms}))
        }
        "swarm_members" => {
            let p: MembersParams = args(method, params)?;
            let swarm = p
                .swarm
                .as_deref()
                .map(|s| own_d(s, SWARM_KIND, me))
                .transpose()?;
            let decision_events = relay
                .fetch(vec![
                    Filter::new()
                        .kind(Kind::from(SWARM_DECISION_KIND))
                        .author(me_pk),
                ])
                .await?;
            let decisions = Decisions::read(me, &decision_events);
            let trusted: Vec<PublicKey> = decisions
                .trusted(swarm.as_deref())
                .iter()
                .filter_map(|k| PublicKey::from_hex(k).ok())
                .collect();
            let mut events = relay
                .fetch_by_authors(&trusted, || {
                    Filter::new()
                        .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
                        .pubkey(me_pk)
                })
                .await?;
            // Untrusted applicant sample, so the buyer can decide; never feeds eligibility.
            events.extend(
                relay
                    .fetch(vec![
                        Filter::new()
                            .kind(Kind::from(SWARM_MEMBERSHIP_KIND))
                            .pubkey(me_pk)
                            .limit(256),
                    ])
                    .await?,
            );
            let engine = Engine {
                store: store.clone(),
                buyer: me.to_owned(),
                sessions: false,
            };
            let mut used: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
            let rows: Vec<Value> = latest_memberships(&events)
                .into_iter()
                .filter(|(_, m)| m.buyer == me && swarm.as_deref().is_none_or(|s| s == m.swarm))
                .map(|(e, m)| {
                    let u = used.entry(m.swarm.clone()).or_insert_with(|| engine.used_secs(&[], &m.swarm, now).unwrap_or_default());
                    let secs = u.get(&m.seller).copied().unwrap_or(0);
                    json!({"swarm": m.swarm, "seller": m.seller, "status": m.status, "minutes": m.minutes,
                        "minutes_used_today": secs as f64 / 60.0, "since": m.since, "joined_at": e.created_at.as_secs(), "note": m.note,
                        "decision": {"buyer_wide": decisions.buyer_wide(&m.seller), "swarm": decisions.for_swarm(&m.seller, &m.swarm)},
                        "accepted": decisions.allows(&m.seller, &m.swarm)})
                })
                .collect();
            Ok(json!({"buyer": me, "members": rows}))
        }
        "swarm_decide" => {
            let p: DecideParams = args(method, params)?;
            let seller = if p.seller.starts_with("npub1") {
                use nostr_sdk::prelude::FromBech32;
                PublicKey::from_bech32(&p.seller)
                    .map_err(|_| "invalid seller npub")?
                    .to_hex()
            } else if valid_hex(&p.seller) {
                p.seller.clone()
            } else {
                return Err("seller must be an npub or 64-hex key".into());
            };
            let swarm = match p.swarm.as_deref() {
                Some(s) => {
                    let d = own_d(s, SWARM_KIND, me)?;
                    if latest_swarms(&relay.fetch(vec![own(SWARM_KIND, &d)]).await?).is_empty() {
                        return Err(format!("no swarm {d} of yours on the relay"));
                    }
                    Some(d)
                }
                None => None,
            };
            let decision = Decision {
                buyer: me.to_owned(),
                seller: seller.clone(),
                swarm: swarm.clone(),
                decision: if p.accept { "accepted" } else { "denied" }.into(),
                membership_event_id: None,
                reason: String::new(),
            };
            let d_tag = match &swarm {
                Some(d) => format!("{seller}:{d}"),
                None => seller.clone(),
            };
            let stored = relay.fetch(vec![own(SWARM_DECISION_KIND, &d_tag)]).await?;
            let created = replacement_timestamp(now, &stored, me, SWARM_DECISION_KIND, &d_tag)?;
            let event_id = relay
                .publish(home, decision.to_event_draft().map_err(es)?, created)
                .await?;
            Ok(json!({"decision": decision, "event_id": event_id,
                "scope": if swarm.is_some() { "this swarm" } else { "all your swarms" }}))
        }
        "swarm_job_create" => {
            let p: JobCreateParams = args(method, params)?;
            let swarm_d = own_d(&p.swarm, SWARM_KIND, me)?;
            let s = latest_swarms(&relay.fetch(vec![own(SWARM_KIND, &swarm_d)]).await?)
                .into_iter()
                .map(|(_, s)| s)
                .next()
                .ok_or_else(|| {
                    format!("no swarm {swarm_d} of yours on the relay; create it with swarm_create")
                })?;
            if s.status != "open" {
                return Err(format!("swarm {swarm_d} is closed; nothing was published"));
            }
            if p.goal.trim().is_empty() {
                return Err("goal must not be empty".into());
            }
            for t in &p.tasks {
                check_task(t)?;
            }
            let starts = p.starts.unwrap_or(now);
            let ends = p.ends.unwrap_or(starts + swarm_minutes::DAY_SECS);
            let title = p
                .title
                .clone()
                .map(|t| t.trim().to_owned())
                .unwrap_or_else(|| {
                    p.goal
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("")
                        .trim_start_matches('#')
                        .trim()
                        .chars()
                        .take(80)
                        .collect()
                });
            let slug = p.slug.clone().unwrap_or_else(|| {
                format!("{}-{starts}", slugify(&title, 48))
                    .trim_start_matches('-')
                    .to_owned()
            });
            if !valid_slug(&slug) {
                return Err(format!(
                    "invalid slug {slug:?}: 1-64 of a-z, 0-9, -; pass slug"
                ));
            }
            if store.swarm_job(&slug).map_err(es)?.is_some()
                || !latest_swarm_jobs(&relay.fetch(vec![own(SWARM_JOB_KIND, &slug)]).await?)
                    .is_empty()
            {
                return Err(format!(
                    "you already have a swarm job {slug}; pick another slug. Nothing was published."
                ));
            }
            let job = SwarmJob {
                buyer: me.to_owned(),
                swarm: swarm_d.clone(),
                id: slug.clone(),
                title,
                goal: p.goal,
                starts,
                ends,
                budget_sats: p.budget_sats,
                status: "scheduled".into(),
                update: None,
                result: None,
            };
            let draft = job.to_event_draft().map_err(es)?;
            let event_id = relay.publish(home, draft, now).await?;
            let row = SwarmJobRow {
                d: slug.clone(),
                swarm: swarm_d,
                title: job.title.clone(),
                goal: job.goal.clone(),
                starts,
                ends,
                budget_sats: job.budget_sats,
                status: "scheduled".into(),
                result: None,
                steering: Vec::new(),
                idle_reason: None,
                members_json: "[]".into(),
                published_status: Some("scheduled".into()),
                published_update: None,
                last_update_unix: now,
            };
            store.insert_swarm_job(&row, now as i64).map_err(es)?;
            let queued: Vec<i64> = p
                .tasks
                .iter()
                .map(|t| store.queue_swarm_task(&slug, t, "agent", now as i64))
                .collect::<Result<_, _>>()
                .map_err(es)?;
            Ok(
                json!({"swarm_job": job, "address": format!("{SWARM_JOB_KIND}:{me}:{slug}"), "naddr": naddr(home, SWARM_JOB_KIND, me, &slug),
                "event_id": event_id, "queued": queued, "planner": {"configured": false, "mode": "check-in"}, "note": NO_PLANNER}),
            )
        }
        other => Err(format!("unknown swarm method {other}")),
    }
}

#[cfg(test)]
#[path = "swarm_tests.rs"]
mod tests;
