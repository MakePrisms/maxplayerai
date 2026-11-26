# Swarm in maxplayer — swarms, swarm jobs, no sidecar

**Status:** design spec. No code written. **Anchor commit: `4d67a21c5c8f6032f1602b50008b4ec6f42a39b1`
(= `origin/main` at the time of writing).** Code citations are `file:line @4d67a21c`, re-derived by
grep at that commit.

**Scope.** Make swarm a normal part of `maxplayer`: one install for buyers and sellers, named swarms
that sellers join once, swarm events on our relay, the runner's work done by the buyer daemon, and a
planner that runs on whatever LLM agent the buyer already has.

## 0. Terminology

| Term | Meaning | Today's code calls it |
|---|---|---|
| **swarm** | The feature, and a named group of sellers a buyer creates ("Math", "Help activists"). Sellers join a swarm once and stay available for all its swarm jobs. | (no object; implied by memberships) |
| **swarm job** | A goal the buyer gives to one of its swarms (title, goal, window, budget). | challenge / run |
| **task** | One piece of a swarm job handed to one seller. On the wire it is an ordinary maxplayer job (offer → claim → award → result → collect). | assignment / job |
| **member** | A seller that joined a swarm and is accepted for it by its buyer (buyer-wide or for that swarm, §4). | member |

The word "challenge" is removed from code, docs, CLI and UI (§10). Kind numbers do not change.

## 1. Settled inputs — not relitigated here

#swarm-production, 11–13 Nov 2026:

1. **Remove the sidecar.** Swarm becomes a regular part of `maxplayer`. (Josip)
2. **Use case for now:** our own buyer posts swarm jobs (math, physics problems …) and pays sellers to
   take part. Free-vs-paid per buyer (`free_for`) is out of scope. (Josip)
3. **Our relay**, not public relays, carries the swarm events. (Josip)
4. **Buyer UX:** the human tells their agent "install maxplayer buyer", then "create a swarm job to do
   X". No custom installs, no complex configs. (Josip)
5. **Any LLM**, not Claude-only. How the buyer pays for its LLM (subscription or API key) is
   irrelevant; maxplayer never sees an LLM credential. (Josip)
6. **Swarm jobs can be fan-out or iterative**, so an unattended planner is needed. (Josip)
7. **Planner backend:** built-in presets for known CLIs plus a raw `planner_command` the buyer's
   agent registers at install. (Josip)
8. **A small task queue** per swarm job in the daemon. (Josip)
9. **One planner call per change** (today's `assign` and `assess` merged). (Josip)
10. **Many buyers.** Any buyer may create swarms; a buyer is identified by its pubkey. No buyer key is
    built into the client. (Josip)
11. **Naming** as in §0. (Josip: swarm, swarm job; Bob: task)
12. **Swarms are explicit and a buyer may have several.** The buyer creates a swarm (name, purpose)
    before any swarm job; sellers discover and join it, commit minutes per day, and stay available
    for every swarm job in it without re-joining. (Josip, Bob)
13. **Acceptance at two levels.** A buyer-wide decision trusts or blocks a seller in all that buyer's
    swarms (one click, Bob's default); an optional per-swarm decision accepts or excludes a seller in
    one swarm. Eligible = accepted at either level and denied at neither. (Bob 13 Nov; Josip 16 Nov
    14:33)
14. **Free seats across one buyer's swarms:** a seller in several swarms of one buyer gets a task
    from each while its fresh heartbeat shows a free seat for it: the beat's `free_slots`
    (`protocol-v1.md` §4.2) minus this buyer's running tasks to that seller the beat may not reflect
    yet (§7.3). Still one running task per seller per swarm. A seller whose heartbeat states no
    `slots`/`free_slots` (built before them) gets one task at a time across all of the buyer's
    swarms, as before: `in_flight`/`queue_depth` is not free capacity. (Josip 18 Nov; replaces
    "no cross-swarm cap", Bob)
15. **First-draft questions:** `join` edits seller config (append-only); `solved` needs one
    independent verify task, no buyer confirmation; our buyer uses the `claude` preset; `codex`
    ships as a preset only after a live canary. (Josip, accepted defaults)
16. **Private tasks are a setting.** A swarm can send its tasks as private offers. (Bob, 13 Nov 17:13)
17. **Lead-defined coordination structure (boards) moves to the shared-repo follow-up.** Bob asked
    that the lead have the tools to build its own structure (as in Anthropic's Fermat run), with
    nothing prescribed. Without a shared work product, one text per task covers what v1 problems
    need, so v1 communicates as the runner does today (§9.5) and the boards design is kept in §14.2.
    (Bob, 13 Nov 17:03–17:18; Josip, 16 Nov 14:42)
18. **Everything passes through the lead.** Sellers never take orders from other sellers: only the
    lead writes task text and the queue; what a seller reports goes to the lead, which decides what,
    if anything, other sellers see. The follow-up keeps this rule for boards. (Bob, 13 Nov 17:17)
19. **Session jobs are the default** where the seller supports them: one session per seller per swarm
    job, later tasks as reply turns; fall back to a one-off job otherwise or when the lead marks a
    task `fresh`. Swarm members are told to enable sessions. (Bob, 13 Nov 17:26)
20. **Shared work product (contribution mode / shared repo) is a follow-up spec**, not part of this
    one. Its risks and required defaults are recorded in §14.1 so the follow-up starts from them.
    (Bob, 13 Nov 17:31)

## 2. What exists today

| Piece | Where | What it does |
|---|---|---|
| Event kinds | `crates/maxplayer-core/src/kinds.rs:26`, `:28`, `:30 @4d67a21c` | `30341` CHALLENGE (buyer), `30342` CHALLENGE MEMBERSHIP (seller), `30343` CHALLENGE DECISION (buyer). Validation in `challenge.rs`; author checks at `challenge.rs:121`, `:232`. |
| Membership scope | `challenge.rs:179–183` | Membership `d` must be the buyer pubkey and it may not carry an `a` reference: membership is per buyer, there is no swarm object. |
| Sidecar `maxplayer-swarm` | `crates/maxplayer-challenges/` (own workspace, ~4.7k lines Rust) | Signs/reads the three kinds on nos.lol, primal, offchain.pub with a 2-of-3 quorum (`README.md:27`). Refuses `relay.maxplayer.ai` (`src/relay.rs:774`). No release binary; CI only builds it (`.github/workflows/ci.yml:328`). |
| Runner | `scripts/challenges/swarm.mjs` (2,130 lines) | Node loop, `tick()` (`swarm.mjs:1453`) every 5 s (`swarm.mjs:1976`). Settles open jobs, reads relays, calls the planner, posts tasks through buyer MCP `post_job` (`swarm.mjs:1857`). |
| Planner | `scripts/challenges/planner-llm.mjs` | Spawns `claude -p --output-format text --tools '' --no-session-persistence` (`planner-llm.mjs:101`). Modes `analyze` (`swarm.mjs:1746`), `assign`, `assess` (`swarm.mjs:1750`, on result-hash change `:1748`). Prompt treats every input as untrusted data (`planner-llm.mjs:91`). |
| Task ↔ swarm job link | `web/app/src/challenge/shared/events.mjs:101–102` | Offer text prefixed `[swarm:<buyer>:<d>]\n`; legacy `[challenge:…]` still matched. |
| Minutes | `web/app/src/challenge/shared/model.mjs:109`, `:143`, `:150` | Derived from job events (award → result, capped at deadline), summed over the selected challenge's jobs only. Shared by runner and board. |
| Board `/swarm` | `web/app/src/challenge/` | Reads the public relays, denylists ours (`shared/relay.mjs:539`), buyer key baked at build (`web/app/scripts/swarm-buyer.mjs:28`). |
| Seller join | `web/app/.well-known/skills/swarm-join/skill.md:8` | Build sidecar from source, append buyer to `[seller].accept_offers_only_from` (`home.rs:303`), restart, `join`. |

Relay facts:
- Persistent kinds are admitted only through `required_scope_for_kind`; the maxplayer list ends at
  `ingest.rs:682`, anything else is `restricted: unknown event kind` (`ingest.rs:692`). 30341–30343
  are not on it.
- Production: `openRead = true` (`nix/relay-host.nix:62`), `requireRelayMembership = true` (`:69`).
  Anonymous readers get stored history but no live events (`handlers/req.rs:384`). The board
  already polls every 15 s when AUTH is refused (`shared/relay.mjs:132`).

## 3. Target architecture

```
human ──chat──▶ buyer's agent ──MCP──▶ maxplayer buyer daemon ──▶ our relay ◀── sellers (maxplayer)
                                         │  swarm engine (no LLM)         ▲
                                         │  ├─ swarm jobs + task queues   │ 30341–30344
                                         │  ├─ dispatch / settle / pay    │ + normal job kinds
                                         │  └─ planner driver ──spawn──▶ planner command (buyer's LLM CLI)
                                         ▼
                                    /swarm board (reads our relay)
```

- **Buyer's agent** (any LLM, over MCP): creates swarms and swarm jobs, reads status, steers, closes.
  Nothing depends on it being online.
- **Swarm engine in the buyer daemon** (Rust, no LLM): everything mechanical the runner does today.
- **Planner** (an LLM run headless by the daemon): `analyze` once per swarm job day, then one `step`
  call per change.

## 4. Wire (swarm events only)

Only swarm events change. Offers, claims, awards, results, payment, collect and heartbeats are
untouched; no `PROTOCOL_VERSION` bump.

| Kind | Name | Author | `d` | Key tags | Content |
|---|---|---|---|---|---|
| `30344` **new** | SWARM | buyer | slug | `name`, `status` (`open`\|`closed`), `tasks` (`public`\|`private`, default `public`) | purpose / description (markdown) |
| `30341` | SWARM JOB | buyer | slug | `a` = `30344:<buyer>:<swarm d>` **(new, required)**, `title`, `starts`, `ends`, `status`, `budget <n> sat`, optional `update`, `result` | goal |
| `30342` | SWARM MEMBERSHIP | seller | `<buyer>:<swarm d>` **(was `<buyer>`)** | `a` = swarm address **(was forbidden)**, `p` = buyer, `since`, `minutes` (1–480), `status` (`joined`\|`left`) | note |
| `30343` | SWARM DECISION | buyer | `<seller>` (buyer-wide, as today) or `<seller>:<swarm d>` **(new, per swarm)** | `p` = seller, `decision` (`accepted`\|`denied`), `a` = swarm address **(only and required on per-swarm decisions)** | reason |

- Validators: a swarm job's `a` must name a swarm by the same author; a membership's `a`, `p` and `d`
  must agree. `30344` is unused at the anchor except as a "wrong kind is refused" test value
  (`challenge.rs:435`), which moves to another number.
- **Eligibility** for a swarm job: membership of that job's swarm, `status = joined`, published and
  `since` at or before the job's `starts` (unchanged rule), and the buyer's decisions allow the
  seller in that swarm: take the latest buyer-wide decision and the latest decision for this swarm;
  the seller is eligible if at least one is `accepted` and neither is `denied`. So a buyer-wide deny
  blocks everywhere, a per-swarm deny excludes from one swarm, and a per-swarm accept admits to one
  swarm without trusting the seller elsewhere.
- Decision validator: a per-swarm decision's `d` suffix, `a` and `p` must agree and the swarm must be
  by the same buyer; a buyer-wide decision carries no `a`.
- **Minutes** are per swarm per day: used = sum over that swarm's tasks to that seller whose award
  falls in the day, across all the swarm's jobs. Two overlapping swarm jobs in one swarm share the
  seller's minutes (today each challenge counted separately, `model.mjs:143`, `:150`).
- **Session turns count too** (§7.6). A turn's minutes run from its FOLLOW-UP (`3412`, buyer-signed,
  `root` = the task's offer) to its result, capped at the turn's `deadline`; the turn belongs to the
  same swarm job as the offer it continues. The minutes model gains this rule (today it counts
  offers only).
- Public task offers keep the text prefix `[swarm:<buyer>:<swarm job d>]`; the legacy
  `[challenge:…]` matcher is dropped. Private tasks carry no public link (§7.5).
- Swarm job `update` gains a buyer-signed JSON tally (per member: tasks done, minutes used today).
  Public swarms can recompute it from job events; private swarms rely on it (§7.5).

## 5. Relay

1. Admit 30341–30344 in `required_scope_for_kind` (`ingest.rs:671–682`) as `Scope::MessagesWrite`.
   Writes stay member-gated.
2. No per-kind author allowlist and no buyer filter in clients: any relay member may create swarms;
   `swarm list`, the board and sellers show every buyer's swarms. If spam appears, restrict who may
   publish 30344 with a relay rule, never a key hardcoded in clients.
3. The board reads anonymously with its existing polling. No new read service.

## 6. CLI: `maxplayer swarm …` replaces the sidecar

| Command | Who | Notes |
|---|---|---|
| `maxplayer swarm list [--all] [--json]` | anyone | Swarms on our relay from every buyer: name, purpose (first line), buyer (npub + kind-0 name), members, open swarm jobs, and whether you are a member. `--all` adds closed swarms. |
| `maxplayer swarm show <swarm> [--json]` | anyone | One swarm: description, members with minutes, swarm jobs. |
| `maxplayer swarm join <swarm> --minutes N [--note …]` | seller | Publishes the membership. |
| `maxplayer swarm leave <swarm>` | seller | Membership `status = left`. |
| `maxplayer swarm create --name … --purpose-file … [--slug …]` | buyer (ops) | Same daemon request as MCP `swarm_create`. |
| `maxplayer swarm job create <swarm> --goal-file … --budget-sats N [--title …] [--starts … --ends …]` | buyer (ops) | Same daemon request as MCP `swarm_job_create`. |
| `maxplayer swarm job status <swarm job> [--json]` | both | Replaces sidecar `status`. |
| `maxplayer swarm decide --seller <npub\|hex> --accept\|--deny [--swarm <swarm>]` | buyer | Buyer-wide without `--swarm`, per swarm with it. |
| `maxplayer swarm members [<swarm>] [--json]` | buyer | Applicants and members, per swarm or all. |
| `maxplayer swarm planner-test` | buyer | §9.3. |

- `<swarm>` and `<swarm job>` are `naddr`s as printed by `list`/`show`; `<buyer npub>/<slug>` is
  accepted too. The buyer is read from the address; there is no default buyer.
- Dispatch from `crates/maxplayer/src/cli.rs` next to `buyer`/`seller` (`cli.rs:47`). One relay: the
  home's `relay_url`. Quorum, outbox and multi-relay paging go away.
- `join` appends the buyer to `[seller].accept_offers_only_from` (append-only, keeps entries) and
  prints that the seller must restart. `leave` does not remove it.
- `join` while one of the swarm's jobs is running: membership counts from the swarm's next job; `join`
  says so. A minutes change for a swarm with a running job that counts the seller is refused (it
  would forfeit the day), as the sidecar does today.
- Delete `crates/maxplayer-challenges/` and its CI job; rewrite the `swarm-join` skill. The skill
  tells members sessions are on by default (docker seat), and `join` prints a hint when it is off.

## 7. Swarm engine in the buyer daemon

Started next to the existing loops (`buyer/mod.rs:352`, `:355`) when `[swarm].enabled = true`
(default off).

### 7.1 State (buyer store, schema 9 → 10, `buyer/store.rs:51`)

- `swarms`: `d`, name, purpose, status.
- `swarm_jobs`: `d`, swarm `d`, title, goal, starts, ends, budget_sats, status, plan JSON, solved
  result, steering notes, planner counters, last-planned fingerprint.
- `swarm_tasks`: id, swarm job `d`, workstream, text, duration_secs, max_sats, optional seller pin,
  state (`queued | posted | collected | failed | dropped`), job_id, outcome, answer excerpt, `quotes`
  (earlier task ids whose answers are appended), created_by (`agent | planner`), `fresh` (bool),
  session job id and turn number when sent as a turn.
- `swarm_sessions`: swarm job `d`, seller, job id of turn 1, last turn, state
  (`open | context_reset | dead | closed`).
- Replaces the runner's journal file and lock.

### 7.2 Tick (every 5 s)

1. **Settle** posted tasks through the existing auto-award and delivery watcher (`spawn_auto_award`
   `buyer/mod.rs:1894`, `settle_awarded` `:3158`): collect, store the answer, free the seller.
   Expired/refused become outcomes.
2. **Read** swarms, memberships, decisions and task job events from our relay; compute eligibility and
   minutes per swarm (Rust port of `model.mjs`, §7.4).
3. **Swarm job status:** `scheduled → in_progress` at `starts`; progress `update` every 180 s;
   `completed` when solved, at `ends`, or when every member is out of minutes.
4. **Wake the planner** if a trigger fired (§9.2).
5. **Dispatch** queued tasks (§7.3).

### 7.3 Queue and dispatch

- Ordered queue of `queued` tasks per swarm job.
- A seller is free for a swarm job when its fresh heartbeat (≤ 30 min old) shows it serving (the
  runner reads heartbeat serving state today, `model.mjs:158`), it has no running task from that
  swarm, and it has a seat left (§1.14):
  - **Seats stated** (`slots` + `free_slots` on the beat): seats left = `free_slots` minus this
    buyer's running tasks to it the beat may not reflect — a one-off task whose award is not older
    than the beat (claimed or not), and a session turn posted no earlier than the beat. This can
    count a seat the beat already shows (the seller claimed before the beat, award after), never
    miss one, so the engine may wait a beat longer but never posts into a seat the seller says it
    does not have. Each task sent in a tick takes one seat off for the rest of that tick. The seller
    republishes within seconds of a seat change (at most once per 15 s), so the next beat catches up.
  - **No seats stated** (older seller): no running task from any of this buyer's swarms.
- For each free, eligible member, take the first task that fits: pin matches or none; duration
  ≥ 300 s and ≤ minutes left; price ≥ seller rate and ≤ `max_sats`; reserved + price ≤ budget. Post
  as today (`swarm.mjs:1857`): public, targeted, `text/plain`, inline, prefixed, deterministic
  `request_id`.
- A task nobody can take stays queued; the planner sees it.
- Price: the seller's rate, capped by `max_sats`.

### 7.4 Minutes: one formula, two implementations

The board stays JS, the daemon is Rust. Add `web/app/src/swarm/shared/vectors.json` (events in →
per-seller eligibility and minutes per swarm out), run by both test suites.

### 7.5 Private tasks (`tasks = private` on the swarm)

- Each task is posted with `visibility: private`: targeted, encrypted task text, inline answer in the
  encrypted result body. Private offers, private inline delivery and `[privacy].private_jobs`
  (`home.rs:1737`, default `true` at `:1747`) already exist; the runner today forces
  `visibility:'public'` (`swarm.mjs:1857`).
- **Linking.** The private OFFER schema forbids every unlisted tag and keeps public content empty
  (`docs/specs/private-offers-wire-v2.md` §2.3), so a private task has no public `[swarm:…]` prefix
  and no swarm tag. We do not widen that schema. The daemon links tasks from its own store, and the
  public view of a private swarm uses the buyer-signed tally in the swarm job's `update` (§4).
  Sellers see their own tasks locally.
- **Public view of a private swarm:** name, purpose, members, buyer-reported minutes and task counts.
  No task text or answers.
- **Seller support:** `join` on a private swarm refuses when the local `[privacy].private_jobs` is
  `false`. Any seller with `maxplayer swarm` runs a build that has private jobs.
- **Not a security boundary against the seller.** A private task still reaches one untrusted
  seller, so the planner output filter (§10.4) stays.
- Setting chosen at `swarm_create` (`tasks: private`) / `maxplayer swarm create --private-tasks`;
  changing it applies to tasks posted afterwards.

### 7.6 Session jobs by default

Uses the existing session jobs (`docs/specs/session-jobs.md`): FOLLOW-UP `3412`, MCP/daemon `reply`,
claims advertise `["session","1"]`; sellers need `[sessions]` on (the default since sessions became default-on; `enabled = false` turns
it off) on a docker seat with container delivery.

- **One session per seller per swarm job.** The seller's first task in that swarm job is an ordinary
  task job. If its claim advertised sessions, the daemon records a session; the seller's next task
  from that swarm job goes as a `reply` turn on it (same agent conversation), with self-contained
  task text, so the turn stands on its own if memory is lost.
- **Falls back to a new one-off task** when: the claim did not advertise sessions; the task is
  `fresh`; the task is shorter than 900 s (a seller on the default `wrap_up_secs` needs 720 s left to
  start a turn, and the buyer cannot see that setting); the session is `dead` (reply refused, or the turn never started before its deadline —
  parked sessions expire after 6 h or a seller host/Docker restart, and can be evicted for disk);
  or the seller is not the pinned/selected seller for that task anyway. A turn that reports
  `["context","reset"]` is still a valid result; the session continues without memory and the lead
  sees `context_reset`.
- **`fresh`.** The lead or the buyer's agent may mark any task `fresh` (new agent, no history). The
  independent verify task before `solved` (§10.6) is always fresh and goes to a different seller.
- **Dispatch rules unchanged.** A turn is a task like any other: same minutes, rate, budget and
  one-task-per-seller-per-swarm checks; the previous turn is collected before the next is sent (as
  `reply` requires). Each turn is paid on collect at the seller's rate.
- **Cap.** Each session is opened with `max_session_sats` = the swarm job's budget, so the session cap
  never binds before the swarm job budget, which the daemon enforces per turn.
- **Close.** Sessions are closed (`reply close`) when the swarm job closes; otherwise they expire on the
  seller's lifetime.
- **Cost.** Every turn runs both reviews again (session-jobs §0.8), adding latency per task.
- **Private tasks.** Follow-ups on private jobs are encrypted like the offer (session-jobs §2.1).
  Whether the anchor's implementation covers private follow-ups end to end is checked in PR 4; if not,
  private swarms fall back to one-off tasks until it does.

## 8. MCP tools (buyer's agent)

| Tool | Does |
|---|---|
| `swarm_create {name, purpose, slug?, tasks?: public\|private}` | Publishes a 30344 swarm. Returns its address. |
| `swarm_list {}` / `swarm_members {swarm?}` | The buyer's swarms; applicants and members with minutes. |
| `swarm_decide {seller, accept, swarm?}` | Buyer-wide decision, or per swarm when `swarm` is given. |
| `swarm_job_create {swarm, goal, title?, starts?, ends?, budget_sats, tasks?[]}` | Publishes a 30341 swarm job (default: now → +24 h). Optional tasks are queued (fan-out without a planner). Says whether a planner is configured. |
| `swarm_job_status {swarm_job, results_page?}` | Status, members with minutes (each fresh seat announcement carries `heartbeat.announced_at_unix` and `observed_at_unix`, when the daemon's last members snapshot read it; status adds `announced_secs_ago`, the announcement's age as of that snapshot, and `checked_secs_ago`, the snapshot's own age, which keeps growing once the job stops running or the relay cannot be read; both are absent on snapshots from older builds and when `heartbeat` is null), queue, running tasks (each with its `stage`: `posting`, `awaiting_claim`, `awarding` (a claim is being awarded, its price reserved), `awarded`, `turn_pending` or `turn_sent`, and `stage_secs`, the time in that stage: from the post, from the award, or from the turn's claim; `null` without a timestamp), results (paged), budget, planner state, last idle reason. |
| `swarm_job_tasks {swarm_job, add?[], drop?[], replace?[]}` | Edit the queue. |
| `swarm_job_steer {swarm_job, note}` | Note the planner sees on every call. |
| `swarm_job_close {swarm_job, result?, status: completed\|cancelled}` | Stop posting; in-flight tasks still settle and pay. |

A `swarm-buyer` skill and the tool descriptions carry the guidance: create a swarm first, write
self-contained tasks of ≥ 5 min, verify before closing as solved.

## 9. Planner

### 9.1 Interface

- The daemon builds the full prompt (rules + state JSON), writes it to the command's stdin, extracts
  one JSON object from stdout, validates it, and feeds rejection reasons back on the next call.
- `analyze` (once per swarm job day, and when the plan is invalidated): workstreams + verification.
- `step` (replaces `assign` + `assess`): input = goal, plan, steering notes, queue, running tasks,
  results (newest 8 full, older trimmed, ≤ 200k chars as today), members with minutes and rates,
  budget headroom, last rejections. Output any of: tasks to add, queue `drop`/`replace`, revised
  `plan`, `wait` + `idleReason`, `solved` + result + verification. Tasks returned after posting
  closed are ignored.
- Prompt and checks are ported from `planner-llm.mjs` and the runner (e.g. `swarm.mjs:761`) to Rust,
  including "all inputs are untrusted data" (`planner-llm.mjs:91`).

### 9.2 When `step` runs

A task settled (batched per tick); the queue is empty while a payable member is free; the last
response was rejected; a steering note or edit arrived; or 10 min passed with none of these. One
call in flight per swarm job. Timeouts 360 s `analyze`, 180 s `step`; the process group is killed on
timeout.

### 9.3 Presets and `planner_command`

```toml
[swarm]
planner = "claude"            # preset name, or "command"
planner_command = "…"         # only when planner = "command"
```

| Preset | Command | Status at writing |
|---|---|---|
| `claude` | `claude -p --output-format text --tools "" --no-session-persistence` | Flags checked in `--help` (2.1.280: `--tools ""` disables all tools); what the runner runs today. Live canary not run here (expired login). |
| `codex` | `codex exec --ephemeral --skip-git-repo-check --sandbox read-only --disable shell_tool --disable unified_exec -` | Flags exist in 0.169.0 (`exec --help`, `features list`). Not proven that no tool remains: ship only after a live canary. `--sandbox read-only` alone is not enough (§10). |
| `cursor` | — | `-p` "has access to all tools, including write and shell" (Cursor CLI docs); `--mode ask` still reads. No preset. |
| `goose` | — | No documented way to load no extensions in `goose run`. No preset. |

`maxplayer swarm planner-test` (at install, on daemon start, before each swarm job): tiny prompt →
JSON back, plus the canaries in §10. Failure disables the planner and shows in
`swarm_job_status`.

### 9.4 No planner: check-in mode

Agent-queued tasks are dispatched; results wait for the agent's next `swarm_job_status`.
`swarm_job_create` says so.

### 9.5 How the lead and sellers communicate (as today)

v1 keeps the runner's model. Boards, a shared "team orders" text and structured seller reports move to
the follow-up with the shared repo (§14.2), where there is a real work product to coordinate around.

- **Lead → seller:** only through the task text the lead writes. Team-wide context is written into
  each task. The lead may attach up to 5 earlier answers (`includeAnswers`, `planner-llm.mjs:44`),
  appended as untrusted data.
- **Seller → lead:** only through the task's answer, which the lead sees in `step` results. A seller
  may suggest next steps or splits in prose; the lead decides.
- **No seller-to-seller path:** everything another seller sees was written or selected by the lead
  (§1.18).
- **Buyer's agent → lead:** steering notes (`swarm_job_steer`), seen on every call. They reach sellers
  only if the lead writes them into tasks.
- **New versus today:** with session jobs (§7.6) a seller's agent keeps its own earlier tasks of the
  swarm job in memory.

## 10. Security of the planner call

Seller answers enter the prompt, and task text the planner returns is posted publicly. A planner with
any tool, even read-only, can be steered into reading `~/.maxplayer/key` or wallet files and leaking
them in a task.

0. Private tasks (§7.5) cut public exposure but not exposure to the receiving seller, so every
   item below applies to both settings.
1. Presets carry the no-tools flags; agents never compose them.
2. Canaries in `planner-test`, in a fresh temp dir: a write canary (file appears ⇒ refuse) and a read
   canary (token from a file appears in output ⇒ refuse). They catch forgotten flags, not every case.
3. Every call runs in a fresh empty temp dir, deleted afterwards; environment inherited.
4. Output filter before queueing: refuse tasks containing the buyer's secret key, a cashu token
   prefix (`cashuA`/`cashuB`) or a wallet seed phrase.
5. Output is JSON only and the daemon queues only within budget: with tools off the worst case is
   wasted budget or a fake `solved`.
6. `solved` needs one independent verify task by a different member; the agent can override with
   `swarm_job_close`.
7. Our buyer runs on a dedicated machine/user.

## 11. Renaming "challenge"

| Today | Becomes |
|---|---|
| `crates/maxplayer-core/src/challenge.rs`, `CHALLENGE_KIND`, `CHALLENGE_MEMBERSHIP_KIND`, `CHALLENGE_DECISION_KIND` | `swarm.rs`, `SWARM_JOB_KIND`, `SWARM_MEMBERSHIP_KIND`, `SWARM_DECISION_KIND`, new `SWARM_KIND` |
| `kinds.rs` table rows "CHALLENGE …" | "SWARM JOB", "SWARM MEMBERSHIP", "SWARM DECISION", "SWARM" |
| `web/app/src/challenge/`, `challengeTaskPrefix`, `CHALLENGE_RELAY_*`, `CHALLENGE_BUYER_PUBKEY` | `web/app/src/swarm/`, `swarmTaskPrefix`; relay and buyer-key settings removed |
| `docs/challenges-v1.md`, `docs/challenge-coordinate.md`, `scripts/challenges/` | replaced by this spec and the new docs; removed in PR 6 |
| "run", "challenge" in CLI/UI/errors | "swarm job" |

Done inside the PRs that touch each file (2, 3, 6), not as a separate sweep.

## 12. Staged PRs

Each behind a default-off flag where it changes running behaviour.

1. **Relay admits 30341–30344.** Test: member write of each kind accepted, non-member refused. Relay
   deploy.
2. **Wire + CLI.** `swarm.rs` with the §4 validators (new 30344, membership per swarm, required `a`
   on swarm jobs), `maxplayer swarm …` (§6), `swarm-join` skill rewrite, delete
   `crates/maxplayer-challenges` and its CI job. Tests: port the sidecar's event/eligibility tests;
   `list` shows swarms of two buyers; `join` publishes `d = <buyer>:<swarm>`; config append keeps
   entries; minutes change refused mid-job; join during a running job reports next-job eligibility; decision vectors: buyer-wide accept +
   per-swarm deny excludes from that swarm only, per-swarm accept alone admits to that swarm only,
   buyer-wide deny wins over a per-swarm accept.
3. **Board on our relay.** `/swarm` lists swarms from every buyer, `/swarm/<naddr>` one swarm and its
   jobs; drop the denylist and build-time buyer key; minutes per swarm and the shared vectors (§7.4).
   Vercel deploy.
4. **Daemon engine + MCP, no planner** (§7, §8) behind `[swarm].enabled`, including private tasks
   (§7.5). Tests: fan-out of N
   tasks over M members with a stub seller; budget never exceeded; restart resumes from the store;
   expired task frees its seller; a member of two swarms gets one task from each; vectors pass in
   Rust; a private swarm's offers carry no task text or swarm link in public events and its tally
   matches the store; with a session-capable stub seller, the second task to it in one swarm job is a `reply`
   turn and its minutes are counted from the follow-up; a `fresh` task, a non-session claim and a dead
   session each produce a new one-off job; closing the swarm job closes its sessions.
5. **Planner** (§9, §10): presets, `planner_command`, `planner-test` with canaries, `analyze` +
   `step`, triggers, output filter, verify-before-solved. Tests: stub planner;
   rejected output fed back; canaries refuse a writing / reading command; idle ceiling; one call per
   settled batch; `includeAnswers` appends earlier answers as untrusted data.
6. **Retire** `scripts/challenges/` and the old docs after one live swarm job on the new path.

Bob's rule stands: no swarm merges while a swarm job is live; confirm the board deploy before telling
anyone to switch.

## 13. Migration

Members are few and their events live on the public relays. After PR 2 we create our swarm(s); each
seller runs `maxplayer swarm join <swarm> --minutes N` once and we re-`decide` them. No dual-relay
period.

## 14. Deliberately left out

- Per-buyer free/paid (`free_for`), auto-raise beyond `max_sats`.
- Input files, git/code delivery for tasks (text/inline only; see #1250), and a buyer-run checker
  plus toolchain capabilities such as `lean`. Needed for proof/code swarm jobs like the Fermat run;
  follow-up spec, see §14.1.
- Boards, a shared team-orders text and structured seller reports (§14.2).
- Fully hidden swarms (encrypted name, purpose, goal and roster; invite-only discovery).

### 14.1 Notes for the follow-up: shared work product

Idea: swarm tasks deliver into a shared buyer repo through contribution mode (seller forks, delivers
a commit; `verify_contribution`, `delivery_git.rs:583`), a buyer-side checker validates, the lead
merges. New risks versus text answers:

1. **The buyer runs seller code.** Builds, install hooks and tests execute seller code; an
   unsandboxed checker gives every member code execution next to the buyer's key and wallet.
2. **Seller-to-seller instructions through the repo.** Harnesses auto-load `AGENTS.md`,
   `CLAUDE.md`, `.cursor/` etc. from the working directory, bypassing "everything through the lead"
   (§1.18).
3. **Gaming the checker** by editing tests or checker config (in Lean: `sorry`, new `axiom`).
4. **Repo exposure:** every member sees the whole repo, private tasks or not; buyer CI with secrets
   could run on merged changes.
5. **Licence/provenance and junk** (copied code, huge files).

Defaults the follow-up must enforce (not advise): a dedicated empty repo per swarm job, never the
buyer's own; checker in a throwaway sandbox with no network and nothing from the buyer home, and no
automatic merge without one; refuse changes to tests, checker config, agent instruction files,
`.github/`, submodules, out-of-repo symlinks and files over a size cap; only the lead merges, after
the checker passes; off by default, enabled per swarm.

The normal market already carries risks 1, 2, 4, 5 for contribution jobs: collect only
materializes a read-only copy (`collect.rs:253`), and the default `ContentPolicy` refuses only empty
diffs (`contribution.rs:285`). A stricter default policy and an "untrusted code" warning in the buyer
skill and collect output are separate from swarm work.
- Restricting who may create swarms (addable later as a relay rule on 30344).
- Opting out of one swarm job inside a swarm (membership is per swarm).
- MCP sampling as a planner backend.

No open questions remain.

### 14.2 Notes for the follow-up: boards

Designed in this spec's earlier revisions (PR #1313 history, commit `a9c40938` §9.5); moved out on
16 Nov because without a shared repo one text per task is enough. Starting point:

- **Board:** named list of short text entries per swarm job, written **only by the lead** (planner
  JSON output, or the buyer's agent via MCP). The lead chooses which tasks get it pasted (all, one
  workstream, none = lead memory) and how much. No default boards; the prompt gives no example
  structure.
- **Into tasks:** the daemon prepends selected boards under `## Team orders`; ordinary job text, no
  wire change.
- **Seller reports:** an optional JSON block at the end of an answer (`notes`, `proposed_subtasks`
  with a `parent` link), size-capped, stored in a lead inbox marked untrusted. Nothing reaches
  another seller until the lead restates or accepts it (§1.18). Accepted subtasks grow a task tree.
- **Costs:** every seller-to-team hop waits for a lead call; the lead bottlenecks at hundreds of
  sellers. Mid-task (live) boards need a seller tool and sandbox network access.
