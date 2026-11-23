# Hook agents — a seller's own agent behind a command

**Status:** design spec. No code written. **Anchor commit: `4caf510cfa9424331c3810ead5bf665d18afa9ca`
(= `origin/main` at the time of writing, 23 Nov 2026).** Every line number below was re-derived by
`git grep -n` at that commit and is written `file:line @4caf510`. Paths are abbreviated:

| Prefix | Path |
|---|---|
| `core/` | `crates/maxplayer-core/src/` |
| `cli/` | `crates/maxplayer/src/` |
| `ppp/` | `crates/buzz/crates/maxplayer-private-protocol/src/` |

**Scope.** A seller runs an agent of their own choosing, on their own machine, outside our sandbox,
and serves Maxplayer jobs with it. The daemon hands each job to a **hook**: a command the seller
configures. The daemon runs it once per turn, writes the job as JSON to its stdin, waits for it to
exit, and delivers what it printed. Everything else about the job — offer, review, claim, award,
payment, settlement, sessions — is the machinery that runs today.

---

## 0. Settled inputs — not relitigated here

From the design discussion of 20–23 Nov 2026:

1. **No new job type.** A hook serves the existing inline (non-contribution) jobs: offers carrying
   `["param","accepts-delivery","inline"]`. Nothing changes on the wire; a buyer cannot tell a hook
   job from any other inline job.
2. **The seller owns their safety.** No answer scan, no content filter, no control over which tools
   the agent uses. The daemon only sees text in and text out.
3. **Config only.** A hook is a custom agent preset, `[agents.<name>] hook = [...]`, like the
   existing `[agents.<name>] argv = [...]`. There is no `--hook` flag.
4. **Mixed seats.** Hook entries and container entries can be listed together in `[seller] agents`
   and share the single `[seller] slots` count.
5. **Routing by eligibility.** The daemon picks the first roster entry that can serve the offer. A
   hook entry can serve only inline, non-contribution offers.
6. **The hook is a blocking call.** Same shape as running the harness today. No seller socket, no
   `seller deliver`/`seller release` command. An asynchronous agent waits inside its adapter.
7. **Stdout is always JSON**, `{"status": ..., "answer": ...}`.
8. **Self-reported family/model from config only**, optional. Absent ⇒ the claim carries no
   family/model for the hook (the existing "absent stays absent" rule).
9. **Health is the simplest thing that works:** a ping at boot and on each heartbeat tick, and job
   failures counted through the existing harness strikes. Whether a ping calls the model is the
   adapter's choice.
10. **Sessions work out of the box.** Each turn is another blocking call with the same `job_id` and
    a higher `turn`. The adapter keeps the conversation. A best-effort `close` event ends it.
11. **Shipped with it:** a published `seller-hook` skill page that documents the contract (HTTP
    agents are mentioned, with no example), a per-turn `claude -p` adapter, a Claude Code channel
    adapter, and `maxplayer seller hook test`.

---

## 1. What exists today

- **Roster.** `[seller] agents` lists preset names (`core/home.rs:236 @4caf510`). Each resolves
  through `resolve_agent_preset_in` (`core/agent_presets.rs:94 @4caf510`), which checks custom
  `[agents.<name>]` presets first, then the built-ins. A custom preset is `AgentPresetConfig { argv }`
  (`core/home.rs:1658 @4caf510`, `deny_unknown_fields`). The result is a `RegisteredAgent { name,
  argv }` (`core/seller_agents.rs:51 @4caf510`) wrapped by `LiveRoster`, whose `dispatch(requested)`
  (`core/seller_roster.rs:424 @4caf510`) returns the first serving entry, or the serving entry with
  exactly the requested name.
- **Claim gate.** `classify_offer` (`core/seller_node/run.rs:2945 @4caf510`) refuses an offer the
  roster cannot serve by NAME only (`run.rs:3015 @4caf510`). Contribution offers are gated earlier,
  by `[seller] contribution_enabled` (`run.rs:8035 @4caf510`, `core/home.rs:325 @4caf510`). Nothing
  gates on `accepts-delivery`: a seat claims git-only offers and picks the transport at execution.
- **Claim advertising.** The claim lists every serving name (`run.rs:8598 @4caf510`) plus the
  roster's filterable families/models (`core/heartbeat.rs:555 @4caf510`, family derived from the
  name by `harness_family_for_preset`, `core/agent_presets.rs:67 @4caf510`). `["session","1"]` is
  added only on a docker container-delivery seat whose runsc gate allows it
  (`core/session.rs:264 @4caf510`). The private dispatch block names the entry the claim expects to
  run (`run.rs:8622 @4caf510`).
- **Execution.** `execute_job` dispatches AGAIN from the stored `requested_agent`
  (`run.rs:9662 @4caf510`), then branches on container delivery (`run.rs:9708 @4caf510`). An answer
  is published by `deliver_inline` (`run.rs:11027 @4caf510`): size cap `INLINE_ANSWER_MAX_BYTES`
  (`run.rs:3906 @4caf510`, 4 KiB minus the marker line; protocol cap `ppp/lib.rs:19 @4caf510`),
  receipt over the answer digest, private binding, result event. Failures go through
  `fail_job_with_feedback` (`run.rs:11892 @4caf510`).
- **Inline-only fallback.** Right after award, `seller_delivery_budget`
  (`core/review/delivery_budget.rs:8 @4caf510`) reserves review space for a private delivery. If the
  inline reservation is refused permanently, inline is dropped and the agent must deliver through
  git (`run.rs:9612 @4caf510`). It needs the award: it reads the claim/award selection
  (`delivery_budget.rs:27 @4caf510`), so it cannot run before the claim.
- **Inputs.** Before the claim, a private offer's attachments are fetched, checked against the
  signed manifest and pinned as `refs/heads/input-baseline` in a bare repo at `input_cache`
  (`core/seller_node/privacy.rs:257 @4caf510`). Execution reads that pin (`privacy.rs:490 @4caf510`)
  and bind-mounts it read-only into the container (`run.rs:10611 @4caf510`). `inputs::materialize`
  (`core/private_content/inputs.rs:138 @4caf510`) writes manifest-listed regular blobs into a fresh
  directory (0700 dirs, 0600 files, `O_NOFOLLOW`, `create_new`).
- **Health.** A failed run faults the entry (`core/seller_roster.rs:463 @4caf510`): dropped with a
  backoff, re-probed by `start_due_harness_probes` (`run.rs:9180 @4caf510`), restored only by a
  passing probe (`seller_roster.rs:497 @4caf510`). The boot probe narrows the roster before the
  first beat (`run.rs:9249 @4caf510`). The beat says `accepting=y` while any entry serves
  (`run.rs:7571 @4caf510`).
- **Sessions.** A turn ≥ 2 runs through `run_turn` (`core/seller_node/session_run.rs:522 @4caf510`):
  payment gate, slot, session record, holder gate, then `resume_turn`, which requires container
  delivery (`session_run.rs:851 @4caf510`). Results go through `publish_turn_result`
  (`session_run.rs:1175 @4caf510`), which already has an inline arm (`session_run.rs:1484 @4caf510`).
  Sessions end through `session_gc::end_session` (`core/seller_node/session_gc.rs:871 @4caf510`),
  which removes the container.

---

## 2. Config

```toml
[seller]
agents = ["claude", "my-hook"]      # container entry first, hook second; any order, any mix

[agents.my-hook]
hook = ["/home/me/bin/mp-claude-hook"]  # argv array; a bare string is refused, as for argv
family = "claude-code"                   # optional, self-reported; must be in HARNESS_FAMILIES
model = "claude-opus-5"                  # optional, self-reported; requires family
```

- `AgentPresetConfig` gains `hook: Option<Vec<String>>`, `family: Option<String>`,
  `model: Option<String>`; `argv` becomes optional. Exactly one of `argv`/`hook` must be present
  (parse error otherwise). `family`/`model` are refused on an `argv` preset (an ACP harness reports
  its own model).
- `family` must be one of `HARNESS_FAMILIES`; `model` without `family` is refused. Both are
  self-reports and are documented as such.
- A hook entry may be named after a built-in (`[agents.claude] hook = [...]`): custom presets
  already shadow built-ins, so buyers who request `harness=claude` reach it, and its family is
  derived from the name as for any `claude` entry. A stated `family` that contradicts a built-in
  name is refused.
- `RegisteredAgent` gains `kind: AgentKind`, `Acp` (today) or `Hook { family, model }`. The hook
  argv is stored in `argv`.
- `maxplayer seller serve --agent my-hook` works for a first run because `--agent` already resolves
  custom presets.

**Boot checks for a hook-only seat** (every entry is a hook): the sandbox and nix readiness checks
(`cli/doctor.rs:3093 @4caf510`) report "not applicable: hook-only seat" instead of failing. Nothing
of ours runs code for a hook entry, so neither check describes a requirement. A mixed seat keeps
every check as today.

---

## 3. Routing

`LiveRoster::dispatch` and `serves` take a second argument, the offer's shape:

```rust
pub struct OfferShape { pub inline: bool, pub contribution: bool }
```

- An `Acp` entry is eligible for every shape (today's behavior).
- A `Hook` entry is eligible iff `inline && !contribution`.
- `requested = None` ⇒ the first serving, eligible entry. `requested = Some(name)` ⇒ the serving
  entry with that name, which must also be eligible; otherwise `None` and the seller declines.
  Never substituted, as today.

Callers:

- `classify_offer` builds the shape from `offer.accepts_inline_delivery()`
  (`core/gateway.rs:490 @4caf510`) and the contribution pin the claim path already resolved
  (`run.rs:8035 @4caf510`). A new `SkipReason::NoEligibleAgent` names the case "the requested agent
  exists but cannot serve this offer" apart from `AgentUnavailable`.
- The capacity re-drive (`capacity_retry_offer`, `run.rs:2516 @4caf510`) reads the contribution
  pin back from the store (`contribution_pin`).
- **Claim-time pin.** The entry chosen at claim is stored on the claim row (new nullable column
  `claims.agent_label`). `execute_job` and session turns dispatch by that label when present
  (exact name, no eligibility recheck: eligibility was decided against the same offer). A NULL
  label (rows from before this change, or the unlabelled `--agent-argv` hatch) uses today's
  `dispatch(requested_agent)`. Routing therefore cannot change between claim and execution.

### 3.1 Correction to the discussion: the delivery-budget check stays after the award

The discussion assumed the inline review-space check could move before the claim. It cannot: it
signs a review request over the claim and the award (`delivery_budget.rs:27 @4caf510`). It still
runs at the same point. When it drops inline for a job routed to a hook entry, the job fails with
`delivery_failed` (no strike: the hook never ran). The buyer's reservation is released at once by
the existing failure feedback. It fires only for private jobs whose inline reservation cannot be
sealed, a permanent refusal that a git-capable entry handles today by delivering through git.

---

## 4. Claim advertising

- The claim's `agent` tag lists the serving names, as today. The buyer's award filter already
  matches a requested harness against that list.
- Families/models: a hook entry contributes its configured `family`/`model` (or the family derived
  from a built-in name). `SeatCapability::from_roster` gets the family per entry from one helper,
  `entry_family(entry)`, instead of `harness_family_for_preset(name)`, so the beat, the claim and
  the private dispatch block (`run.rs:8622 @4caf510`) agree.
- A hook entry with neither ⇒ no family/model tags for it. Buyers filtering on family/model do not
  match it; buyers that don't filter can award it.
- `["session","1"]`: when the entry chosen for THIS claim is a hook, the claim advertises sessions
  iff `[sessions] enabled`. The docker/runsc conditions are about parking containers and do not
  apply. A container entry keeps today's rule.

---

## 5. The hook contract (v1)

The daemon runs `hook` argv with no shell, writes ONE line of JSON to stdin, closes stdin, and
reads stdout and stderr. Every event carries `"v": 1`. A hook must ignore unknown fields and exit
non-zero on an unknown `event`.

### 5.1 `job` — one call per turn

```json
{"v":1,"event":"job","job_id":"<hex>","turn":1,"session":true,
 "prompt":"<the buyer's task, or the follow-up text for turn ≥ 2>",
 "output":"text/markdown","inputs_dir":"/abs/path or null","deadline_unix":1795200000}
```

- `prompt` is the buyer's own text, verbatim: the offer task for turn 1, the follow-up for turn n.
  None of the container prompt (git remote, markers, sentinel instructions) applies.
- `session` is true when more turns may follow (the claim advertised sessions).
- `output` is the offer's requested output type, when stated.
- `inputs_dir`: see §6. `null` when the job has no input files.
- The working directory is the per-job directory (§6).

**Answer on stdout**, exit 0:

```json
{"status":"done","answer":"..."}
```

`status` is `done` | `needs_input` | `out_of_time`. `needs_input` is a question for the buyer on a
session turn, delivered like an answer with `turn_status=needs_input`. On a single-turn job it is
delivered as an ordinary answer, since there is no next turn.

**Outcomes:**

| Hook result | Daemon action | Reason code | Strike |
|---|---|---|---|
| exit 0, valid JSON, non-empty `answer` ≤ cap | `deliver_inline` (turn 1) / inline `publish_turn_result` (turn n) | — | — |
| exit 0, answer over `INLINE_ANSWER_MAX_BYTES` | fail, never truncated | `delivery_failed` | no |
| exit 0, stdout not the JSON above, empty answer, unknown status | fail | `execution_failed` | yes |
| non-zero exit, killed by signal | fail; stderr tail logged | `execution_failed` | yes |
| still running at `deadline_unix − 30 s` | kill process group, fail | `execution_failed` | no (job clock, as `DeadlineExceeded` today) |

- The answer cap is today's inline cap (§1). No marker is involved, so the hook path checks the
  same constant `deliver_inline` checks.
- Stdout is read up to 1 MiB; more is a contract violation. Stderr's last 4 KiB go to the seller
  log, never to the buyer.
- The 30 s publish margin leaves time to sign and publish before the offer deadline.
- The process runs in its own process group; a kill reaches every child.
- Environment: the daemon's environment minus every `MAXPLAYER_*` variable (config overrides may
  carry credentials), plus `MAXPLAYER_HOOK_V=1`.

### 5.2 `ping`

```json
{"v":1,"event":"ping"}
```

Exit 0 within 30 s = healthy. Stdout is ignored. What a ping checks is the adapter's decision
(a socket, a login, a tiny model call); the contract only promises pings arrive periodically.

### 5.3 `close`

```json
{"v":1,"event":"close","job_id":"<hex>"}
```

Sent once when a hook session ends (buyer close, expiry, eviction). Best effort: 30 s timeout,
exit status ignored, never holds a slot or fails anything. A crash or restart can end a session
with no `close`, so an adapter must not rely on it as its only cleanup.

---

## 6. Inputs and the per-job directory

- The pre-claim fetch, verification and pin are unchanged (§1).
- At execution of a hook job, the daemon creates the per-job directory
  `job_workdir(home, job_id)` (`<home>/seller-jobs/<job_id>`), mode 0700, and, when the job has
  inputs, `materialize`s the pinned manifest into `<dir>/inputs`. `inputs::materialize` already
  refuses traversal, links, duplicate or overlapping paths, size/sha mismatches and an existing
  destination, and writes 0600 files with no exec bit.
- The pinned manifest is read from the stored task content the claim path already authenticated,
  never refetched.
- The hook's cwd is `<dir>`; `inputs_dir` is `<dir>/inputs` or `null`.
- The directory is deleted when the job ends: after the result is journaled or the job failed
  (single-turn), or when the session ends (session). The input cache itself keeps today's lifetime.

---

## 7. Execution (turn 1)

`execute_job` resolves the entry (claim-time label, §3). For a `Hook` entry it calls a new
`execute_hook_job` and returns, before the container branch (`run.rs:9708 @4caf510`). Steps:

1. `assign_agent` journals the label; `mark_executing`, as today.
2. The delivery-budget check (§3.1).
3. Stored creq read, as today.
4. Per-job directory and inputs (§6).
5. Run the hook (§5.1) on a blocking task, bounded by the deadline.
6. Map the outcome (§5.1 table). A delivered answer uses `deliver_inline` with exec metadata from
   `seller_exec_metadata` (`core/seller_exec.rs:2575 @4caf510`): `harness` = the entry label,
   `usage_transport` = `hook`, `metadata_trust` = `seller-claimed`, `model` = the configured model
   when set, wall time measured by the daemon.
7. Session (claim advertised it, `[sessions] enabled`): after a successful delivery, record a
   session (§8). `deliver_inline` appends `turn_status` when the status is not `done`, through the
   same `append_result_tags` the git path uses.
8. Single-turn: delete the per-job directory.

Strikes: an `execution_failed` hook outcome faults the entry with `Fault::Unproven`, exactly as a
dead harness is faulted today (`seller_roster.rs:463 @4caf510`). The deadline kill uses the
existing `DeadlineExceeded` no-op.

---

## 8. Sessions

- **Record.** A hook session is a `SessionRecord` with `container = ""`, `holder = None`,
  `harness = <label>`, `acp_session_id = None`, and a new column `sessions.kind` = `hook`
  (existing rows read as `container`). Lifetime, close and restart bookkeeping are unchanged.
- **Turn n.** `run_turn` keeps the payment gate, slot wait, start margin and session lookup. For a
  `hook` record it skips the holder gate (nothing to verify) and calls `run_hook_turn`:
  1. The follow-up review exactly as `resume_turn` builds it today, awaited before anything runs.
     A refusal withholds the turn (refused, unpaid), as today.
  2. Run the hook with `turn = n`, `prompt = the follow-up text`, `inputs_dir` from turn 1.
  3. Publish through `publish_turn_result` with `TurnDelivery::Inline` and the hook's status.
  4. Failures refuse the turn with the §5.1 reason codes; the session stays parked.
- **End.** `end_session` skips every docker call for a `hook` record, deletes the per-job
  directory, and the caller sends `close` (§5.3) off the loop. The GC's disk measurement skips hook
  records (nothing on disk is ours except the inputs). The GC's lifetime expiry ends hook sessions
  through the same path.
- **Restart.** `recover_sessions` has nothing to stop for a hook record; unfinished turns are
  re-driven as today.

---

## 9. Health

- **Boot.** The pre-advertise probe runs a `ping` for each hook entry instead of an ACP turn.
  Failing entries are dropped through `narrow_roster_to` (`run.rs:9249 @4caf510`). A seat where
  nothing serves refuses to boot, as today.
- **Running.** On each heartbeat tick, every SERVING hook entry is pinged off the loop (one
  in-flight ping per entry). A failed ping faults it (`Fault::Unproven`); the beat then advertises
  without it, and `accepting=n` if nothing else serves.
- **Restore.** A dropped hook entry's due probe (`run.rs:9180 @4caf510`) is a ping. A passing ping
  restores it. Same backoff, same strike history as a harness.
- No new timer, no new counter.

---

## 10. What ships alongside

- `web/app/.well-known/skills/seller-hook/skill.md`: the contract in §5–§6 written for agents,
  with the two adapters below as examples, the self-reported nature of family/model, the seller's
  own-safety deal, and one sentence that an HTTP agent is reached through a command. Linked from
  the skills index and `llms.txt` like the other pages.
- `examples/hooks/claude-per-turn.sh`: `claude -p` for turn 1, `claude -p --resume <id>` for later
  turns, the Claude session id kept in a per-job state file, removed on `close`. Its ping runs
  `claude --version`.
- `examples/hooks/claude-channel/`: one script with two modes.
  - `serve`: the MCP channel server Claude Code starts over stdio. It listens on a unix socket in
    its own state directory, pushes each job into the running session as a
    `notifications/claude/channel` event (`job_id`, `turn`, `inputs_dir` as meta), and exposes an
    `answer(job_id, status, text)` tool.
  - `push`: the hook. It sends the job to the socket and blocks until Claude calls `answer`, then
    prints the JSON and exits 0. No socket ⇒ non-zero (no session running). Ping = the socket
    answers.
  Channels are a Claude Code research preview; the page says so.
- `maxplayer seller hook test <entry>`: sends `ping`, a `job` turn 1 with a sample input file, a
  `job` turn 2 with the same id, and `close`; prints each result and the verdict the daemon would
  reach. No relay, no money.
- `docs/SELLER-QUICKSTART.md`: a short "Hook agents" section pointing to the skill page.

---

## 11. Implementation — three PRs, no feature flag

A hook entry only exists when a seller configures one, so absent config every path is today's.

**PR 1 — single-turn hooks.** §2, §3, §4 (without sessions), §5.1–5.2, §6, §7, §9.
Tests (each red on revert of its piece):
- Config: `hook` + `argv` both/neither refused; `model` without `family` refused; unknown family
  refused; `family` on an `argv` preset refused; a bare-string hook refused.
- Routing: a hook-only roster skips a git-only offer and a contribution offer
  (`NoEligibleAgent`); a mixed roster `[hook, claude]` routes a git-only offer to `claude` and an
  inline offer to the hook; a named request for the hook on a git-only offer declines.
- Claim pin: a claim routed to the hook executes on the hook after the roster order changes; a
  NULL-label row dispatches as today.
- Advertising: hook family/model reach the claim's filterable tags and the beat; absent stays
  absent.
- Execution, against a stub hook script: done ⇒ inline result with the answer digest receipt;
  over-cap ⇒ `delivery_failed`, nothing published; bad JSON / non-zero ⇒ `execution_failed` +
  strike; deadline ⇒ process group killed (a grandchild too), no strike; stdin carries the task
  verbatim; `MAXPLAYER_*` absent from the hook's environment.
- Inputs: a private job's files appear under `inputs_dir`, 0600, cwd is the job dir; the dir is
  gone after delivery.
- Health: boot ping failure drops the entry; heartbeat ping failure flips `accepting=n` on a
  hook-only seat; a passing due ping restores it.
- Doctor: a hook-only seat passes without sandbox/nix; a mixed seat still requires them.

**PR 2 — hook sessions.** §4 session advertising, §5.3, §8.
Tests: claim advertises sessions for a hook entry with `[sessions] enabled` on a seat with no
docker; turn 2 runs the hook with `turn=2` and the follow-up text after the review passes; a
refused review runs nothing; `needs_input` reaches the result tags; close/expiry send `close` and
delete the dir with no docker call; a hanging `close` does not hold a slot.

**PR 3 — docs, adapters, `seller hook test`.** §10. The adapters are exercised by
`seller hook test` in CI against a fake `claude` on PATH.

---

## 12. Risks

- **Self-reported identity.** A hook's family/model are whatever the seller wrote. Today's tags
  are self-reported too but come from the harness's own report. Buyers who need more must choose
  sellers by reputation.
- **The 4 KiB inline cap.** Answer-mode jobs inherit it. A long answer fails `delivery_failed`
  instead of being cut. Raising the cap is a protocol constant change (`ppp/lib.rs:19 @4caf510`)
  and is out of scope here.
- **Shared context across buyers.** A long-lived agent behind one hook can carry buyer A's prompt
  into buyer B's answer. The skill page tells sellers so; the daemon cannot prevent it.
- **Host execution.** Buyer text and files reach the seller's own agent on the seller's own
  machine. That is the point of the feature and the seller's choice.

---

## 13. Deliberately left out (each addable later without a wire change)

- A `--hook` CLI flag.
- Native HTTP hooks (a command can forward to any HTTP agent).
- Ping-reported family/model.
- Per-turn model reporting in the stdout envelope.
- Separate slot counts per entry kind.
- A git delivery path for hooks (files written by the agent are not delivered).
- Delivering input files to a hook on another machine.

---

## 14. Open questions (with proposed defaults)

1. **Publish margin before the deadline.** Default 30 s (§5.1).
2. **Should a hook-only seat still advertise `nix` capability tokens?** Default: no capability
   probe runs for a hook-only seat, so it advertises none and buyers requiring capabilities do not
   match it.
