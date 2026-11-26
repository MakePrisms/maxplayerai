//! Swarm engine state (docs/specs/swarm.md §7.1, buyer store v10): swarms, swarm jobs, their task
//! queues and the session per seller per swarm job. Additive tables; they stay empty while
//! `[swarm] enabled = false`. Replaces the runner's journal file and lock.
use super::*;
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS swarms (
    d               TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    purpose         TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('open','closed')),
    tasks           TEXT NOT NULL CHECK (tasks IN ('public','private')),
    updated_at_unix INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS swarm_jobs (
    d                TEXT PRIMARY KEY,
    swarm_d          TEXT NOT NULL,
    title            TEXT NOT NULL,
    goal             TEXT NOT NULL,
    starts           INTEGER NOT NULL,
    ends             INTEGER NOT NULL,
    budget_sats      INTEGER NOT NULL CHECK (budget_sats >= 0),
    status           TEXT NOT NULL CHECK (status IN ('scheduled','in_progress','completed','cancelled')),
    -- Planner state (PR 5); written by nothing yet.
    plan_json        TEXT,
    planner_json     TEXT,
    fingerprint      TEXT,
    result           TEXT,
    steering_json    TEXT NOT NULL DEFAULT '[]',
    idle_reason      TEXT,
    -- Members with minutes as of the last tick (swarm_job_status reads it).
    members_json     TEXT NOT NULL DEFAULT '[]',
    -- The status last published in a 30341, and when the last update went out.
    published_status TEXT,
    published_update TEXT,
    last_update_unix INTEGER NOT NULL DEFAULT 0,
    created_at_unix  INTEGER NOT NULL,
    updated_at_unix  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS swarm_tasks (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    swarm_job_d       TEXT NOT NULL,
    workstream        TEXT,
    text              TEXT NOT NULL,
    duration_secs     INTEGER NOT NULL,
    max_sats          INTEGER NOT NULL CHECK (max_sats >= 0),
    seller_pin        TEXT,
    state             TEXT NOT NULL CHECK (state IN ('queued','posted','collected','failed','dropped')),
    seller            TEXT,
    price_sats        INTEGER,
    job_id            TEXT,
    deadline_unix     INTEGER,
    posted_at_unix    INTEGER,
    -- When the seller was awarded turn 1 (minutes count from here; turns count from posted_at).
    awarded_at_unix   INTEGER,
    collected_at_unix INTEGER,
    outcome           TEXT,
    answer            TEXT,
    quotes_json       TEXT NOT NULL DEFAULT '[]',
    created_by        TEXT NOT NULL CHECK (created_by IN ('agent','planner')),
    fresh             INTEGER NOT NULL DEFAULT 0,
    -- A session turn: the session's turn-1 job id and this task's turn number (>= 2).
    session_job_id    TEXT,
    turn              INTEGER,
    followup_id       TEXT,
    -- The exact post_job arguments (one-off tasks), so a retry is the identical request.
    params_json       TEXT,
    -- 1 once the task's funds are known not to be held or spent (never awarded, released).
    released          INTEGER NOT NULL DEFAULT 0,
    created_at_unix   INTEGER NOT NULL,
    updated_at_unix   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS swarm_sessions (
    swarm_job_d     TEXT NOT NULL,
    seller          TEXT NOT NULL,
    job_id          TEXT NOT NULL,
    last_turn       INTEGER NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('open','context_reset','dead','closed')),
    updated_at_unix INTEGER NOT NULL,
    PRIMARY KEY (swarm_job_d, seller)
);";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwarmRow {
    pub d: String,
    pub name: String,
    pub purpose: String,
    pub status: String,
    pub tasks: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwarmJobRow {
    pub d: String,
    pub swarm: String,
    pub title: String,
    pub goal: String,
    pub starts: u64,
    pub ends: u64,
    pub budget_sats: u64,
    pub status: String,
    pub result: Option<String>,
    pub steering: Vec<String>,
    pub idle_reason: Option<String>,
    pub members_json: String,
    pub published_status: Option<String>,
    pub published_update: Option<String>,
    pub last_update_unix: u64,
}
impl SwarmJobRow {
    pub fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "scheduled" | "in_progress")
    }
}

/// A task as the agent (or later the planner) writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTask {
    pub text: String,
    pub duration_secs: u64,
    pub max_sats: u64,
    /// Pin the task to one seller (hex).
    #[serde(default)]
    pub seller: Option<String>,
    /// A new agent with no history: never sent as a session turn.
    #[serde(default)]
    pub fresh: bool,
    #[serde(default)]
    pub workstream: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskRow {
    pub id: i64,
    pub job: String,
    pub workstream: Option<String>,
    pub text: String,
    pub duration_secs: u64,
    pub max_sats: u64,
    pub seller_pin: Option<String>,
    pub state: String,
    pub seller: Option<String>,
    pub price_sats: Option<u64>,
    pub job_id: Option<String>,
    pub deadline_unix: Option<u64>,
    pub posted_at_unix: Option<u64>,
    pub awarded_at_unix: Option<u64>,
    pub collected_at_unix: Option<u64>,
    pub outcome: Option<String>,
    pub answer: Option<String>,
    pub created_by: String,
    pub fresh: bool,
    pub session_job_id: Option<String>,
    pub turn: Option<u32>,
    pub followup_id: Option<String>,
    #[serde(skip)]
    pub params_json: Option<String>,
    pub released: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwarmSessionRow {
    pub job: String,
    pub seller: String,
    pub job_id: String,
    pub last_turn: u32,
    pub state: String,
}

const JOB_COLUMNS: &str = "d, swarm_d, title, goal, starts, ends, budget_sats, status, result, \
     steering_json, idle_reason, members_json, published_status, last_update_unix, published_update";
const TASK_COLUMNS: &str = "id, swarm_job_d, workstream, text, duration_secs, max_sats, seller_pin, \
     state, seller, price_sats, job_id, deadline_unix, posted_at_unix, collected_at_unix, outcome, \
     answer, created_by, fresh, session_job_id, turn, followup_id, params_json, released, \
     awarded_at_unix";

fn u(v: Option<i64>) -> Option<u64> {
    v.map(|v| v.max(0) as u64)
}

fn job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SwarmJobRow> {
    let steering: String = r.get(9)?;
    Ok(SwarmJobRow {
        d: r.get(0)?,
        swarm: r.get(1)?,
        title: r.get(2)?,
        goal: r.get(3)?,
        starts: r.get::<_, i64>(4)? as u64,
        ends: r.get::<_, i64>(5)? as u64,
        budget_sats: r.get::<_, i64>(6)? as u64,
        status: r.get(7)?,
        result: r.get(8)?,
        steering: serde_json::from_str(&steering).unwrap_or_default(),
        idle_reason: r.get(10)?,
        members_json: r.get(11)?,
        published_status: r.get(12)?,
        last_update_unix: r.get::<_, i64>(13)? as u64,
        published_update: r.get(14)?,
    })
}

fn task_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: r.get(0)?,
        job: r.get(1)?,
        workstream: r.get(2)?,
        text: r.get(3)?,
        duration_secs: r.get::<_, i64>(4)? as u64,
        max_sats: r.get::<_, i64>(5)? as u64,
        seller_pin: r.get(6)?,
        state: r.get(7)?,
        seller: r.get(8)?,
        price_sats: u(r.get(9)?),
        job_id: r.get(10)?,
        deadline_unix: u(r.get(11)?),
        posted_at_unix: u(r.get(12)?),
        collected_at_unix: u(r.get(13)?),
        outcome: r.get(14)?,
        answer: r.get(15)?,
        created_by: r.get(16)?,
        fresh: r.get::<_, i64>(17)? != 0,
        session_job_id: r.get(18)?,
        turn: r.get::<_, Option<i64>>(19)?.map(|t| t as u32),
        followup_id: r.get(20)?,
        params_json: r.get(21)?,
        released: r.get::<_, i64>(22)? != 0,
        awarded_at_unix: u(r.get(23)?),
    })
}

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SwarmSessionRow> {
    Ok(SwarmSessionRow {
        job: r.get(0)?,
        seller: r.get(1)?,
        job_id: r.get(2)?,
        last_turn: r.get::<_, i64>(3)? as u32,
        state: r.get(4)?,
    })
}

impl BuyerStore {
    pub fn put_swarm(&self, s: &SwarmRow, now_unix: i64) -> Result<(), StoreError> {
        self.lock()?.execute(
            "INSERT INTO swarms (d, name, purpose, status, tasks, updated_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(d) DO UPDATE SET name = ?2, purpose = ?3, status = ?4, tasks = ?5,
                 updated_at_unix = ?6",
            params![s.d, s.name, s.purpose, s.status, s.tasks, now_unix],
        )?;
        Ok(())
    }

    pub fn swarm_row(&self, d: &str) -> Result<Option<SwarmRow>, StoreError> {
        Ok(self
            .lock()?
            .query_row(
                "SELECT d, name, purpose, status, tasks FROM swarms WHERE d = ?1",
                [d],
                |r| {
                    Ok(SwarmRow {
                        d: r.get(0)?,
                        name: r.get(1)?,
                        purpose: r.get(2)?,
                        status: r.get(3)?,
                        tasks: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Insert a new swarm job; `false` when one with this `d` already exists (nothing changed).
    pub fn insert_swarm_job(&self, j: &SwarmJobRow, now_unix: i64) -> Result<bool, StoreError> {
        let n = self.lock()?.execute(
            "INSERT OR IGNORE INTO swarm_jobs (d, swarm_d, title, goal, starts, ends, budget_sats,
                 status, result, published_status, last_update_unix, created_at_unix,
                 updated_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![
                j.d,
                j.swarm,
                j.title,
                j.goal,
                j.starts as i64,
                j.ends as i64,
                j.budget_sats as i64,
                j.status,
                j.result,
                j.published_status,
                j.last_update_unix as i64,
                now_unix
            ],
        )?;
        Ok(n == 1)
    }

    pub fn swarm_job(&self, d: &str) -> Result<Option<SwarmJobRow>, StoreError> {
        Ok(self
            .lock()?
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM swarm_jobs WHERE d = ?1"),
                [d],
                job_row,
            )
            .optional()?)
    }

    /// Swarm jobs, oldest first: of one swarm or all, open only or all.
    pub fn swarm_jobs(
        &self,
        swarm: Option<&str>,
        open_only: bool,
    ) -> Result<Vec<SwarmJobRow>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM swarm_jobs
             WHERE (?1 IS NULL OR swarm_d = ?1)
               AND (?2 = 0 OR status IN ('scheduled','in_progress'))
             ORDER BY created_at_unix, d"
        ))?;
        let rows = stmt.query_map(params![swarm, open_only as i64], job_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_swarm_job_status(
        &self,
        d: &str,
        status: &str,
        result: Option<&str>,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            "UPDATE swarm_jobs SET status = ?2, result = COALESCE(?3, result), updated_at_unix = ?4
             WHERE d = ?1",
            params![d, status, result, now_unix],
        )?;
        Ok(())
    }

    pub fn set_swarm_job_tick(
        &self,
        d: &str,
        idle_reason: Option<&str>,
        members_json: Option<&str>,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            "UPDATE swarm_jobs SET idle_reason = ?2, members_json = COALESCE(?3, members_json),
                 updated_at_unix = ?4
             WHERE d = ?1",
            params![d, idle_reason, members_json, now_unix],
        )?;
        Ok(())
    }

    pub fn set_swarm_job_published(
        &self,
        d: &str,
        status: &str,
        update: Option<&str>,
        at_unix: u64,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            "UPDATE swarm_jobs SET published_status = ?2, published_update = ?3, last_update_unix = ?4
             WHERE d = ?1",
            params![d, status, update, at_unix as i64],
        )?;
        Ok(())
    }

    /// Append a steering note; the number of notes now stored.
    pub fn add_swarm_steer(&self, d: &str, note: &str, now_unix: i64) -> Result<usize, StoreError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw: String = tx.query_row(
            "SELECT steering_json FROM swarm_jobs WHERE d = ?1",
            [d],
            |r| r.get(0),
        )?;
        let mut notes: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
        notes.push(note.to_owned());
        tx.execute(
            "UPDATE swarm_jobs SET steering_json = ?2, updated_at_unix = ?3 WHERE d = ?1",
            params![d, serde_json::to_string(&notes).expect("strings"), now_unix],
        )?;
        tx.commit()?;
        Ok(notes.len())
    }

    pub fn queue_swarm_task(
        &self,
        job: &str,
        t: &NewTask,
        created_by: &str,
        now_unix: i64,
    ) -> Result<i64, StoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO swarm_tasks (swarm_job_d, workstream, text, duration_secs, max_sats,
                 seller_pin, state, created_by, fresh, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?8, ?9, ?9)",
            params![
                job,
                t.workstream,
                t.text,
                t.duration_secs as i64,
                t.max_sats as i64,
                t.seller,
                created_by,
                t.fresh as i64,
                now_unix
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Tasks the engine still has to settle: `posted`, or `failed` with funds possibly held (a late
    /// delivery still settles and is recorded).
    pub fn swarm_tasks_live(&self) -> Result<Vec<TaskRow>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM swarm_tasks
             WHERE state = 'posted'
                OR (state = 'failed' AND released = 0
                    AND (job_id IS NOT NULL OR followup_id IS NOT NULL))
             ORDER BY id"
        ))?;
        let rows = stmt.query_map([], task_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn swarm_task(&self, id: i64) -> Result<Option<TaskRow>, StoreError> {
        Ok(self
            .lock()?
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM swarm_tasks WHERE id = ?1"),
                [id],
                task_row,
            )
            .optional()?)
    }

    /// One swarm job's tasks in queue order.
    pub fn swarm_tasks(&self, job: &str) -> Result<Vec<TaskRow>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM swarm_tasks WHERE swarm_job_d = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([job], task_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every task of every swarm job of one swarm.
    pub fn swarm_tasks_of_swarm(&self, swarm: &str) -> Result<Vec<TaskRow>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM swarm_tasks
             WHERE swarm_job_d IN (SELECT d FROM swarm_jobs WHERE swarm_d = ?1) ORDER BY id"
        ))?;
        let rows = stmt.query_map([swarm], task_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// `queued → posted` for `seller` at `price`, recorded BEFORE anything is posted so a crash
    /// leaves a task the next tick retries with the identical request. `false` when the task is
    /// no longer queued (dropped or replaced meanwhile): nothing was changed.
    #[allow(clippy::too_many_arguments)]
    pub fn claim_swarm_task(
        &self,
        id: i64,
        seller: &str,
        price_sats: u64,
        deadline_unix: u64,
        now_unix: u64,
        params_json: Option<&str>,
        session: Option<(&str, u32)>,
    ) -> Result<bool, StoreError> {
        let n = self.lock()?.execute(
            "UPDATE swarm_tasks SET state = 'posted', seller = ?2, price_sats = ?3,
                 deadline_unix = ?4, posted_at_unix = ?5, params_json = ?6, session_job_id = ?7,
                 turn = ?8, job_id = NULL, followup_id = NULL, outcome = NULL, released = 0,
                 awarded_at_unix = NULL,
                 updated_at_unix = ?5
             WHERE id = ?1 AND state = 'queued'",
            params![
                id,
                seller,
                price_sats as i64,
                deadline_unix as i64,
                now_unix as i64,
                params_json,
                session.map(|s| s.0),
                session.map(|s| s.1 as i64)
            ],
        )?;
        Ok(n == 1)
    }

    pub fn set_swarm_task_job(
        &self,
        id: i64,
        job_id: &str,
        now_unix: u64,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            // `posted_at_unix` moves to the instant the offer went out: claiming the task stamped it
            // before the post, and retries in between are not time a seller had to claim it.
            "UPDATE swarm_tasks SET job_id = ?2, outcome = NULL, posted_at_unix = ?3,
                 updated_at_unix = ?3 WHERE id = ?1",
            params![id, job_id, now_unix as i64],
        )?;
        Ok(())
    }

    pub fn set_swarm_task_followup(
        &self,
        id: i64,
        followup_id: &str,
        turn: u32,
        now_unix: u64,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            "UPDATE swarm_tasks SET followup_id = ?2, turn = ?3, outcome = NULL, updated_at_unix = ?4
             WHERE id = ?1",
            params![id, followup_id, turn as i64, now_unix as i64],
        )?;
        Ok(())
    }

    pub fn set_swarm_task_awarded(&self, id: i64, at_unix: u64) -> Result<(), StoreError> {
        self.lock()?.execute(
            "UPDATE swarm_tasks SET awarded_at_unix = ?2 WHERE id = ?1 AND awarded_at_unix IS NULL",
            params![id, at_unix as i64],
        )?;
        Ok(())
    }

    /// `posted → queued`: the attempt published nothing (a refused reply).
    pub fn requeue_swarm_task(&self, id: i64, now_unix: u64) -> Result<(), StoreError> {
        self.lock()?.execute(
            "UPDATE swarm_tasks SET state = 'queued', seller = NULL, price_sats = NULL,
                 deadline_unix = NULL, posted_at_unix = NULL, params_json = NULL,
                 session_job_id = NULL, turn = NULL, followup_id = NULL, job_id = NULL,
                 updated_at_unix = ?2
             WHERE id = ?1 AND state = 'posted'",
            params![id, now_unix as i64],
        )?;
        Ok(())
    }

    /// Close a posted (or failed, for a late delivery) task as `collected` or `failed`.
    pub fn finish_swarm_task(
        &self,
        id: i64,
        state: &str,
        outcome: &str,
        answer: Option<&str>,
        released: bool,
        now_unix: u64,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            "UPDATE swarm_tasks SET state = ?2, outcome = ?3, answer = COALESCE(?4, answer),
                 released = ?5,
                 collected_at_unix = CASE WHEN ?2 = 'collected' THEN ?6 ELSE collected_at_unix END,
                 updated_at_unix = ?6
             WHERE id = ?1 AND state IN ('posted','failed')",
            params![id, state, outcome, answer, released as i64, now_unix as i64],
        )?;
        Ok(())
    }

    pub fn set_swarm_task_outcome(
        &self,
        id: i64,
        outcome: &str,
        released: bool,
        now_unix: u64,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            "UPDATE swarm_tasks SET outcome = ?2, released = ?3, updated_at_unix = ?4 WHERE id = ?1",
            params![id, outcome, released as i64, now_unix as i64],
        )?;
        Ok(())
    }

    /// Replace (Some) or drop (None) a QUEUED task of `job`; `false` when it is not queued there.
    pub fn edit_swarm_task(
        &self,
        job: &str,
        id: i64,
        t: Option<&NewTask>,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        let conn = self.lock()?;
        let n = match t {
            None => conn.execute(
                "UPDATE swarm_tasks SET state = 'dropped', updated_at_unix = ?3
                 WHERE id = ?1 AND swarm_job_d = ?2 AND state = 'queued'",
                params![id, job, now_unix],
            )?,
            Some(t) => conn.execute(
                "UPDATE swarm_tasks SET workstream = ?3, text = ?4, duration_secs = ?5,
                     max_sats = ?6, seller_pin = ?7, fresh = ?8, updated_at_unix = ?9
                 WHERE id = ?1 AND swarm_job_d = ?2 AND state = 'queued'",
                params![
                    id,
                    job,
                    t.workstream,
                    t.text,
                    t.duration_secs as i64,
                    t.max_sats as i64,
                    t.seller,
                    t.fresh as i64,
                    now_unix
                ],
            )?,
        };
        Ok(n == 1)
    }

    pub fn drop_queued_swarm_tasks(&self, job: &str, now_unix: i64) -> Result<usize, StoreError> {
        Ok(self.lock()?.execute(
            "UPDATE swarm_tasks SET state = 'dropped', updated_at_unix = ?2
             WHERE swarm_job_d = ?1 AND state = 'queued'",
            params![job, now_unix],
        )?)
    }

    pub fn swarm_session(
        &self,
        job: &str,
        seller: &str,
    ) -> Result<Option<SwarmSessionRow>, StoreError> {
        Ok(self
            .lock()?
            .query_row(
                "SELECT swarm_job_d, seller, job_id, last_turn, state FROM swarm_sessions
                 WHERE swarm_job_d = ?1 AND seller = ?2",
                [job, seller],
                session_row,
            )
            .optional()?)
    }

    pub fn swarm_sessions(&self, job: &str) -> Result<Vec<SwarmSessionRow>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT swarm_job_d, seller, job_id, last_turn, state FROM swarm_sessions
             WHERE swarm_job_d = ?1 ORDER BY seller",
        )?;
        let rows = stmt.query_map([job], session_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Open sessions whose swarm job is no longer open: the engine closes them (§7.6).
    pub fn swarm_sessions_to_close(&self) -> Result<Vec<SwarmSessionRow>, StoreError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT s.swarm_job_d, s.seller, s.job_id, s.last_turn, s.state FROM swarm_sessions s
             JOIN swarm_jobs j ON j.d = s.swarm_job_d
             WHERE s.state IN ('open','context_reset') AND j.status IN ('completed','cancelled')
             ORDER BY s.swarm_job_d, s.seller",
        )?;
        let rows = stmt.query_map([], session_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn swarm_rows(&self) -> Result<Vec<SwarmRow>, StoreError> {
        let conn = self.lock()?;
        let mut stmt =
            conn.prepare("SELECT d, name, purpose, status, tasks FROM swarms ORDER BY d")?;
        let rows = stmt.query_map([], |r| {
            Ok(SwarmRow {
                d: r.get(0)?,
                name: r.get(1)?,
                purpose: r.get(2)?,
                status: r.get(3)?,
                tasks: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn put_swarm_session(
        &self,
        job: &str,
        seller: &str,
        job_id: &str,
        last_turn: u32,
        state: &str,
        now_unix: u64,
    ) -> Result<(), StoreError> {
        self.lock()?.execute(
            "INSERT INTO swarm_sessions (swarm_job_d, seller, job_id, last_turn, state, updated_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(swarm_job_d, seller) DO UPDATE SET job_id = ?3, last_turn = ?4,
                 state = ?5, updated_at_unix = ?6",
            params![job, seller, job_id, last_turn as i64, state, now_unix as i64],
        )?;
        Ok(())
    }
}
