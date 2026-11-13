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
| **member** | A seller that joined a swarm and is accepted by its buyer. | member |

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
13. **Acceptance is per buyer:** accepting a seller once trusts it in all that buyer's swarms. (Bob)
14. **No cross-swarm cap:** a seller in several swarms of one buyer may get a task from each at the
    same time; only the seller's own slot count limits it. (Bob)
15. **First-draft questions:** `join` edits seller config (append-only); `solved` needs one
    independent verify task, no buyer confirmation; our buyer uses the `claude` preset; `codex`
    ships as a preset only after a live canary. (Josip, accepted defaults)
16. **Private tasks are a setting.** A swarm can send its tasks as private offers. (Bob, 13 Nov 17:13)
17. **The lead has the tools to set up its own coordination structure if it wants to**, e.g. the five
    tables in Anthropic's Fermat run (Announce, Assign, Notes, Subtasks, Landed). Nothing is
    prescribed: no default boards, no required structure. Team-wide orders reach sellers inside the
    task text. (Bob, 13 Nov 17:03–17:18)
18. **Everything passes through the lead.** Sellers never take orders from other sellers: only the
    lead writes boards and the queue; seller notes and proposed subtasks go to the lead, which
    decides what, if anything, other sellers see. (Bob, 13 Nov 17:17)

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
| `30343` | SWARM DECISION | buyer | seller pubkey | `p` = seller, `decision` | reason (unchanged; per buyer) |

- Validators: a swarm job's `a` must name a swarm by the same author; a membership's `a`, `p` and `d`
  must agree. `30344` is unused at the anchor except as a "wrong kind is refused" test value
  (`challenge.rs:435`), which moves to another number.
- **Eligibility** for a swarm job: membership of that job's swarm, `status = joined`, published and
  `since` at or before the job's `starts` (unchanged rule), and the buyer's latest decision for the
  seller is `accepted`.
- **Minutes** are per swarm per day: used = sum over that swarm's tasks to that seller whose award
  falls in the day, across all the swarm's jobs. Two overlapping swarm jobs in one swarm share the
  seller's minutes (today each challenge counted separately, `model.mjs:143`, `:150`).
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
| `maxplayer swarm decide --seller <npub\|hex> --accept\|--deny` | buyer | Per buyer. |
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
- Delete `crates/maxplayer-challenges/` and its CI job; rewrite the `swarm-join` skill.

## 7. Swarm engine in the buyer daemon

Started next to the existing loops (`buyer/mod.rs:352`, `:355`) when `[swarm].enabled = true`
(default off).

### 7.1 State (buyer store, schema 9 → 10, `buyer/store.rs:51`)

- `swarms`: `d`, name, purpose, status.
- `swarm_jobs`: `d`, swarm `d`, title, goal, starts, ends, budget_sats, status, plan JSON, solved
  result, steering notes, planner counters, last-planned fingerprint.
- `swarm_tasks`: id, swarm job `d`, workstream, text, duration_secs, max_sats, optional seller pin,
  state (`queued | posted | collected | failed | dropped`), job_id, outcome, answer excerpt, `quotes`
  (earlier task ids whose answers are appended), created_by (`agent | planner`).
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
- A seller is free for a swarm when it has no running task from that swarm and its heartbeat shows a
  free slot (the runner reads heartbeat serving state today, `model.mjs:158`). No cross-swarm cap
  (§1.14).
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
  No task text, answers or boards.
- **Seller support:** `join` on a private swarm refuses when the local `[privacy].private_jobs` is
  `false`. Any seller with `maxplayer swarm` runs a build that has private jobs.
- **Not a security boundary against the seller.** A private task still reaches one untrusted
  seller, so the planner output filter (§10.4) stays.
- Setting chosen at `swarm_create` (`tasks: private`) / `maxplayer swarm create --private-tasks`;
  changing it applies to tasks posted afterwards.

## 8. MCP tools (buyer's agent)

| Tool | Does |
|---|---|
| `swarm_create {name, purpose, slug?, tasks?: public\|private}` | Publishes a 30344 swarm. Returns its address. |
| `swarm_list {}` / `swarm_members {swarm?}` | The buyer's swarms; applicants and members with minutes. |
| `swarm_decide {seller, accept}` | Per-buyer acceptance. |
| `swarm_job_create {swarm, goal, title?, starts?, ends?, budget_sats, tasks?[]}` | Publishes a 30341 swarm job (default: now → +24 h). Optional tasks are queued (fan-out without a planner). Says whether a planner is configured. |
| `swarm_job_status {swarm_job, results_page?}` | Status, members with minutes, queue, running tasks, results (paged), budget, planner state, last idle reason. |
| `swarm_job_tasks {swarm_job, add?[], drop?[], replace?[]}` | Edit the queue. |
| `swarm_job_steer {swarm_job, note}` | Note the planner sees on every call. |
| `swarm_job_close {swarm_job, result?, status: completed\|cancelled}` | Stop posting; in-flight tasks still settle and pay. |
| `swarm_job_boards {swarm_job, create?[], post?[], remove?[]}` | Same board operations as the planner (§9.5), for a buyer's agent acting as lead. |

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

### 9.5 Boards: optional primitives for the lead

Boards are tools, not a structure. A swarm job starts with **no boards**; the planner prompt
describes what the primitives can do and leaves it to the lead (planner, or the buyer's agent)
whether to use them at all. Board operations are part of the lead's JSON output, so the lead still
needs no tools. A lead that wants the Fermat run's setup can build it from these primitives:

| Their table | How a lead could build it |
|---|---|
| Announce | a board the lead creates with `include: all` |
| Assign | a board the lead writes, plus task pins and workstream owners |
| Notes | seller reports go to the lead's inbox; the lead republishes what it chooses to a board |
| Subtasks | the task queue; sellers *propose* subtasks with a `parent` link, the lead accepts them into the queue (task tree) |
| Landed | the results log, filled automatically; only the lead decides what of it other sellers see |

- **Rule: the lead is the only writer.** Nothing a seller writes reaches another seller unless the
  lead itself writes it to a board or into a task. No board has member write access.
- **Board** = named, ordered list of short text entries stored by the daemon per swarm job.
  Fields set by the lead: `name`, `purpose`, `include`
  (`all` \| `workstream` \| `none`: which tasks get it pasted in), `cap_chars` (how much of the newest
  entries is pasted). The lead creates, renames, posts to, and removes entries from any board, and
  may define none or as many as it likes (limit 16 per swarm job).
- **Into the task text.** The daemon builds each task as:
  `## Team orders` / boards selected by `include`, newest first, trimmed to `cap_chars` /
  `## Your task` / `## Reporting` (how to report back). Every entry was written by the lead.
  Ordinary job text, so sellers and the wire do not change.
- **Back from sellers: to the lead only.** A seller may end its answer with one fenced JSON block:
  `{"notes":["text"],"proposed_subtasks":[{"text","parent","workstream","durationSeconds"}]}`.
  The daemon strips it from the answer, checks sizes (≤ 2 KB per note, ≤ 8 notes and ≤ 8 proposals
  per task) and that `parent` exists, and stores it in the **lead inbox**, marked untrusted with its
  seller and task. Nothing from the inbox is pasted into any task or board automatically.
- **Lead decides.** `step` receives the inbox, every board and the task tree, and returns board
  writes and queue additions in its own words: accept a proposal (`accept: [proposal id]`, optionally
  edited), reject it, or summarise several notes into one board entry. Accepted subtasks keep their
  `parent`, so the tree still grows from sellers' splits. Dispatch then enforces minutes, rate and
  budget as for any task. New inbox items count as a change for the `step` trigger (§9.2).
- **Cost.** Every seller-to-team hop now waits for a lead call (seconds to a few minutes), and the
  lead's input grows with the inbox (trimmed like results, newest first). With a few dozen members
  this is fine; at Fermat scale (hundreds of helpers splitting work themselves) the lead becomes
  the bottleneck. Accepted trade-off.
- **No planner (check-in mode).** The inbox waits for the buyer's agent, which accepts or rejects
  through `swarm_job_boards` / `swarm_job_tasks`.
- **Timing.** Sellers see board updates when their next task starts. Tasks are 5–30 min, so that is
  the coordination delay. Mid-task reads/writes are a later phase (§14).
- **Visibility.** In a public swarm, boards are public because the task text is. In a private swarm
  they travel only inside encrypted task text (§7.5).
- **Injection.** A seller can still try to steer the lead through its notes, as it can through its
  answer today. The prompt treats inbox items as untrusted data, the lead restates rather than
  copies, and the output filter (§10.4) applies to board writes as to tasks. There is no direct
  seller-to-seller path.

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
   entries; minutes change refused mid-job; join during a running job reports next-job eligibility.
3. **Board on our relay.** `/swarm` lists swarms from every buyer, `/swarm/<naddr>` one swarm and its
   jobs; drop the denylist and build-time buyer key; minutes per swarm and the shared vectors (§7.4).
   Vercel deploy.
4. **Daemon engine + MCP, no planner** (§7, §8) behind `[swarm].enabled`, including private tasks
   (§7.5) and boards with task-text assembly and seller report parsing (§9.5). Tests: fan-out of N
   tasks over M members with a stub seller; budget never exceeded; restart resumes from the store;
   expired task frees its seller; a member of two swarms gets one task from each; vectors pass in
   Rust; a private swarm's offers carry no task text or swarm link in public events and its tally
   matches the store; board text lands in the next task; a seller report lands only in the lead
   inbox, never in another seller's task text, until the lead accepts it; reports past the caps are
   refused.
5. **Planner** (§9, §10): presets, `planner_command`, `planner-test` with canaries, `analyze` +
   `step` with board operations, triggers, output filter, verify-before-solved. Tests: stub planner;
   rejected output fed back; canaries refuse a writing / reading command; idle ceiling; one call per
   settled batch; a swarm job with no boards works exactly like one without the feature; a stub
   lead that chooses to build the five Fermat-style boards can drive a task tree with them.
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
  next phase.
- Live boards: sellers reading or writing boards mid-task. Needs a seller tool and network access
  from the sandbox; next phase after file delivery.
- Fully hidden swarms (encrypted name, purpose, goal and roster; invite-only discovery).
- Restricting who may create swarms (addable later as a relay rule on 30344).
- Per-swarm acceptance (acceptance is per buyer, §1.13).
- Opting out of one swarm job inside a swarm (membership is per swarm).
- MCP sampling as a planner backend.

No open questions remain.
