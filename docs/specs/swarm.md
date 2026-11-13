# Swarm in maxplayer — no sidecar, no runner script

**Status:** design spec. No code written. **Anchor commit: `4d67a21c5c8f6032f1602b50008b4ec6f42a39b1`
(= `origin/main` at the time of writing).** Code citations are `file:line @4d67a21c`, re-derived by
grep at that commit.

**Scope.** Make swarm a normal part of `maxplayer`: one install for buyers and sellers, swarm events on
our relay, the runner's work done by the buyer daemon, and a planner that runs on whatever LLM agent
the buyer already has.

## 0. Settled inputs — not relitigated here

Josip, #swarm-production, 11–13 Nov 2026:

1. **Remove the sidecar.** Swarm becomes a regular part of `maxplayer`.
2. **Use case for now:** our own buyer posts swarm runs (math, physics problems …) and pays sellers
   to take part. Free-vs-paid per buyer (`free_for`) is out of scope.
3. **Our relay**, not public relays, carries the swarm events.
4. **Buyer UX:** the human tells their agent "install maxplayer buyer", then "create a swarm job to do
   X". No custom installs, no complex configs.
5. **Any LLM.** Not Claude-only. How the buyer pays for their LLM (subscription or API key) is
   irrelevant to maxplayer; maxplayer never sees an LLM credential.
6. **Runs can be fan-out or iterative**, so an unattended planner is needed.
7. **Planner backend:** a command the buyer's agent registers at install (`planner_command`), with
   built-in presets for known CLIs and a raw command as the escape hatch.
8. **A small task queue** per run in the daemon.
9. **One planner call per change** (today's `assign` and `assess` merged).
10. **Many buyers.** Any buyer can create swarm runs; a buyer is identified by its pubkey. Sellers
    discover runs on our relay and join a run's buyer. No buyer key is built into the client.
11. **Open questions 2–5 of the first draft accepted with their defaults** (Josip, 13 Nov 15:29);
    question 1 is superseded by input 10. See §12.

## 1. What exists today

| Piece | Where | What it does |
|---|---|---|
| Event kinds | `crates/maxplayer-core/src/kinds.rs:26`, `:28`, `:30 @4d67a21c` | `30341` run (buyer), `30342` membership (seller), `30343` decision (buyer). Validation in `challenge.rs`; author checks at `challenge.rs:121`, `:232`. |
| Sidecar `maxplayer-swarm` | `crates/maxplayer-challenges/` (own workspace, ~4.7k lines Rust) | Signs/reads the three kinds on nos.lol, primal, offchain.pub with a 2-of-3 quorum and an outbox (`README.md:27 @4d67a21c`). Refuses `relay.maxplayer.ai` (`src/relay.rs:774`). No release binary; CI builds it only (`.github/workflows/ci.yml:328`). |
| Runner | `scripts/challenges/swarm.mjs` (2,130 lines) | Node loop: `tick()` (`swarm.mjs:1453`) every 5 s (`swarm.mjs:1976`). Settles open jobs, reads relays, calls the planner, posts tasks through buyer MCP `post_job` (`swarm.mjs:1857`). |
| Planner | `scripts/challenges/planner-llm.mjs` | Spawns `claude -p --output-format text --tools '' --no-session-persistence` (`planner-llm.mjs:101`). Modes `analyze` (`swarm.mjs:1746`), `assign`, `assess` (`swarm.mjs:1750`, runs when the result hash changes, `swarm.mjs:1748`). Prompt treats every input as untrusted data (`planner-llm.mjs:91`). |
| Task ↔ run link | `web/app/src/challenge/shared/events.mjs:101` | Public offer text prefixed `[swarm:<buyer>:<d>]\n`. |
| Minutes | `web/app/src/challenge/shared/model.mjs:109` | Derived from job events: award → result, capped at deadline. Shared by runner and board. |
| Board `/swarm` | `web/app/src/challenge/` | Reads the public relays; denylists ours (`shared/relay.mjs:539`); buyer key baked at build (`web/app/scripts/swarm-buyer.mjs:28`). |
| Seller join | `web/app/.well-known/skills/swarm-join/skill.md:8` | Build sidecar from source, append buyer to `[seller].accept_offers_only_from` (`home.rs:303`), restart, `join`. |

Relay facts at this commit:
- Persistent kinds are admitted only through `required_scope_for_kind`; the maxplayer list ends at
  `ingest.rs:682` and anything else is `restricted: unknown event kind` (`ingest.rs:692`). 30341–30343
  are not on it.
- Production has `openRead = true` (`nix/relay-host.nix:62`) and `requireRelayMembership = true`
  (`:69`). Anonymous readers get stored history and EOSE but no live events (`handlers/req.rs:384`);
  a refused AUTH falls back to anonymous reads (`connection.rs:678`). The board already falls back to
  15 s polling when AUTH is refused (`scripts/challenges/README.md:87`; `shared/relay.mjs:132`).

## 2. Target architecture

```
human ──chat──▶ buyer's agent ──MCP──▶ maxplayer buyer daemon ──▶ our relay ◀── sellers (maxplayer)
                                         │  swarm engine (no LLM)        ▲
                                         │  ├─ run store + task queue    │ 30341/30342/30343
                                         │  ├─ dispatcher / settle / pay │ + normal job kinds
                                         │  └─ planner driver ──spawn──▶ planner command (buyer's LLM CLI)
                                         ▼
                                    /swarm board (reads our relay)
```

Three roles, cleanly split:
- **Buyer's agent** (any LLM, through MCP): creates runs, reads status, steers, closes. It is a client;
  nothing depends on it being online.
- **Swarm engine in the buyer daemon** (Rust, no LLM): everything mechanical the runner does today.
- **Planner** (an LLM, run headless by the daemon through `planner_command`): `analyze` once per run
  day, then one `step` call per change.

## 3. Relay

1. Add 30341, 30342, 30343 to the maxplayer list in `required_scope_for_kind` (`ingest.rs:671-682`)
   as `Scope::MessagesWrite`. Writes stay member-gated (sellers and our buyer are already members).
2. No per-kind author allowlist on the relay and no buyer filter in clients: any relay member may
   publish a run, and `swarm list`, the board and sellers show runs from every buyer. Spam is bounded
   by the membership gate. If it becomes a problem, restrict who may publish 30341 with a relay rule,
   never with a key hardcoded in the client.
3. Reads: the board reads anonymously (open read) with its existing polling fallback. No new read
   service.
4. Deploy: relay deploy before PR 2 is released (§9).

## 4. CLI: `maxplayer swarm …` replaces the sidecar

| Command | Who | Replaces |
|---|---|---|
| `maxplayer swarm list [--all] [--json]` | seller | new: open runs on our relay from every buyer |
| `maxplayer swarm status (<run> \| --buyer <npub>) [--json]` | both | sidecar `status` |
| `maxplayer swarm join (<run> \| --buyer <npub>) --minutes N [--note …]` | seller | sidecar `join` |
| `maxplayer swarm leave (<run> \| --buyer <npub>)` | seller | sidecar `leave` |
| `maxplayer swarm decide --seller <hex> --accept\|--deny` | buyer | sidecar `decide` |
| `maxplayer swarm members [--json]` | buyer | sidecar `members` |

- Dispatch from `crates/maxplayer/src/cli.rs` next to `buyer`/`seller` (`cli.rs:47`).
- One relay: the home's `relay_url`. The quorum, outbox and multi-relay paging go away; validation
  stays in `maxplayer-core/src/challenge.rs`.
- **Identifying the buyer.** A buyer is its pubkey. A run is addressed `30341:<buyer pubkey>:<d>`
  and printed by `list` as an `naddr`, so two buyers' runs never clash. `<run>` accepts that `naddr`;
  the buyer is read from the run event. `--buyer <npub|hex>` is the alternative. There is no default.
- **`list`** shows, per run: title, buyer (npub + kind-0 name if any), status, starts/ends, budget,
  number of accepted members, and whether this seller is already a member of that buyer. Default:
  `scheduled` and `in_progress` runs; `--all` adds closed ones.
- **Membership stays per buyer**, as on the wire today: the 30342 event has `d = <buyer pubkey>`
  (`challenge.rs`). Joining via a run makes the seller a member of that run's buyer, so it also
  covers that buyer's later runs until `leave`. A seller joins several buyers with several
  memberships, each with its own daily minutes. The existing eligibility rule stays: a membership
  counts for a run only if published at or before its `starts`, so joining a run that has already
  started counts from the buyer's next run; `join` says so.
- `join` appends the buyer to `[seller].accept_offers_only_from` itself (preserving entries) and
  prints that the seller must be restarted. Today this is a manual step and the most common miss.
  `leave` does not remove it (the seller may still want that buyer's market jobs).
- Run creation (`publish`) is **not** a CLI command for humans; it is the MCP tool `swarm_create` (§6).
  A thin `maxplayer swarm create --goal-file …` wrapper over the same daemon request is fine for ops.
- Keep the existing safety rule: `join` refuses a minutes change while a run that counts the seller is
  running (it would forfeit the day).
- Delete `crates/maxplayer-challenges/` and its CI job; update `swarm-join` skill to the new commands.

## 5. Swarm engine in the buyer daemon

Started next to the existing loops (`spawn_delivery_watcher`, `spawn_reconcile_loop`,
`buyer/mod.rs:352`, `:355`) when `[swarm].enabled = true` (default off).

### 5.1 State (buyer store, schema 9 → 10, `buyer/store.rs:51`)

- `swarm_runs`: `d`, goal, title, starts, ends, budget_sats, status, plan JSON, solved result,
  planner counters, last-planned fingerprint.
- `swarm_tasks`: id, run `d`, workstream, task text, duration_secs, max_sats, optional seller pin,
  state (`queued | posted | collected | failed | dropped`), job_id, outcome, answer excerpt,
  `quotes` (earlier task ids whose answers are appended), created_by (`agent | planner`).
- Replaces the runner's journal file and lock.

### 5.2 Tick

Every 5 s (same cadence as today):
1. **Settle** posted tasks: use the existing auto-award and delivery watcher (`spawn_auto_award`
   `buyer/mod.rs:1894`, `settle_awarded` `:3158`); on delivery, collect, store the answer
   (flattened like `readDeliveredAnswer` today), mark the seller free. Expired/refused → outcome.
2. **Read** run/membership/decision events and the run's job events from our relay; compute
   eligibility and minutes used/left (Rust port of `model.mjs:109`, §5.4).
3. **Run status:** `scheduled → in_progress` at `starts`; progress `update` every 180 s;
   `completed` on solved, at `ends`, or when every seller is out of minutes.
4. **Wake the planner** if a trigger fired (§7.2).
5. **Dispatch** from the queue to free sellers (§5.3).

### 5.3 Queue and dispatch

- Ordered queue of `queued` tasks per run.
- For each free, eligible seller (one job at a time, as today), take the first task that fits:
  pin matches or no pin; `duration ≥ 300 s` and ≤ minutes left; price ≥ seller's rate and
  ≤ `max_sats`; reserved + price ≤ budget. Post it as today (`swarm.mjs:1857`): public, targeted,
  `text/plain`, inline, `[swarm:<buyer>:<d>]` prefix, deterministic `request_id`.
- A task nobody can take stays queued; the planner sees it in its input.
- Price: pay the seller's rate (capped by `max_sats`). Auto-raise and paid/free policy beyond that
  are out of scope (§0.2).

### 5.4 Minutes: one formula, two implementations

**Scope change: minutes are counted per buyer per day, across all of that buyer's runs.** Today a
run counts only its own jobs (`model.mjs:143` takes the selected run's job facts; `:150` sums them),
so two overlapping runs from one buyer could each use a seller's full daily minutes. With many runs
per buyer this matters: the membership offers minutes to a buyer, not to a run. Used minutes for a
seller = sum over that buyer's jobs to that seller whose award falls in the run day, whichever run
posted them. One job at a time per seller per buyer, across runs.

The board stays JS, the daemon becomes Rust. Both must compute identical minutes and eligibility.
Add `web/app/src/challenge/shared/vectors.json` (events in → per-seller minutes/eligibility out),
run by both test suites. A change to either implementation without the vectors fails CI.

## 6. MCP tools (buyer's agent)

| Tool | Does |
|---|---|
| `swarm_create {goal, title?, starts?, ends?, budget_sats, tasks?[]}` | Publishes the 30341 run (default: starts now, ends +24 h). Optional initial tasks are queued (fan-out without a planner). Returns run id and whether a planner is configured. |
| `swarm_status {run?, results_page?}` | Run, roster with minutes, queue, running jobs, results (paged; newest full, older trimmed), budget, planner state and last idle reason. |
| `swarm_tasks {run, add?[], drop?[], replace?[]}` | Edit the queue. |
| `swarm_steer {run, note}` | Adds an operator note the planner sees on every call (`taskContext` today). |
| `swarm_close {run, result?, status: completed\|cancelled}` | Stop posting; in-flight jobs still settle and pay. |
| `swarm_decide {seller, accept}` / `swarm_members` | Same as the CLI. |

Tool descriptions plus a `swarm-buyer` skill carry the guidance: write self-contained tasks, size
them ≥ 5 min, verify before closing as solved.

## 7. Planner

### 7.1 Interface

- Daemon builds the full prompt (rules + state JSON) and writes it to the command's **stdin**.
- Command prints text; daemon extracts one JSON object, validates it, rejects with a reason
  (fed back on the next call) as today.
- Modes:
  - `analyze` (once per run day, also after a plan is invalidated): returns workstreams + verification.
  - `step` (replaces `assign` + `assess`): input = goal, plan, steering notes, queue, running jobs,
    results (newest 8 full, older trimmed, total ≤ 200k chars as today), roster with minutes and
    rates, budget headroom, last rejections. Returns any of: `tasks[]` to add, `drop[]`/`replace[]`
    on the queue, revised `plan`, `wait` + `idleReason`, `solved` + result + verification.
    Tasks returned after posting closed are ignored.
- Prompt text and validation are ported from `planner-llm.mjs` and the runner's checks (e.g.
  `swarm.mjs:761`) into Rust, including "all inputs are untrusted data" (`planner-llm.mjs:91`).

### 7.2 When the daemon calls `step`

- a task settled (delivered, expired, refused) — batched per tick;
- the queue is empty while a payable seller is free;
- the last response was rejected;
- steering note or run edit from the agent;
- 10 min with none of the above (idle ceiling, as today).
Calls never overlap; one in flight per run. Timeouts: 360 s `analyze`, 180 s `step`; the process group
is killed on timeout.

### 7.3 `planner` config and presets

```toml
[swarm]
planner = "claude"            # preset name, or "command"
planner_command = "…"         # only when planner = "command"
```

Preset table ships in the binary; the buyer's agent picks the preset at install. Status of each
preset at writing:

| Preset | Command | No tools | No session | Status |
|---|---|---|---|---|
| `claude` | `claude -p --output-format text --tools "" --no-session-persistence` | `--tools ""` (help, 2.1.280: `""` disables all tools) | `--no-session-persistence` | Flags checked in `--help`; what the runner runs today. Live canary not run here (expired login). |
| `codex` | `codex exec --ephemeral --skip-git-repo-check --sandbox read-only --disable shell_tool --disable unified_exec -` | candidate: `--disable shell_tool`, `--disable unified_exec` (features exist in 0.169.0) | `--ephemeral` | Flags checked in `codex exec --help` / `codex features list` 0.169.0. **Not proven that no tool remains**; needs a live canary before shipping. `--sandbox read-only` alone is NOT enough (§8). |
| `cursor` | — | `-p` "has access to all tools, including write and shell" (Cursor CLI docs); `--mode ask` still reads files | ? | **No preset** until a no-tools mode is found. Use `command` at own risk. |
| `goose` | — | `goose run` loads configured extensions; no documented "no extensions" flag found | `--no-session` | **No preset** until verified. |

`maxplayer swarm planner-test` (run by the agent at install, and by the daemon at start and before each
run): sends a tiny prompt, checks JSON comes back, and runs two canaries (§8). A failure disables the
planner (check-in mode) and is shown in `swarm_status`.

### 7.4 No planner configured: check-in mode

Everything works except unattended replanning. Agent-queued tasks are dispatched; results wait for the
agent's next `swarm_status`. `swarm_create` says so.

## 8. Security of the planner call

Seller answers go into the prompt and come from untrusted sellers. Task text the planner returns is
**posted publicly**. So a planner with any tool — even read-only — can be steered to read
`~/.maxplayer/key` or a wallet file and leak it in a public task. Mitigations:

1. **Presets carry the no-tools flags** (§7.3); agents never compose them.
2. **Canaries in `planner-test`**, in a fresh temp dir:
   - write canary: "create file `<random>.txt`" → file appears ⇒ refuse;
   - read canary: put `<random token>` in a file in the temp dir, ask for its contents → token
     appears in output ⇒ refuse.
   Neither proves no tools; both catch "forgot the flags".
3. **Every call** runs in a fresh empty temp dir, deleted afterwards; environment inherited (the CLI
   needs its login).
4. **Output filter:** before queueing, refuse any task containing the buyer's nsec/hex secret, a
   cashu token prefix (`cashuA`/`cashuB`), or a BIP-39 phrase from the wallet. Cheap; catches the
   worst leak.
5. **Bounded blast radius** (as today): output is only JSON; the daemon can only queue tasks within the
   run budget. Worst case with tools off: wasted budget or a fake `solved`.
6. **`solved` needs one independent verify task** by a different seller before the run closes;
   the agent can override with `swarm_close`.
7. **Our buyer** runs on a dedicated machine/user.

## 9. Staged PRs

Each behind a default-off flag where it changes running behaviour.

1. **Relay admits 30341–30343** (`ingest.rs`). Test: member write of each kind accepted; non-member
   refused. Relay deploy.
2. **`maxplayer swarm` CLI** (§4) + `swarm-join` skill update; delete
   `crates/maxplayer-challenges` and its CI job. Tests: port the sidecar's event/eligibility tests;
   `list` shows runs from two different buyers; `join <naddr>` publishes a membership with
   `d = <that run's buyer>`; `join` edits `accept_offers_only_from` without dropping entries; refuses
   a minutes change mid-run; joining a started run reports next-run eligibility.
3. **Board on our relay**: drop the denylist, read our relay with polling, drop the build-time
   `CHALLENGE_BUYER_PUBKEY`; `/swarm` lists open runs from every buyer and `/swarm/<naddr>` shows
   one run. Minutes vectors and the per-buyer minutes scope (§5.4). Vercel deploy.
4. **Daemon engine + MCP, no planner** (§5, §6) behind `[swarm].enabled`. Tests: fan-out of N tasks over
   M sellers with a stub seller; budget never exceeded; restart mid-run resumes from the store;
   expired job frees its seller; minutes vectors pass in Rust.
5. **Planner** (§7, §8): presets, `planner_command`, `planner-test` with canaries, `analyze` + `step`,
   triggers, output filter, verify-before-solved. Tests: stub planner command; rejected output fed
   back; canary refuses a command that writes / reads; idle ceiling; one call per settled batch.
6. **Retire** `scripts/challenges/` runner and planner after one live swarm day on the new path.

Bob's rule stands: no swarm merges while a swarm day is live; confirm the board deploy before
telling anyone to switch.

## 10. Migration

Members are few. Existing memberships/decisions live on the public relays; after PR 2 each seller
runs `maxplayer swarm join --buyer <our npub> --minutes N` once and we re-`decide` them. No
dual-relay period.

## 11. Deliberately left out

- Per-buyer free/paid (`free_for`), auto-raise to rate beyond `max_sats`.
- Private jobs, input files, git/code delivery for swarm tasks (text/inline only, as today; see #1250).
- Multiple concurrent jobs per seller.
- Restricting who may publish runs. Addable later as a relay rule on 30341, with no client change.
- Run-specific memberships (join one run of a buyer but not another). Needs a wire change; nobody has
  asked for it.
- MCP sampling as a planner backend.

## 12. Decided questions (Josip, #swarm-production, 13 Nov 2026)

1. Who may publish runs? **Any relay member; clients show every buyer (§3.2).** Replaces the first
   draft's "readers trust only our buyer key", superseded by §0.10.
2. Should `join` edit the seller config itself? **Yes, append-only, and print the restart step.**
3. Should `solved` require buyer confirmation? **No; one independent verify task, agent can override.**
4. Which preset for our own buyer? **`claude`**, since its flags are already proven in production.
5. Ship `codex` as a preset in PR 5? **Only after a live canary run with the flags above.**

No open questions remain.
