# Session jobs — one job, many turns

**Status:** design spec. No code written. **Anchor commit: `e026242030df68163d1c5f4c638d8889bd7a8300`
(= `origin/main` at the time of writing, 21 Oct 2026).** Every line number below was re-derived by
`git grep -n` at that commit and is written `file:line @e0262420`. Paths are abbreviated:

| Prefix | Path |
|---|---|
| `core/` | `crates/maxplayer-core/src/` |
| `relay/` | `crates/buzz/crates/buzz-relay/src/` |
| `relaydb/` | `crates/buzz/crates/buzz-db/src/` |
| `ppp/` | `crates/buzz/crates/maxplayer-private-protocol/src/` |
| `mcp.rs` | `crates/maxplayer/src/mcp.rs` |

**Scope.** A buyer keeps one awarded job open across turns. After the first delivery it sends
follow-ups on the same job; the same seller answers each one in the same container, with the agent's
conversation still loaded. Offer, claim and award happen once. The main use is interactive coding.

---

## 0. Settled inputs — not relitigated here

Decided by **Josip** (and **Petar** for parking) in Discord **#multi-turn-jobs**, **19–21 Oct 2026**.
Final stakeholder summary: 21 Oct 2026 12:38 UTC.

1. **Build the session job directly; every job is continuable.** No continuation-offer step first.
   A seller on an older version says on its claim that it does not support sessions; its jobs stay
   single-turn. (Josip, 20 Oct.)
2. **Turn price = job price.** Each turn costs the price agreed at claim; x turns cost x × that price.
   Follow-ups carry no price. **The buyer sets a total cap per session.** The buyer reserves a turn's
   payment before sending the follow-up; the seller will not start turn N+1 until turn N is paid.
   (Josip, 20 Oct.)
3. **No `ask_buyer` in v1.** A turn that needs an answer ends with a `needs_input` status; a turn that
   runs out of time ends with `out_of_time`. Each is a normal, fully priced turn. (Josip, 20 Oct.)
4. **Parking via `docker stop` / `docker start -ai` on the job's own container.** The netns holder keeps
   running while parked; the periodic cleanup checks the session record before removing it. Rejected:
   mounted state folder, `docker commit`. (Petar + Josip, 21 Oct 12:21 UTC.)
5. **6 h default parked lifetime** (seller config), **disk budget with oldest-first eviction**,
   **buyer-side close**. (Josip, 20 Oct 17:29 UTC.)
6. **Host or Docker restart expires parked sessions.** No holder rebuild in v1. (Josip, 21 Oct 12:33 UTC.)
7. **Each follow-up carries its own deadline**, 1 h default set by the buyer side, seller
   `job_timeout_secs` override as today. A turn that runs out delivers what it has. (Josip, 21 Oct
   12:36 UTC.)
8. **Both reviews on every turn**, the seller-side follow-up review overlapped with resume; measure
   review time on the test env before optimizing. Follow-up review context: every buyer message in full,
   earlier deliveries summarized or left out, neither party writes it, the reviewer summarizes only past
   the 128 KB limit. Existing trust opt-outs apply unchanged. (Josip, 20 Oct 15:52 + 21 Oct 12:38 UTC.)
9. **A follow-up to a busy seller waits for a slot**; parked sessions take a free slot before new
   offers. (Josip, 20 Oct.)
10. **No maximum session size in v1** (harnesses compact long conversations). `session/load` end-to-end
    test in the sandbox skipped (Josip: safe to assume). (21 Oct.)
11. **Credential proxy is stopped at park and re-minted per turn** (new proxy, new model-key
    placeholder each turn), not kept alive while parked. §6.4. (Josip, 21 Oct 22:24 UTC.)
12. **Defaults** (Josip, 21 Oct 22:27 UTC): session cap when neither `post_job` nor `reply` sets
    `max_session_sats` = `10 × job price`; parked disk budget = 20 GiB per seat (revisited after the
    test-env size measurement); no automatic close of abandoned private session repos in v1
    (seller-only, create-only, 5 GiB quota); `reply` with an uncollected previous turn is refused with
    "collect first".

---

## 1. What exists today

- **One job is one turn.** The lifecycle ends at `accept → pay → receipt` (`docs/protocol-v1.md:407
  @e0262420`). Multi-turn buying is a buyer-side convention in a site skill
  (`web/app/.well-known/skills/multi-turn-buying/skill.md:2 @e0262420`); nothing in code connects turn
  N to turn N+1.
- **The conversation dies with the container.** Harness state lives under the image's
  `HOME=/home/agent` (`docker/maxplayer-sandbox/Dockerfile:271 @e0262420`), and jobs can install into the
  writable `/nix/store` (`Dockerfile:192 @e0262420`). On a docker seat with container delivery the
  container runs `maxplayer __deliver phase1` (`core/seller_node/run.rs:9353 @e0262420`); after it exits
  the seller captures diagnostics and removes it (`CleanupPolicy::CaptureThenRemove`,
  `core/seller_node/run.rs:9515 @e0262420`; `cleanup_job_container`, `core/seller_exec.rs:3213 @e0262420`;
  `force_remove_argv`, `core/seller_exec.rs:1823 @e0262420`). The holder goes when `prepared` drops
  (`core/seller_node/run.rs:9557 @e0262420`; `impl Drop for NetnsHolder`, `core/sandbox_netns.rs:920
  @e0262420`).
- **The workdir is on the host** (`job_workdir` = `$MAXPLAYER_HOME/seller-jobs/<job_id>`,
  `core/seller_exec.rs:1671 @e0262420`), bind-mounted at `/work` (`core/seller_exec.rs:228`, `:1136
  @e0262420`). See Appendix A1: it is not removed today.
- **ACP adapters support reload.** The shipped pins (`Dockerfile:93-94 @e0262420`: claude-agent-acp
  0.85.0, codex-acp 2.1.1) both advertise `loadSession: true` in `initialize` (npm-packed `dist/`,
  checked in the workspace: `claude-agent-acp/dist/acp-agent.js:1308`, `loadSession` at `:1373`, reading
  the transcript from disk via `dist/resumed-session.js:37`; `codex-acp/dist/index.js:37448`,
  `loadSession` at `:37855`). The driver speaks `initialize`, `session/new`, `session/prompt`,
  `session/cancel` (`core/driver/acp_driver.rs:366`, `:397`, `:412`, `:443 @e0262420`) and has no
  `session/load`; the `Driver` trait is `core/driver/mod.rs:117 @e0262420`.

---

## 2. Wire

### 2.1 DECISION — one new kind, `3412` FOLLOW-UP, buyer-authored

`3410`/`3411` are already taken by `maxplayer-trade` (`crates/maxplayer-trade/src/lib.rs:6-7
@e0262420`); `3412` is unused in `crates/`, `docs/` and `web/app/src` at the anchor (`23412`, an
ephemeral trade kind, is a different number). Close is a follow-up with a different status rather than a
second kind.

| Tag | Card. | Req. | Meaning |
|---|---|---:|---|
| `["e", offer_id, "", "root"]` | 1 | yes | The job |
| `["e", prev_result_id, "", "reply"]` | 0..1 | yes* | The result this turn continues. Required for `turn` |
| `["status", "turn" \| "close"]` | 1 | yes | Start a turn, or close the session |
| `["turn", n]` | 0..1 | yes* | Turn number, `n ≥ 2`. Required for `turn` |
| `["param","deadline", unix]` | 0..1 | yes* | This turn's deadline. Required for `turn` |
| `["i", text]` | 0..1 | yes* | Buyer message, public jobs only |
| `["content-id", …]`, `["content-commitment", …]` | 0..1 | yes* | Buyer message, private jobs only |
| `["p", seller_pubkey]` | 1 | yes | The awarded seller |
| `["t","maxplayer"]`, `["v", …]` | 1 | yes | As every event |

On a private job the message is a NIP-44/NIP-59 private content body exactly like the offer task: add a
`ContentType::FollowUp` variant (`ppp/lib.rs:72 @e0262420`) with the validation rules of `Task` except
`offer_id`/`award_id` are required (`ppp/lib.rs:283 @e0262420` is the `Answer` arm it mirrors).
`ContentBody` is `deny_unknown_fields` (`ppp/lib.rs:208 @e0262420`) and a new enum variant is refused by
an old reader — harmless, because only the awarded seller reads a follow-up and it only reads one after
advertising sessions on its claim.

A reader MUST ignore a follow-up whose author is not the buyer that authored the job's AWARD, whose
`p` is not this seat, or whose `turn` is not exactly one more than the last turn the seller delivered.

### 2.2 Claim — the capability

`["session", "1"]` on CLAIM (3402), 0..1, **absent ⇒ no session support**. Absent is the fail-closed
direction: every claim on the wire today reads as single-turn. A seat emits it only when
`[sessions] enabled = true` **and** it is a docker seat on the container-delivery path (§5.1). Protocol
§6.2 says a claim carries the three filterable tags "and no others" (`docs/protocol-v1.md:607
@e0262420`); that sentence gains `session`. It is **not** an award filter: every job stays awardable to
every seller (input 1); it only decides whether `reply` works after turn 1.

### 2.3 Result — per-turn status

| Tag | Card. | Meaning |
|---|---|---|
| `["e", followup_id, "", "reply"]` | 0..1 | The follow-up this result answers (absent on turn 1) |
| `["turn_status", "done" \| "needs_input" \| "out_of_time"]` | 0..1 | Absent ⇒ `done` |
| `["context", "reset"]` | 0..1 | The agent's conversation could not be reloaded (§6.3) |
| `["creq", creqA…]` | 0..1 | Turn ≥ 2 on a priced job: this turn's payment request (§8.2) |

The question of a `needs_input` turn is the result's content (inline) or the delivery commit's message
plus content (git), §7.2.

`session_expired` is not a result — nothing was delivered. It is a FEEDBACK (3404) with
`["status","refusal"]`, `["reason_code","session_expired"]`, `["e", followup_id, "", "reply"]`; it does
not count against the seller. Protocol §7.1 already makes the reason vocabulary extensible
(`docs/protocol-v1.md:807 @e0262420`).

### 2.4 Accept — binds a turn

A session ACCEPT (3406) carries `["e", result_id, "", "reply"]`. Protocol §6.5 calls ACCEPT
ambiguous across re-deliveries and binding it to a result "a deliberate future protocol rev"
(`docs/protocol-v1.md:715`, `:719 @e0262420`). Session jobs need it: one claim produces one result per
turn. The tag is additive (`docs/protocol-v1.md:33 @e0262420`: readers ignore unknown tags) and only
session-aware peers rely on it; both ends of a session job are session-aware by construction (§2.2).
For a session job, ACCEPT closes the **turn**; the job is closed by a `close` follow-up, a REJECT, or
seller-side expiry.

### 2.5 No `PROTOCOL_VERSION` bump

Same argument as `docs/specs/free-job-lane.md` §1.2: the wire version is compared by strict equality
(`core/gateway.rs:10 @e0262420`, `parse_offer` `:591`), so a bump partitions the market. Everything above
is a new kind or a new optional tag. Protocol §2.1 lists the maxplayer-owned kinds
(`docs/protocol-v1.md:35 @e0262420`) and §3 the kind table (`:60`); both gain `3412`.

---

## 3. Relay admission and the private allowlist — the relay changes

**All of these ship in the same PR and must be deployed to relay.maxplayer.ai before any seller enables
`[sessions]`.** Order: relay (and reviewer service) first, then sellers, then buyers.

1. **Ingest allowlist.** `required_scope_for_kind` (`relay/handlers/ingest.rs:546 @e0262420`) refuses
   unknown kinds with `restricted: unknown event kind` (`:680`). Add `const KIND_MAXPLAYER_SESSION_FOLLOWUP:
   u32 = 3412;` beside `:73` and to the trade-path arm ending `:669`, scope `MessagesWrite`. Extend the
   literal-number test that today asserts `3410` is refused (`:4510 @e0262420`) to assert `3412` is
   admitted and `3410` still refused.
2. **Private allowlist (`validate_private`, `ppp/wire.rs:238 @e0262420`).** It refuses any kind outside
   `3400..=3407` (`:243`) and any unlisted field (`:403`).
   - Range → `3400..=3407 | 3412`; add `3412` to the `allowed` table (`:313`), the `required` table
     (`:449`) and the `unreachable!` matches (`:399`, `:482`); status set for 3412 =
     `["turn","close"]` next to `:495`; 3412 requires exactly one participant (the seller) — today's
     non-offer rule allows 1..2 (`:544`).
   - `3402` allowed (`:333`): add `"session"`, value `member(v, &["1"])`.
   - `3403` allowed (`:353`): add `"reply"`, `"turn_status"`, `"context"`, `"creq"` (the existing
     16 KiB creq bound at `:440` applies).
   - `3406` (`:344`): add `"reply"`.
   - Feedback reasons (`ppp/wire.rs:170 @e0262420`): add `"session_expired"`.
   - `3404` allowed (`:345`): add `"reply"` — §2.3's `session_expired` FEEDBACK carries
     `["e", followup_id, "", "reply"]`, which the private allowlist would otherwise refuse.
     (Found while implementing; the list above omitted it.)
   - Client side, same rows: the core private carrier projection
     (`core/private_content/carriers.rs`, `project`) has its own per-kind tag allowlist and drops
     unlisted tags before signing; it gains `session` on 3402 and `turn_status`/`context`/`creq`
     on 3403, or a private session claim/result would be published without them.
   - **Why this is load-bearing on the relay, not only in clients:** private-repo provisioning validates
     the claim with this same function (`relay/api/git/private_jobs.rs:202 @e0262420`). A private claim
     carrying `session` is refused by an un-upgraded relay, and provisioning fails.
3. **Private repo closure.** The relay closes a private job repo as soon as the buyer publishes a
   3400/3406/3407 for it (`relaydb/private_jobs.rs:78 @e0262420`), and a closed repo refuses every seller
   ref (`can_create_ref`, `relaydb/private_jobs.rs:39 @e0262420`). Turn 1's ACCEPT would therefore stop
   turn 2 from pushing. Change: store `session BOOL` on `private_job_repositories`, set at provisioning
   from the validated claim's `session` tag; for a session repo `closed` is computed from 3407 or a 3412
   with `["status","close"]` only. Seller delivery refs stay create-only (`:41`) under
   `refs/heads/delivery/<hex32>` (`:56`); turns use `refs/heads/delivery/<followup_id>` (§7.1), so no ref
   is ever updated. Quotas are per repo, so all turns share `MAX_REPO_BYTES` (5 GiB).
4. **Reviewer service.** Review subjects accept only OFFER and RESULT
   (`core/review.rs:106 @e0262420`; `enabled` `:72`, `unsupported subject kind` `:82`). The reviewer (the
   privacy service) needs the 3412 subject (§9) and is deployed with the relay.

Public events are not run through `validate_private`, so a public-only deployment needs changes 1 and 4.

---

## 4. Per-turn deadline

- **Buyer:** `reply` sets `["param","deadline", now + 3600]` unless the caller passes `deadline_secs`
  (same default as an offer's, `core/job_lifecycle.rs:38 @e0262420`).
- **Seller:** the turn's run deadline is `job_deadline_unix` over the follow-up instead of the offer
  (`core/seller.rs:145 @e0262420`) — `job_timeout_secs` (`core/home.rs:217`) still overrides, and the
  600 s fallback (`core/seller.rs:20`) still applies.
- **Everything counts:** slot wait, review, `docker start` and `session/load` are inside the deadline.
  If a slot frees with less than `wrap_up_secs + PUSH_MARGIN_SECS + 120` left
  (`PUSH_MARGIN_SECS` = 300, `core/delivery_orchestrator.rs:998 @e0262420`), the seller does not start;
  it publishes FEEDBACK `at_capacity` and the session stays parked. Nothing is charged.
- **Out of time:** at `deadline − wrap_up_secs` (default **300**, unmeasured, input 7) the orchestrator
  sends `session/cancel` (`core/driver/acp_driver.rs:443 @e0262420`), waits for the turn to end, then
  snapshots and delivers with `["turn_status","out_of_time"]`. A cancelled turn with no file change and an
  empty last message is refused `no_sentinel` exactly as today — nothing was observed, nothing is charged.

---

## 5. Parking

### 5.1 Where sessions run

Only on **docker seats using container delivery** (the default for `mode = "docker"`). There the seller
host does not drive ACP — the orchestrator inside the container does (`run_phase1_entry`,
`core/delivery_orchestrator.rs:410`; `drive_acp_agent`, `:594 @e0262420`). That is what makes `docker
start` work: the container's command is fixed at create time and is the orchestrator itself, reading
`/maxplayer-io/inputs.json` (`CONTAINER_EXCHANGE_DIR`, `:889`). The host re-creates the exchange dir at
the same bind source (`create_exchange_dir`, `:1429`) and writes the next turn's inputs before
`docker start -ai`. Host-path and launcher seats do not advertise `session` (left out, §13).

### 5.2 End of turn

1. The orchestrator writes the outcome as today plus `acp_session_id` (`AgentOutcome`,
   `core/delivery_orchestrator.rs:1012 @e0262420`, gains one optional field), reaps other processes and
   exits (`reap_other_processes`, `:1640`). The container is now stopped. If it is still running after
   `CONTAINER_EXIT_GRACE_SECS`, `docker stop`.
2. The host captures diagnostics as today, then **does not remove**: a new `CleanupPolicy::CaptureThenPark`
   (`core/seller_exec.rs:2000 @e0262420`) settles the `JobContainer` guard (`:2185`, its `Drop` removes,
   `:2210`) without `docker rm`.
3. The netns holder is detached from `prepared`'s drop: `NetnsHolder::park()` disarms its `Drop`
   (`core/sandbox_netns.rs:920 @e0262420`) and returns the name. The credential proxy is **stopped** (§6.4).
   Held-tool sockets detach as today (`core/seller_node/run.rs:9518 @e0262420`).
4. Write the session record (§5.4) with `state = parked`, `last_turn_at = now`, and the holder's
   `State.StartedAt`.

A stopped container uses no RAM or CPU; only disk (the container's writable layer plus `/work`).

### 5.3 What must keep a parked holder alive

- **The expiry sweep.** The holder and helpers carry `HOLDER_CLEANUP_AFTER_LABEL` = turn-1 deadline + 1 h
  (`core/sandbox_netns.rs:87`, `:148 @e0262420`; stamped at `core/seller_exec.rs:3025`), and labels
  cannot change on a running container. `sweep_expired` (`core/sandbox_netns.rs:2077 @e0262420`, called
  from `core/seller_node/run.rs:5060`) gains a `keep: &BTreeSet<String>` of job ids with a live session
  record; `partition_owned` (`core/sandbox_netns.rs:1713`) reports those as `skipped` with reason
  `parked_session` instead of removing them.
- **The boot reaper** (`reap_orphans`, `core/sandbox_netns.rs:1983 @e0262420`, at boot
  `core/seller_node/run.rs:4176`) keeps holders that any container — `ps --all`, so stopped ones too
  (`core/sandbox_netns.rs:1820`) — names in its `NetworkMode` (`reapable_holders`, `:1862`). A parked job
  container keeps `container:<holder-id>`, so the holder is kept. This is believed, not measured (no
  docker on the spec host): the implementation PR carries a live test for it.
- **The job container itself** has only `JOB_LABEL` (`core/seller_exec.rs:1120 @e0262420`) and no seat
  label, so neither sweep selects it (`list_owned_argv` filters on the seat label,
  `core/sandbox_netns.rs:1464`). Only session GC removes it.
- The resolver file `seller-jobs/resolv-<job>.conf` (`core/seller_exec.rs:3001 @e0262420`) is mounted
  read-only at create and must survive; session GC deletes it with the session.

### 5.4 Session record (seller store)

`SCHEMA_VERSION` 14 → 15 (`core/seller_node/store.rs:48 @e0262420`), additive tables only (`migrate`
`:1181`; the `jobs.state` CHECK at `:981` is not touched).

```sql
CREATE TABLE IF NOT EXISTS sessions (
  job_id TEXT PRIMARY KEY, buyer_pubkey TEXT NOT NULL,
  container TEXT NOT NULL, holder TEXT, holder_started_at TEXT,
  image_id TEXT NOT NULL, orchestrator_version TEXT NOT NULL,
  acp_session_id TEXT, harness TEXT NOT NULL, model TEXT,
  turn INTEGER NOT NULL, last_turn_at INTEGER NOT NULL, disk_bytes INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL CHECK (state IN ('running','parked','closed','expired')));
CREATE TABLE IF NOT EXISTS session_turns (
  followup_id TEXT PRIMARY KEY, job_id TEXT NOT NULL, turn INTEGER NOT NULL,
  deadline_unix INTEGER NOT NULL, result_id TEXT, delivery_oid TEXT,
  state TEXT NOT NULL CHECK (state IN
    ('received','waiting_payment','reviewing','running','delivered','paid','refused')),
  created_at_unix INTEGER NOT NULL, updated_at_unix INTEGER NOT NULL,
  UNIQUE (job_id, turn));
```

Turn 1 has no `session_turns` row; it is the existing `jobs` row.

### 5.5 Lifetime, disk, close, restart

- `[sessions] lifetime_secs = 21600` (input 5). A GC pass runs on the existing sweep cadence
  (`run_expiry_sweep`, `core/seller_node/run.rs:5051 @e0262420`): a parked session past
  `last_turn_at + lifetime_secs` is expired — `docker rm --force` the container and the holder, remove
  the workdir and the resolver file, `state = expired`.
- `[sessions] disk_budget_bytes` (default §0 item 12). After each park, measure the session
  (`docker container inspect --size` → `SizeRw`, plus `du` of the workdir) into `disk_bytes`; while the
  sum over parked sessions exceeds the budget, expire the oldest `last_turn_at` first. A running session
  is never evicted.
- **Close.** A `close` follow-up from the job's buyer expires the session immediately (state `closed`).
- **Host or Docker restart** (input 6). On resume, `docker inspect` the holder: not running, or
  `State.StartedAt` ≠ the recorded value (a restarted holder is a new namespace with no rules) ⇒ expire
  and answer the follow-up with `session_expired`. The job container is never started without the
  firewall it was created into.
- **Seller restart** does not touch parked sessions (stopped containers and the record survive). A
  turn in state `running` at boot is treated like `ResumeAction::RunAgent`
  (`core/seller_node/run.rs:1534 @e0262420`): if the turn's deadline is still live, restart it from the
  same follow-up; else FEEDBACK `execution_failed`, turn unpaid, session parked.
- **Image or binary upgrade.** The container keeps its old image, and with it the old orchestrator
  binary baked in at `/usr/local/bin/maxplayer` (`Dockerfile:220 @e0262420`). Inputs carry
  `session_protocol: 1`; the orchestrator refuses an inputs file it does not understand, and the host
  expires a session whose recorded `orchestrator_version` is below the protocol it would send. Within
  one session protocol version an old container keeps working.

---

## 6. Resume

### 6.1 Order (review overlapped with start)

1. Follow-up arrives. Decrypt (private), check: job is ours and awarded, author is the AWARD's buyer,
   `turn = last + 1`, session not `closed`/`expired`, previous turn `paid` (§8.3). Write
   `session_turns(received)`.
2. In parallel:
   - **Review** the follow-up (§9).
   - **Slot**: acquire with the waiting `acquire_unreserved` (`core/seller_node/run.rs:222 @e0262420`).
   - **Start**: once the slot is held, check the holder (§5.5), start a fresh credential proxy (§6.4),
     write `inputs.json` **without the buyer's text**, `docker start -ai`. The orchestrator runs
     `initialize` and `session/load { sessionId, cwd: "/work", mcpServers }` and then waits for
     `prompt.json` in the exchange dir.
3. Review passes ⇒ the host writes `prompt.json` (the composed follow-up prompt). Review refuses ⇒ the
   host writes `abort`, the orchestrator exits without prompting, the session is parked again,
   FEEDBACK as the offer review publishes today, the buyer's reservation is released. The buyer's text
   never enters the container before the review passes.
4. `session/prompt`, then the normal turn tail: gate, snapshot, push, RESULT, park.

### 6.2 Busy seller

Parked sessions take free slots before new offers by construction: a follow-up **waits** on the slot
semaphore, while a new offer only **tries** (`try_reserve`, `core/seller_node/run.rs:178 @e0262420`) and
is skipped when full. tokio hands a released permit to queued waiters before returning it to the pool
(`add_permits_locked`, `tokio-1.52.3/src/sync/batch_semaphore.rs:306`, the version in `Cargo.lock`), so
a waiting follow-up gets it first — the FIFO property `core/seller_node/run.rs:209 @e0262420` already
relies on. A follow-up whose deadline cannot be met is dropped per §4.

### 6.3 `session/load` failure ⇒ new session, `context_reset`

Add `Driver::load_session(cfg, id)` (`core/driver/mod.rs:117 @e0262420`), sent only when `initialize`
advertised `agentCapabilities.loadSession` (next to `start_session`,
`core/driver/acp_driver.rs:395 @e0262420`). If the capability is absent, the call errors, or no
`acp_session_id` was recorded, the orchestrator runs `session/new` in the same container: files and
installed tools stay, and the result carries `["context","reset"]`. The prompt then also states the
original task and the follow-ups so far (buyer messages only, newest last, cut at 64 KiB).

### 6.4 Credentials across turns — answers

| Credential | Today | Session answer |
|---|---|---|
| **Model-key placeholder** | Minted per launch (`mint_placeholder`, `core/credential_proxy.rs:975 @e0262420`); the proxy lives for the run and dropping it revokes (`core/seller_exec.rs:2966`). | **Re-mint per turn.** Stop the proxy at park; at resume start a new one, mint new placeholders, and pass the agent env names/values and redirect argv in `inputs.json`. The orchestrator already builds the agent's env from an allowlist (`agent_env_allowlist`, `core/delivery_orchestrator.rs:1396`) and takes the argv from inputs (`Phase1Inputs`, `:338`); it applies the per-turn values over the container env. |
| **Delivery push token** | `fresh-after-agent` (default, 60 s) or `long-lived` capped at `DEFAULT_CONTAINER_DELIVERY_TOKEN_CAP_SECS` = 6 h (`core/seller_exec.rs:313 @e0262420`; `long_lived_expiration`, `core/delivery_orchestrator.rs:1331`). | **Per turn, unchanged.** Fresh mode is already per run. Long-lived is minted per turn against the turn's deadline, so the 6 h cap bounds one turn, not the session. |
| **Codex ChatGPT session** | Host `auth.json` read per run, never mounted (`core/home.rs:854 @e0262420`), lifetime checked against the job (`codex_chatgpt_session_for_command`, `core/seller_exec.rs:2695`; check at `core/codex_subscription.rs:241`). | **Re-read per turn** with `required_lifetime` = the turn's lifetime, and register a fresh Codex proxy credential (`register_codex_session`, `core/credential_proxy.rs:495`). An expired host login fails that turn before the container starts (`execution_failed`, unpaid, session stays parked). |
| **Held tools / MCP tools** | Attached per run (`attach_held_tools`, `core/seller_node/run.rs:5485`); mount subpath fixed at create (`core/held_tool.rs:1020`). | Re-attach per turn into the same subpath; vendor MCP placeholders re-minted with the proxy. |

**Why not keep the proxy alive while parked** (the stakeholder summary's first option): each proxy holds
a port from `proxy_port_range`, which operators size to `slots` (`core/home.rs:2486 @e0262420`), so a few
parked sessions would starve new jobs; and the listener binds `0.0.0.0` with a known quota-theft exposure
for the job's lifetime (`core/credential_proxy.rs:210`, `:1228 @e0262420`), which parking would stretch to
6 h. Re-minting closes both and costs a proxy start per turn.

---

## 7. Delivery per turn

### 7.1 Branch, base, sentinel

- **Branch.** Private: `refs/heads/delivery/<followup_id>` (turn 1 keeps `delivery/<offer_id>`,
  `core/seller_node/privacy.rs:459 @e0262420`). Public: `maxplayer/<job8>-t<n>` (turn 1 keeps
  `maxplayer/<job8>`, `core/seller_node/run.rs:9924 @e0262420`). Every turn pushes a new ref; none is
  updated.
- **Base.** Turn n's commit has exactly one parent: the last git-delivered commit of the session (turn 1:
  as today). Inputs carry `base_oid`; the orchestrator skips provisioning (`run_phase1` step 1,
  `core/delivery_orchestrator.rs:216-244 @e0262420`) and snapshots with that base
  (`snapshot_delivery_at`, `core/seller_git.rs:315`; refused if the tree equals the base, `:271`). The
  layout gate and config replacement run on every push as today. If the agent deleted the base object
  from `.git`, the snapshot fails `delivery_failed` — the agent can only hurt its own turn.
- **Turn hash.** For n ≥ 2 the `job-hash` the result carries and the sentinel is seeded from is the hash
  of the **follow-up**: public `job_hash_for_offer(followup_id, followup_text, amount)`
  (`core/job_lifecycle.rs:2763 @e0262420`), private `pc::job_hash(followup_id)` (`ppp/lib.rs:62`). This
  makes every turn's receipt preimage (`receipt_preimage_for`, `core/authorize_pay.rs:1101 @e0262420`,
  which binds `job_hash`) distinct, and a turn-1 tree cannot be replayed as turn 2, without changing the
  preimage struct.

### 7.2 Inline turns and questions

The agent marks a question by beginning its final message with `MAXPLAYER-NEEDS-INPUT-V1` (the §8.3
pattern, `docs/protocol-v1.md:870 @e0262420`); the composed prompt (`compose_agent_prompt`,
`core/seller_exec.rs:2328`) gains a session section saying so.

- Tree changed ⇒ git delivery; the question (if any) is the result content.
- Tree unchanged since the session base ⇒ inline delivery, content = the marked message
  (`MAXPLAYER-ANSWER-V1` or `MAXPLAYER-NEEDS-INPUT-V1`), or for `out_of_time` the agent's last message.
- **Protocol relaxation, session jobs only:** today inline is refused on contribution jobs
  (`docs/protocol-v1.md:669`, `:677 @e0262420`). On a session job an inline turn is allowed in either class
  and does not move the base. A one-shot job is unchanged.

### 7.3 Turn status

`done` / `needs_input` / `out_of_time` from the orchestrator: `out_of_time` when §4's cancel fired,
`needs_input` from the marker, else `done`. `Phase1Status` (`core/delivery_orchestrator.rs:1032
@e0262420`) is unchanged; the turn status rides in `Phase1Outcome` (`:1061`) as a new optional field.

---

## 8. Payment

### 8.1 Buyer: cap, reserve, send

`reply(job_id, text)` refuses locally, before anything is signed, when:
- the claim had no `session` tag ("seller does not support sessions; post a new job");
- the previous turn is delivered but not collected ("collect turn n first");
- the session is closed/expired, or a turn is already in flight;
- the per-job cap refuses the price (`BudgetGate::check`, `core/budget.rs:191 @e0262420` — still the
  sole spend gate, `:51`);
- **session cap:** `sum(spent + reserved over the session's turns) + price > session_cap_sats`.
  `session_cap_sats` is stored per job in a new buyer `sessions` table (buyer `SCHEMA_VERSION` 8 → 9,
  `core/buyer/store.rs:48`), set from `post_job`/`reply` `max_session_sats` or the default (§0 item 12).

Then: sign the follow-up once and persist it (write-once, retries re-send the same bytes — protocol §11
rule 2's discipline, `docs/protocol-v1.md:989 @e0262420`), reserve the price keyed by the **follow-up id**
(`reserve_at`, `core/buyer/store.rs:370`; reservations are keyed by `job_id TEXT PRIMARY KEY`, `:126`, and
a `spent` row is never re-reserved, so turn n needs its own key), then publish. A free job reserves
nothing and needs no wallet, as today.

### 8.2 Per-turn payment identity

Today one claim `creq` is single-use with `payment_id = job_id` (`core/gateway.rs:1450`, `:1489
@e0262420`); the buyer verifies that id (`core/job_lifecycle.rs:2344`), puts the job id in the NUT-18
payload (`core/payment_wallet.rs:1993`), and the seller skips any payment for a job that already has a
receipt (`core/seller_node/run.rs:10080 @e0262420`). Turn 2 would be dropped as already receipted.

- Turn 1: unchanged.
- Turn n ≥ 2 on a priced job: the RESULT carries a fresh `creq` with `payment_id = followup_id`, same
  amount/unit, mints ⊆ the claim's. The buyer verifies it like the claim creq
  (`verify_accepted_claim_creq`, `core/job_lifecycle.rs:2329 @e0262420`) with `followup_id` in place of the
  job id, and pays with payload `id = followup_id`.
- Seller receive (`on_gift_wrap`, `core/seller_node/run.rs:10035 @e0262420`) maps a payload id found in
  `session_turns` to `(job_id, turn)`; the receipt row is written with `job_id = followup_id`
  (`receipts`, `core/seller_node/store.rs:1017`), so `has_receipt` (`:1987`) dedups per turn unchanged.
  The amount check (`core/seller_node/run.rs:10208`) is unchanged: turn price = job price.
- Buyer: the accept-bind for turn n is `jobs/<followup_id>.json` (`bind_path`,
  `core/job_lifecycle.rs:2736 @e0262420`); `PaymentKey` already includes `result_id`
  (`core/payment.rs:173`), so attempt ids differ per turn; receipts name the result by their `reply` tag.

### 8.3 Seller: one unpaid turn at most

A follow-up for turn n+1 is held in `session_turns(waiting_payment)` until turn n is `paid` (free job:
until turn n's ACCEPT is seen), bounded by the follow-up's deadline. The buyer can collect turn n and
reply immediately; the payment wrap and the follow-up may arrive in either order. A buyer that never
pays leaves the session parked until it expires; the seller's exposure is one turn.

### 8.4 Collect

`collect(job_id)` settles the newest delivered, uncollected turn (optional `turn`). Turn 1 materializes
to `results/<job_id>` as today (`core/collect.rs:458 @e0262420`); turn n to `results/<job_id>-t<n>`.
Verification additionally checks the commit's single parent is the previous git turn's accepted commit.

---

## 9. Reviews

- **Seller, follow-up review.** Add the 3412 subject: `Subject::validate` accepts
  `kind = 3412, offer = root, event = followup` (`core/review.rs:106 @e0262420`), and `enabled` maps 3412
  to `seller_offer` with `skip_buyer_pubkeys` (`:72-82`). The reviewer builds its own context from signed
  relay events — the original task and every follow-up in full, each earlier delivery as one line
  (`turn n: <status>, commit <oid>`), summaries of deliveries left out in v1. Neither party supplies it.
  Only when that context exceeds `MAX_INPUT_BYTES` = 128 KiB (`core/review.rs:13`; checked at
  `core/reviewer.rs:550 @e0262420`) does the reviewer summarize the oldest follow-ups itself; the current
  follow-up is always in full.
- **Buyer, delivery review.** Per turn, from that turn's diff: the reviewer's diff plan already takes a
  base (`core/reviewer.rs:618`, `:625 @e0262420`); for turn n the base is the previous git turn's commit,
  read from the follow-up the result replies to. A `needs_input` question is reviewed like an inline answer.
- `skip_buyer_pubkeys`, `skip_seller_pubkeys`, and `buyer_delivery = false` (`core/review.rs:24
  @e0262420`) apply unchanged.
- **Measure before optimizing** (input 8): the test-env run in §11 logs review wall time per follow-up
  and per delivery.

---

## 10. Buyer MCP surface

`mcp.rs` exposes four tools today (`post_job` `:263`, `get_job` `:359`, `collect` `:374`,
routing `:422 @e0262420`; `AGENTS.md:41` says "exactly the four-tool buyer trade loop").

- **`reply(job_id, text, deadline_secs?, max_session_sats?, close?)`** — new. Starts the next turn
  (§8.1). `close: true` with no text sends the close follow-up and closes locally; it never pays.
- **`get_job`** gains `session: { supported, turn, status: running|parked|closed|expired|no_response,
  turn_status, question, context_reset, expires_hint }`. `question` is the latest `needs_input`
  content. `no_response`: the follow-up's deadline passed with no result; the reservation is released
  (as feedback-driven releases do today, `core/buyer/mod.rs:2934 @e0262420`) and `reply` may retry.
- **`collect`** gains `turn?` and `close_session?` (collect, then send close).

The buyer's agent needs no skill or keyword. `AGENTS.md` and `multi-turn-buying/skill.md` are updated to
point at `reply`.

---

## 11. Implementation — ONE PR behind `[sessions] enabled = false`

`[sessions]` (both roles): `enabled = false`, `lifetime_secs = 21600`, `disk_budget_bytes`,
`wrap_up_secs = 300`, buyer `session_cap_sats`. With `enabled = false` no claim carries `session`, no
follow-up is sent or accepted, and every code path is today's.

| # | File | Change |
|---|---|---|
| 1 | `relay/handlers/ingest.rs` | Admit 3412 (§3.1) + test |
| 2 | `ppp/wire.rs`, `ppp/lib.rs` | §3.2 allowlist rows; `ContentType::FollowUp` |
| 3 | `relaydb/private_jobs.rs` + migration, `relay/api/git/private_jobs.rs` | `session` column, closure rule (§3.3) |
| 4 | `core/kinds.rs` | `SESSION_FOLLOWUP_KIND = 3412` (outside the contiguous-block test at `:67`) |
| 5 | `core/gateway.rs` | follow-up draft/parse; claim `session` tag (`claim_draft :798`); result tags (`result_draft :959`); accept `reply` (`accept_draft :855`) |
| 6 | `core/review.rs`, `core/reviewer.rs`, `core/review/request_wire.rs` | 3412 subject, context assembly, per-turn diff base |
| 7 | `core/driver/{mod,acp_driver}.rs` | `load_session`, `loadSession` capability read |
| 8 | `core/delivery_orchestrator.rs` | resume inputs (`session_protocol`, `acp_session_id`, `base_oid`, env/argv overrides), `prompt.json`/`abort` wait, wrap-up cancel, outcome `acp_session_id` + `turn_status` |
| 9 | `core/seller_exec.rs` | `CaptureThenPark`; per-turn `prepare_launch` without holder creation (`:2932`) |
| 10 | `core/sandbox_netns.rs` | `NetnsHolder::park`; `sweep_expired` keep-set; holder `StartedAt` check |
| 11 | `core/seller_node/{run,store}.rs` | session store v15; follow-up subscription (add 3412 with `#p` = seat to `award_filter`, `core/seller_node/run.rs:1325 @e0262420`); resume flow §6.1; GC/eviction §5.5; per-turn payment §8.2–8.3 |
| 12 | `core/buyer/*`, `core/job_lifecycle.rs`, `core/collect.rs` | buyer store v9; `reply`; per-turn bind/verify/collect; session cap |
| 13 | `mcp.rs` | `reply`, `get_job.session`, `collect` params |
| 14 | `docs/protocol-v1.md`, `AGENTS.md`, quickstarts, skill | §2 tables, §5 lifecycle, §6.2/6.4/6.5 notes, §7.1 reason, §6.4 inline relaxation |

**Failure tests the PR must carry** (each proven by a temporary mutation, per repo practice):

1. A follow-up authored by anyone but the awarded buyer, for another seat, or with a skipped/duplicate
   `turn` is ignored; nothing starts.
2. Follow-up for turn n+1 while turn n is unpaid does not run until the payment lands; past its deadline
   it is dropped with no charge.
3. A turn-2 payment is received and receipted (the turn-1 `has_receipt` skip does not swallow it); a
   replayed turn-2 wrap is a no-op.
4. Buyer `reply` over the session cap, over the per-job cap, or with an uncollected previous turn refuses
   with zero reservation and nothing published.
5. Review refusal: the buyer's text is never written into the exchange dir; session re-parked;
   reservation released.
6. Holder missing or restarted (`StartedAt` mismatch) ⇒ `session_expired`, container and holder removed,
   job container never started.
7. `sweep_expired` with a parked session past its stamp keeps holder and helpers; after expiry removes
   them. Live docker test: `reap_orphans` keeps a holder whose only joiner is a stopped container.
8. `session/load` error or capability absent ⇒ `session/new`, `["context","reset"]` on the result.
9. Wrap-up: an agent still running at `deadline − wrap_up_secs` is cancelled and its changes delivered
   `out_of_time`; an unchanged tree with an empty last message is refused `no_sentinel`.
10. Turn-n commit with a parent other than the previous turn's commit is refused by the buyer; a turn-1
    tree replayed as turn 2 fails the sentinel (turn hash).
11. Private: turn-1 ACCEPT does not close a session repo; a close follow-up or REJECT does; delivery refs
    remain create-only.
12. Disk budget: over budget evicts the oldest parked session and never a running one.
13. `enabled = false`: claim, offer, result and accept bytes are identical to today's (golden test).
14. Old orchestrator (lower `session_protocol`) ⇒ session expired rather than started.

**Test env before enabling anywhere:** two seats on the VPS test env, one Claude and one Codex session of
≥ 3 turns each, logging review wall time per follow-up/delivery, parked size, and wrap-up duration
(the three measured risks).

---

## 12. Self-review

### 12.1 Flaws found and fixed in this spec

1. **Turn 1's ACCEPT would close the private repo** and block every later push
   (`relaydb/private_jobs.rs:78 @e0262420`). Fixed: session-repo closure rule, §3.3.
2. **Turn-2 payment would be silently skipped** by the per-job `has_receipt` check
   (`core/seller_node/run.rs:10080 @e0262420`), and reservations cannot be re-taken for a spent job id
   (`core/buyer/store.rs:126`). Fixed: per-turn payment id = follow-up id, §8.2.
3. **Identical receipt preimages across turns** (same `job_hash`; an unchanged inline answer gives the
   same integrity hash). Fixed: per-turn hash from the follow-up, §7.1.
4. **A relay that has not been upgraded refuses private session claims at provisioning**
   (`relay/api/git/private_jobs.rs:202 @e0262420`). Fixed: deploy order, §3.
5. **Keeping the credential proxy alive while parked** would starve `proxy_port_range` and extend the
   `0.0.0.0` exposure to 6 h. Fixed: re-mint per turn, §6.4. Confirmed by Josip, §0 item 11.
6. **A restarted holder is running but has no rules.** "Holder isn't running" is not enough. Fixed:
   `StartedAt` check, §5.5.
7. **Old orchestrator binary inside parked containers** after an upgrade. Fixed: `session_protocol`
   check, §5.5.
8. **Buyer text reaching the container before the review passes** if resume is overlapped naively.
   Fixed: `prompt.json` written only after the verdict, §6.1.
9. **Inline/contribution conflict**: a question-only turn on a contribution job had no legal delivery.
   Fixed: session-only inline relaxation, §7.2.

### 12.2 Money-path invariants (`docs/protocol-v1.md` §11, `:985 @e0262420`)

- **Rule 1, work follows the award** (`:987`): still holds; turn n ≥ 2 work additionally follows a
  buyer-signed follow-up and a paid turn n−1.
- **Rule 2, one offer one award** (`:989`): no second award ever; follow-ups reuse its write-once
  discipline.
- **Rules 3–4, buyer verifies / no cross-bind** (`:992`, `:994`): per turn, plus the parent check.
- **Rule 5, capped** (`:996`): per-job cap per turn, plus the session cap.
- **Rule 6, fee floor** (`:997`): same amount each turn; unchanged.
- **Rule 7, free jobs** (`:998`): a free session never enters the payment path; "paid" becomes
  "accepted" for the one-unpaid-turn rule.
- **Protocol §5 "ACCEPT closes the job"** (`:426`) changes meaning for session jobs (closes the turn).
  Stated in §2.4; third parties joining ACCEPT→RESULT must use the `reply` tag on session jobs.

### 12.3 Risks that come with settled decisions

- **Every job is continuable** ⇒ every container-delivery seat with sessions on keeps a parked container
  after every job, for up to 6 h, unless the buyer closes. Disk is the cost; the budget and eviction bound
  it. Buyers that never close make eviction the normal path.
- **Turn price = job price** ⇒ a big follow-up on a cheap job is underpaid, a trivial one overpaid
  (accepted for v1).
- **`out_of_time` and `needs_input` are full-price turns** ⇒ a buyer can pay for a turn with little
  output. The empty-and-unmarked case is still refused.
- **Host/Docker restart expires** ⇒ a routine host reboot loses every parked conversation; buyers start
  a new job from their collected files.
- **Spam.** Only the awarded buyer's follow-ups are acted on, one unpaid turn at a time, so a buyer
  cannot queue unpaid work. Any authenticated key can still store 3412 events on the relay (same exposure
  as 3401 today). A free session gives the seller's compute away turn after turn — the free-lane
  admission controls (`docs/specs/free-job-lane.md` §6) are the only brake.
- **Single relay.** Follow-ups, results, payment wraps and private repos all go through
  relay.maxplayer.ai. If it is down, no turn can start; parked sessions expire on the seller's clock while
  the buyer cannot reach them. Nothing is lost that was collected.
- **Abandoned private session repos stay writable** by the awarded seller (create-only, quota-bound)
  until a close or reject. §0 item 12.
- **Unmeasured:** review time per turn, parked size, wrap-up margin (§11 test env).

---

## 13. Deliberately left out (each addable later without a wire change)

- **`ask_buyer`** in-turn tool — v1 ends the turn with `needs_input`.
- **Holder rebuild** after host/Docker restart — v1 expires.
- **Maximum session size / turn count** — harness compaction; the session cap bounds spend.
- **Per-turn pricing** and **quote per follow-up** — turn price = job price.
- Sessions on host-path and launcher seats; proactive expiry notices; delivery summaries in the
  follow-up review context; migrating a session to a new image; advertising sessions on the heartbeat.

---

## 14. Open questions

None. The four proposed defaults were confirmed by Josip (§0 item 12).

---

## Appendix — corrections to the ground, measured at `e0262420`

- **A1. No removal of awarded job workdirs was found.** `git grep` of production code (before each file's
  `#[cfg(test)]`) finds `remove_dir_all` on probe workdirs only (`core/seller_exec.rs:1715`,
  `core/seller_node/run.rs:3896` and `:8053 @e0262420`) and on the exchange dir
  (`core/seller_node/run.rs:9551`); none on `seller-jobs/<job_id>` for an awarded job. The conversation is
  lost because the container is removed, not because the workdir is. Consequence: session GC must remove
  the workdirs it owns; the existing growth of non-session workdirs is a separate issue, not fixed here.
- **A2.** The brief's "`session/load` over the same stdin pipe" assumes the host drives ACP. On the
  container-delivery path the orchestrator inside the container drives it (§5.1), so resume is an
  orchestrator change.
- **A3.** `origin/main` moved from `6ec945fe` to `e0262420` (#1191) between the brief and this spec;
  every citation is at `e0262420`.
- **Unverified here:** no docker on the spec host — §5.3's boot-reaper behaviour with stopped joiners and
  the `docker start -ai` of a container joined to a running holder are argued from code, not run. Codex
  `session/load` has not been exercised (no credentials); Claude's was exercised host-side on 20 Oct
  (fresh adapter process reloaded a saved transcript), not in the sandbox.
