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
   `["param","accepts-delivery","inline"]`. No new event kinds or tags; the only new public values
   are `usage_transport=hook` on the result and, on a session turn 1, a `turn_status` other than
   `done` on an inline result (§7). See §5.0 for what "inline job" means given that buyers declare
   `inline` by default.
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
   family/model for the hook (the existing "absent stays absent" rule). A hook named after a
   built-in (`[agents.claude] hook = …`) is a deliberate exception: buyers who ask for that harness
   by name reach it, so it carries that name's family (§2).
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
- First run with `--agent my-hook`: the first-run path today writes the resolved argv into
  `[seller] agent_command` (the unlabelled ACP fallback). For a hook it must not: it writes
  `agents = ["my-hook"]` and leaves `agent_command` as it was.
- `hook = ` in config makes an older binary refuse the home (`deny_unknown_fields`); the release
  notes say so.

**Boot checks for a hook-only seat** (every entry is a hook): the sandbox and nix readiness checks
(`cli/doctor.rs:3093 @4caf510`) report "not applicable: hook-only seat" instead of failing. Nothing
of ours runs code for a hook entry, so neither check describes a requirement. A mixed seat keeps
every check as today.

---

## 3. Routing — ONE selection per claim

A new `LiveRoster::select(offer: &OfferShape) -> Option<Selected>` replaces the name-only
`serves` in the claim decision. `OfferShape` carries what eligibility needs:

```rust
pub struct OfferShape<'a> {
    pub inline: bool,                       // offer.accepts_inline_delivery()
    pub contribution: bool,                 // a contribution pin was resolved for this offer
    pub request: &'a CapabilityRequest,     // requested agent / family / model / capabilities
}
```

An entry is a **candidate** iff it is serving and:

- its kind admits the shape: an `Acp` entry admits every shape; a `Hook` entry only
  `inline && !contribution`;
- a requested agent name (normalized as today, `core/seller_agents.rs:179 @4caf510`) equals its
  name;
- the offer's family/model/capability request is met by **that entry's own** capability (its
  family, its model, the seat's measured tokens for an `Acp` entry, none for a `Hook` entry), using
  the same `capability_meets_request` the claim gate calls today.

`select` returns the first candidate in roster order; none ⇒ the seller declines with a new
`SkipReason::NoEligibleAgent` ("no configured agent can serve this offer's shape"), distinct from
`AgentUnavailable` ("the named agent is not serving").

This also fixes a latent multi-harness bug: today the claim gate judges the request against the
whole roster (`run.rs:3019 @4caf510`) while execution runs the first entry, so a `family=codex`
request on a `[claude, codex]` seat passes on codex's strength and runs on claude.

**The selection is the single source** for everything the claim says and does:

- the claim's filterable tags and the private dispatch block (§4);
- the claim pin, written in the SAME transaction as the claim row and its outbox draft
  (`claim_and_enqueue`, `core/seller_node/store.rs:1691 @4caf510`): new nullable columns
  `claims.agent_label`, `claims.agent_kind` (`acp` | `hook`) and `claims.agent_fingerprint`
  (sha256 of the entry's kind + argv/hook + family + model).

**Slot before projection.** Today the private claim intent is prepared (`run.rs:8628 @4caf510`)
before the slot reservation (`run.rs:8635 @4caf510`), so a capacity skip leaves a prepared claim
intent whose dispatch block a later re-drive could contradict. The reservation moves first and is
released on every non-claim exit, as `release_on_no_claim` already does. A re-drive then always
prepares the intent for the first time with its own selection.

**Execution reads the pin.** `execute_job` (`run.rs:9662 @4caf510`) and session turns
(`core/seller_node/session_run.rs:855 @4caf510`) dispatch by the pinned label and verify kind and
fingerprint against the live roster. A missing or changed entry fails the job closed with today's
`capability_missing` (nothing ran). A row with no pin (claimed before this change, or the
unlabelled `--agent-argv` hatch) dispatches as today but **only over `Acp` entries**, so a hook
added later can never pick up work that never admitted hooks.

The capacity re-drive (`capacity_retry_offer`, `run.rs:2516 @4caf510`) rebuilds the shape from
the stored offer row and the stored contribution pin (`store.rs:1597 @4caf510`).

### 3.1 Correction to the discussion: the delivery-budget probe stays after the award

The discussion assumed the inline review-space check could move before the claim. It cannot. It
is a size probe (`core/review/delivery_budget.rs:8 @4caf510`) built over the claim/award selection
(`delivery_budget.rs:27 @4caf510`), so it runs where it runs today, right after the award. For a
job pinned to a hook entry, before the hook is spawned:

- `Ok(true)` ⇒ run the hook.
- `Ok(false)` (inline cannot be reviewed; only git could) ⇒ fail `delivery_failed`, no strike.
- `Err(_)` (selection or probe unavailable) ⇒ fail `delivery_failed`, no strike, exactly the arm
  that refuses today (`run.rs:9620 @4caf510`).

The buyer's daemon releases its reservation when it ingests the awarded seller's error feedback
(`core/buyer/mod.rs:3335 @4caf510`). This fires only for private jobs whose inline answer cannot be
sealed for review, which a git-capable entry handles today by delivering through git.

---

## 4. Claim advertising

- **The claim describes the selected entry only.** Its `agent` tag is the selected entry's name
  (none for the unlabelled hatch), and its filterable family/model/capability tags are that
  entry's. The kind-30340 beat stays seat-wide (every serving entry), as today. A buyer's award
  filter reads the claim (`core/gateway.rs:830 @4caf510`), so it judges exactly what will run.
- Family per entry comes from one helper, `entry_family(entry)`: the configured `family`, else
  the built-in name's family (`builtin_family_for_name`), else none. `SeatCapability::from_roster`
  and the private dispatch block (`run.rs:8622 @4caf510`) both use it.
- A hook entry with no family ⇒ no family/model tags. Buyers filtering on family/model do not
  match it; buyers that don't filter can award it.
- **Buyer-side limit, unchanged here.** The buyer refuses a `model` request without a harness
  preset, and a family/model request whose preset has no built-in family
  (`core/buyer/lifecycle.rs:311`, `:325 @4caf510`). So a hook named `my-hook` can be awarded by
  name, or by `harness_family` alone; a `harness` + `model` request matches a hook only when the
  hook is named after the built-in (`[agents.claude] hook = …`). The skill page says so. A hook's
  model is read from its entry at `advertisement()` time, never from the roster's observed-model
  slot, which job and probe writers overwrite.
- `["session","1"]`: when the selected entry is a hook, the claim advertises sessions iff
  `[sessions] enabled`. The docker/runsc conditions are about parking containers and do not
  apply. A container entry keeps today's rule.

---

## 5. The hook contract (v1)

The daemon runs `hook` argv directly (no shell; `argv[0]` resolved on the daemon's `PATH`, a
relative path refused at config load), writes ONE JSON object followed by `\n` to stdin, closes
stdin, and reads stdout and stderr concurrently until the process exits. Every event carries
`"v": 1`. A hook must ignore unknown fields and exit non-zero on an unknown `event`.

### 5.0 What a hook may deliver, and the protocol amendment (review H1)

`accepts-delivery=inline` is a capability the buyer declares, **by default on every offer**
(`core/buyer/mod.rs:734 @4caf510`); git is always accepted. So "inline job" is not a job class: a
hook entry is eligible for nearly every non-contribution offer, including ones whose natural
deliverable is files. Three rules keep a hook honest about that:

1. **The agent is told the delivery contract.** The `job` event carries
   `"answer_max_bytes": <cap>` and `"files_delivered": false`, and both shipped adapters and the
   skill page pass them to the agent ("your reply is the whole deliverable; files you write are not
   delivered; keep it under N bytes").
2. **An honest exit.** `{"status":"cannot_deliver","reason":"…"}` at exit 0 fails the job
   `delivery_failed` with no strike, and the reason goes to the seller log only. The only other
   options would be a strike or a paid non-answer.
3. **Single-turn statuses.** When the claim did not advertise a session, `needs_input` and
   `out_of_time` fail the job `delivery_failed` (no strike) and publish nothing — protocol §6.4
   allows those inline cases on session turns only.

**Protocol amendment (same PR as the code).** `docs/protocol-v1.md` §6.4 requires git whenever the
job's tree is non-empty and §8.3 requires the `MAXPLAYER-ANSWER-V1` marker before any inline
delivery. A hook has no tree of ours and no agent message of ours. The amendment adds: *for a hook
entry, the exit-0 envelope with `status = done` is the success opt-in that stands in for the
marker; a hook job has no work tree, and files the seller's agent writes are not part of any
delivery.* The marker's purpose — never publish a vendor error as a deliverable — is carried by
the envelope: an adapter that wraps an error string as `answer` is breaking the contract, and the
shipped `claude -p` adapter uses the CLI's structured output and its error flag, never free text.

**Open for the team:** whether this is enough, or whether a hook entry should additionally be
limited to offers that are explicitly answer-shaped. The default above keeps the settled §0.1.

### 5.1 `job` — one call per turn

```json
{"v":1,"event":"job","invocation":"<hex>","attempt":1,"job_id":"<hex>","turn":1,"session":true,
 "prompt":"<the buyer's task, or the follow-up text for turn ≥ 2>",
 "output":"text/markdown","inputs_dir":"/abs/path","answer_max_bytes":4076,
 "files_delivered":false,"deadline_unix":1795200000}
```

- `invocation` identifies ONE accepted request: the job id for turn 1, the follow-up event id for
  turn n. A retry of the same request after a daemon restart carries the same value (§7.1); a new
  follow-up always carries a new one. Adapters that keep state key it by `invocation`, and an
  adapter that cannot answer the same `invocation` twice exits non-zero. `attempt` counts calls for
  the same invocation (at-least-once: a restart before the outcome was staged calls again).
- `deadline_unix` is the time by which stdout must be complete: the offer deadline (turn 1) or the
  follow-up's own deadline (turn n), minus the publish margin.
- `prompt` is the buyer's own text, verbatim. None of the container prompt (git remote, markers,
  sentinel instructions) applies.
- `session` is true when the claim advertised sessions (more turns may follow).
- `output` is the offer's requested output type, or `null` when the offer stated none.
- `inputs_dir` is an absolute path, or `null` when the job has no input files (§6).
- cwd is the per-job directory (§6).

**Answer.** On exit 0, stdout must be exactly one JSON object (UTF-8; surrounding whitespace
allowed; a duplicate key is a violation):

```json
{"status":"done","answer":"..."}
```

`status` is `done` | `needs_input` | `out_of_time` | `cannot_deliver` (§5.0; `reason` instead of
`answer`). The answer is delivered byte-for-byte as
decoded; it must contain a non-whitespace character. On a session turn `needs_input` and
`out_of_time` become the result's `turn_status` (`core/session.rs:337 @4caf510`), exactly as for a
container turn; on a single-turn job they fail the job (§5.0 rule 3).

**Outcomes:**

| Hook result | Daemon action | Reason code | Strike |
|---|---|---|---|
| exit 0, valid object, answer ≤ cap | deliver (§7) | — | — |
| exit 0, decoded answer over `INLINE_ANSWER_MAX_BYTES` | fail, never truncated | `delivery_failed` | no |
| exit 0, `cannot_deliver`; or `needs_input`/`out_of_time` on a single-turn job | fail | `delivery_failed` | no |
| exit 0, anything else on stdout (not JSON, extra keys allowed but wrong types, empty answer, unknown status, stdout > 1 MiB) | fail | `execution_failed` | yes |
| non-zero exit, death by signal | fail | `execution_failed` | yes |
| still running at the cutoff | kill group, fail | `execution_failed` | no |
| cutoff already passed when the call would start | fail without spawning | `execution_failed` | no |

**Supervision.** One runner serves `job`, `ping` and `close`:

- The child runs in its own process group. Stdin is written while stdout/stderr are drained, so a
  hook that never reads stdin or fills a pipe cannot wedge the daemon. A broken stdin pipe
  (hook exited early) is not itself an error; the exit status decides.
- Stdout is capped at 1 MiB (first overflow ⇒ kill, violation). Stderr keeps only its last 4 KiB,
  which goes to the seller log, never to the buyer.
- Cutoff (monotonic): `job` ⇒ `deadline_unix − 30 s` (time to sign and publish); `ping`/`close`
  ⇒ 30 s. At the cutoff, or on daemon shutdown: `SIGTERM` to the group, 2 s grace, `SIGKILL` to
  the group, reap the child. The child's exit is observed without reaping it (`waitid(WNOWAIT)`),
  so the group id cannot be recycled before the group kill. An exit first observed at or after the
  cutoff counts as the cutoff.
- When the hook exits, the rest of its group is killed and stdout gets 2 s to reach EOF. Stdout
  still open then (a process that left the group kept it) is incomplete: the answer is refused as a
  violation. All three pipes are non-blocking and pumped by the one supervising thread, and every
  read end is closed before the call returns, so nothing stays blocked on a pipe.
- `ping` and `close` stdout is drained and discarded: never kept, never an overflow.
- A hook that wants a long-lived worker must start it outside its own process group, with its
  stdout pointed elsewhere (that is the adapter's business, e.g. the channel adapter's `serve`, started by Claude Code, not by us).
- Environment: the daemon's environment with every `MAXPLAYER_*` variable removed (config
  overrides may carry credentials), plus `MAXPLAYER_HOOK_V=1`, which is added to the reserved
  env names (`core/home.rs:2438 @4caf510`) so a `maxplayer` command run by the agent does not
  refuse it. `MAXPLAYER_HOME` is removed too: an agent that runs `maxplayer` uses its own home.
  The hook is the seller's own program running as the seller.
- An unknown event: exit code 64 means "unsupported" and is never a strike, so new events can be
  added without breaking old adapters.
- On daemon shutdown every running hook group gets the same SIGTERM/grace/SIGKILL. The group id is
  journaled while the hook runs, and a restart kills a stale group before re-running.

### 5.2 `ping`

```json
{"v":1,"event":"ping","reason":"boot"}
```

`reason` is `boot` | `interval` | `restore`. Exit 0 before the cutoff = healthy. Stdout is ignored. cwd is the seller home. What a ping checks
is the adapter's decision (a socket, a login, a tiny model call); the skill page asks `boot` and
`restore` pings to prove the agent can answer, since a passing `restore` puts the entry back in
front of paying buyers. The shipped `claude -p` adapter makes a one-token model call for those
two.

### 5.3 `close`

```json
{"v":1,"event":"close","job_id":"<hex>"}
```

Sent once when a hook session ends (buyer close, expiry, eviction), after its directory is
removed (cwd is the seller home). Best effort: exit status ignored, never holds a slot or fails
anything. A crash or restart can end a session with no `close`, so an adapter must not rely on it
as its only cleanup.

---

## 6. Inputs and the per-job directory

- The pre-claim fetch, verification and pin are unchanged: `preflight`
  (`core/seller_node/privacy.rs:268 @4caf510`) stores verified objects in the bare repo at
  `input_cache` (`privacy.rs:257 @4caf510`) and pins `refs/heads/input-baseline`.
- At execution, the private selection is validated first (`privacy::execution`, as the container
  path does), then the authenticated manifest is read from the stored task content and
  `inputs::materialize` (`core/private_content/inputs.rs:138 @4caf510`) writes it from the cache
  into a fresh sibling directory `<dir>/.inputs-<random>`, which is renamed to `<dir>/inputs` only
  when complete. `materialize` already refuses traversal, links, duplicate or overlapping paths,
  size/sha mismatches and an existing destination, and writes 0700 dirs / 0600 files with no exec
  bit. A missing or corrupt cache fails the job `execution_failed` without spawning (no strike).
- The per-job directory is `job_workdir(home, job_id)` (`<home>/seller-jobs/<job_id>`), 0700.
  The adapter may write there; the daemon owns only `inputs/`.
- **Every call re-materializes** `inputs/` from the pin (the previous copy is removed first; a
  re-drive after a restart included, since `materialize` refuses an existing destination), so a
  hook that changed its inputs on turn 1 sees the buyer's files again on turn 2.
- The per-job directory sits under the seller home, which also holds the wallet. The hook runs as
  the seller and can read the home anyway; the skill page says to run the agent as another user
  if that matters.
- The directory is removed when the job reaches a terminal state (delivered and not a session,
  failed) or when the session ends. A restart removes the directory of any job it finds terminal.
  The input cache keeps today's lifetime.

---

## 7. Execution (turn 1)

`execute_job` reads the claim pin (§3). For a `Hook` entry it calls `execute_hook_job` and
returns before the container branch (`run.rs:9708 @4caf510`). Steps:

1. Review gate and resume decision exactly as today (`resume_action`, `run.rs:1677 @4caf510`).
2. `assign_agent` journals the label; `mark_executing`.
3. The delivery-budget probe (§3.1).
4. Stored creq read, as today.
5. Inputs (§6).
6. **Staged outcome check** (§7.1): if an outcome for this invocation is already journaled, skip
   to step 8 with it.
7. Run the hook (§5) on a blocking task; map the result (§5.1 table). A deliverable outcome is
   journaled (§7.1) BEFORE anything is signed or bound.
8. Deliver through `deliver_inline` (`run.rs:11027 @4caf510`) with the journaled answer.
   `deliver_inline` gains a `turn_status` parameter (`None` = today's callers, byte-identical) and
   returns a typed outcome (`Delivered` | `Refused(reason)`), so the hook path knows whether to
   clean up. No `fail_job*` call is ever made for a job whose delivery was journaled. Exec
   metadata: a hook variant of `seller_exec_metadata` (`core/seller_exec.rs:2575 @4caf510`) emits
   `harness` = the entry label, `usage_transport` = `hook`, `metadata_trust` = `seller-claimed`,
   `model` = the configured model when set, and the daemon-measured wall time. When the claim
   advertised a session and the status is not `done`, `turn_status` is appended through
   `append_result_tags` as the git path does (`run.rs:10330 @4caf510`).
9. Session (§8.1) or single-turn cleanup (§6).

Strikes: a strike outcome faults the entry with `Fault::Unproven`
(`core/seller_roster.rs:463 @4caf510`), as a dead harness is faulted today.

### 7.1 The staged outcome — a hook answer survives a restart

`deliver_inline` binds the private answer under the intent `answer:<job>` (or `answer:<fid>`)
before signing (`core/seller_node/privacy.rs:181 @4caf510`), and the private store refuses
different content on an existing intent. If the daemon dies after that bind and before the result
is journaled, a re-run of a nondeterministic hook would produce a different answer that can never
be bound: the job would deliver nothing.

So the validated outcome (status, answer bytes, wall time) is written to a new table
`hook_outcomes(invocation PRIMARY KEY, job_id, turn, status, answer, wall_time_ms,
created_at_unix)` in the seller store as soon as the runner returns it, before step 8. A resumed
job or re-driven turn whose invocation has a row delivers that row and never runs the hook again.
The row is deleted when the job or session ends. Once the result is journaled, today's outbox
dedup (`deliver_and_enqueue`, `core/seller_node/store.rs:1972 @4caf510`) already prevents a second
publication.

---

## 8. Sessions

### 8.1 The session exists before the result can be seen

For a session claim, turn 1's delivery and the `SessionRecord` are written together: the session
row (`kind = hook`) is inserted in the same store transaction as the delivery row and the result's
outbox draft. A follow-up therefore can never arrive for a delivered turn 1 with no session
(`core/seller_node/session_run.rs:213 @4caf510` would treat it as ended), and a failed delivery
never leaves a parked session. Implementation: `deliver_and_enqueue` gains an optional session
record argument; `put_session` (`store.rs:2144 @4caf510`) stays for the container path.

A hook session record has `container = ""`, `holder = None`, `harness = <label>`,
`acp_session_id = None`, and a new column `sessions.kind` = `hook` (existing rows read as
`container`).

### 8.2 Turn n

`run_turn` (`session_run.rs:522 @4caf510`) keeps the payment gate, slot wait, start margin and
session lookup. For a `hook` record it skips the holder gate and calls `run_hook_turn`:

1. The follow-up review exactly as `resume_turn` builds it today (`session_run.rs:801 @4caf510`),
   awaited before anything runs; a refusal (including `turn_too_large`) withholds the turn,
   unpaid, as today.
2. Dispatch by the session's pinned label and fingerprint (§3).
3. Inputs re-materialized (§6), staged-outcome check (§7.1, `invocation` = follow-up id), run the
   hook with `turn = n` and the follow-up text, journal the outcome.
4. Publish through `publish_turn_result` (`session_run.rs:1175 @4caf510`) with
   `TurnDelivery::Inline`, the hook's status and the hook metadata of §7 step 8. The private
   answer binds to the follow-up id, as today.
5. Failures refuse the turn with the §5.1 reason codes; the session stays parked.

### 8.3 End, expiry, restart

- `end_session` (`core/seller_node/session_gc.rs:871 @4caf510`) skips every docker call for a
  `hook` record, removes the per-job directory and the staged outcomes, and the caller sends
  `close` (§5.3) off the loop. Buyer close, lifetime expiry and eviction all go through it.
- Lifetime expiry today runs only on the container sweep tick, which is enabled only with a
  sandbox network (`run.rs:6378`, `:6535 @4caf510`), so on a hook-only seat sessions would never
  expire. The session GC gets its own tick whenever `[sessions] enabled`; only the docker sweep
  stays behind `sweep_enabled`. Hook records are excluded from disk eviction (they hold no
  container); their directories are bounded by the input caps and removed at session end.
- `close` is sent BEFORE the directory is removed, detached from the caller's slot, at most four
  at a time; `end_session` compares-and-sets `parked → ended` before any destructive step.
- Turn n's start margin for a hook is the publish margin only, not the container margins of
  `turn_can_start` (`core/session.rs:407 @4caf510`).
- `recover_sessions` (`session_run.rs:1433 @4caf510`) stops containers only for `container`
  records; unfinished hook turns are re-driven as today and land on §7.1.

---

## 9. Health

- **Boot.** `probe_configured_harnesses` (`run.rs:4471 @4caf510`) pings each hook entry instead
  of running an ACP turn; failures are dropped through `narrow_roster_to`
  (`run.rs:9249 @4caf510`). A seat where nothing serves refuses to boot, as today.
- **Boot gates on a hook-only seat** (every entry is a hook): the sandbox policy, token-mode gate,
  capability probe and nix probe (`run.rs:4667 @4caf510`) are skipped, and the doctor's sandbox
  and nix checks (`cli/doctor.rs:3093 @4caf510`) report "not applicable: hook-only seat". The seat
  advertises no capability tokens. A mixed seat keeps every gate, and its tokens describe its
  `Acp` entries only (§3).
- **Running.** Every serving hook entry is pinged (`interval`) off the loop on the lane tick that
  already starts due probes, every heartbeat interval, independent of whether beats are published.
  Two consecutive failed interval pings fault it (`Fault::Unproven`); the beat then advertises
  without it, and `accepting=n` if nothing else serves. A passing interval ping never restores.
- **Restore.** A dropped hook entry's due probe (`run.rs:9180 @4caf510`) is a ping; a passing ping
  restores it, with the roster's backoff and strike history. There is no lockout: like an `Acp`
  entry, a hook that keeps failing jobs keeps being re-probed on the growing backoff.
- **Races.** A ping result is applied only if the entry's state has not changed since the ping
  started (the roster already counts transitions, `seller_roster.rs:320 @4caf510`); a stale
  "healthy" never restores an entry that a later job struck, and a stale failure never strikes a
  restored one. An awarded job pinned to a currently dropped hook still runs: the award is
  committed, and the run itself is the best test.
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

**PR 1 — single-turn hooks.** §2, §3, §3.1, §4 (without sessions), §5.1–5.2, §6, §7, §9.
Tests (each red on revert of its piece):
- Config: `hook` + `argv` both/neither refused; `model` without a family refused; unknown family
  refused; `family`/`model` on an `argv` preset refused; a bare-string or empty hook refused; a
  family contradicting a built-in name refused.
- Selection: a hook-only roster skips a git-only offer and a contribution offer
  (`NoEligibleAgent`); `[hook, claude]` routes a git-only offer to `claude` and an inline offer to
  the hook; a named request for the hook on a git-only offer declines; a `family=codex` request on
  `[claude, codex]` selects `codex` (the latent bug of §3); a capability token measured for the
  `Acp` entries never makes a hook eligible.
- Claim = selection: the claim's `agent`/family/model tags and the private dispatch block name the
  selected entry only; the pin row is written in the claim transaction.
- Pin: a claim routed to the hook executes on the hook after the roster order changes; a changed
  fingerprint fails `capability_missing` without running anything; a NULL-pin row never dispatches
  to a hook.
- Capacity skip: a slot-full offer prepares no private claim intent; its re-drive selects afresh.
- Supervisor, against stub hook scripts: done ⇒ inline result whose receipt binds the answer
  digest; exact-cap and cap+1 multibyte answers; empty/whitespace answer, bad JSON, duplicate key,
  unknown status, stdout > 1 MiB ⇒ `execution_failed` + strike; a hook that never reads stdin and
  writes 1 MiB to stderr does not wedge; deadline ⇒ group killed including a grandchild, no
  strike; cutoff already past ⇒ never spawned; stdin carries the task verbatim and the
  `invocation`; no `MAXPLAYER_*` in the hook's environment except `MAXPLAYER_HOOK_V`.
- Budget probe `Ok(false)` / `Err` ⇒ `delivery_failed`, hook never spawned, no strike.
- Staged outcome: a crash after the outcome row and before the result journal re-delivers the
  same bytes and does not run the hook again.
- Inputs: a private job's files appear under `inputs_dir`, 0600, cwd is the job dir; a partial
  materialization leaves no `inputs/`; the dir is gone after delivery and on a restart that finds
  the job terminal.
- Health: boot ping failure drops the entry; a heartbeat ping failure flips `accepting=n` on a
  hook-only seat; a passing due ping restores it; a stale healthy ping does not undo a newer strike.
- Boot/doctor: a hook-only seat boots with no docker and no nix (with and without
  `--skip-doctor`); a mixed seat still requires both.

**PR 2 — hook sessions.** §4 session advertising, §5.3, §8.
Tests: the claim advertises sessions for a selected hook with `[sessions] enabled` on a seat with
no docker; turn 1's session row and result are written in one transaction (an injected session
insert failure publishes nothing); an immediate follow-up finds the session; turn 2 runs the hook
with `turn=2`, the follow-up text and the follow-up id as `invocation` after the review passes; a
refused review runs nothing; `needs_input` reaches the result tags; inputs changed by turn 1 are
restored for turn 2; close/expiry on a docker-less seat send `close`, delete the dir and make no
docker call; a hanging `close` does not hold a slot.

**PR 3 — docs, adapters, `seller hook test`.** §10. The channel adapter's `answer` tool carries
the `invocation` and is matched to exactly one waiting `push`; a late or duplicate answer is
rejected. Tested by `seller hook test` in CI against a fake `claude` on PATH, including a delayed
turn-1 answer arriving while turn 2 waits.

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
2. **Capability tokens for hooks.** Default: a hook entry advertises none (§3, §9), so buyers
   requiring `node`/`python`/`rust` never match it. A `capabilities = [...]` self-report in the
   preset can be added later without a wire change.
