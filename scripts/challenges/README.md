# Community challenges v1 buyer runner + local E2E

Run from the repository root (Node **24+**, Python 3, Cargo toolchain used by this checkout,
PostgreSQL 17 and Redis installed):

```sh
bash scripts/challenges/run-e2e.sh
```

The harness builds **this integration checkout** with wallet+ACP and Buzz in dedicated
`target/challenges-e2e` and `target/challenges-buzz` directories, installs the web app's locked
Node dependencies, and builds the separate `crates/maxplayer-challenges` workspace (`maxplayer-swarm` binary and
generic local relay fixture) into its own target. Challenge metadata goes only to that generic
relay; jobs stay on Buzz. On this Nix host it discovers
installed Postgres/Redis in `/nix/store` if absent from PATH. Else supply them on PATH;
`nix shell --extra-experimental-features 'nix-command flakes' nixpkgs#postgresql_17 nixpkgs#redis`
provides those dependencies. Use the repository devshell if the native build needs OpenSSL.
No Docker, global DB, production config, real LLM or mint is used.
Buzz’s Git object-store startup probe is disabled (`BUZZ_GIT_CONFORMANCE_PROBE=false`)
because this is an inline-delivery test with no Git/media object store. S3 points at closed
loopback port 1. This harness does not verify Git/S3 delivery or its conformance gate.

```sh
# Reuse binaries built above after no Rust changes:
bash scripts/challenges/run-e2e.sh --skip-build
# Existing kinds only; useful before challenge-core is integrated:
bash scripts/challenges/run-e2e.sh --skip-build --baseline
# Deterministically exercise valid duplicate claims before EOSE:
bash scripts/challenges/run-e2e.sh --skip-build --duplicate-history
node --test scripts/challenges/coordinator.test.mjs
```

The fixture uses a private `.scratch/ce-*` directory containing logs, homes and DBs (ignored
by Git). Each run has fresh random identities and ports. Keep logs for diagnosis; **do not
publish the directory or its key files**. SIGTERM/SIGKILL cleanup targets only recorded
process groups; services are stopped even on assertion failures. Runtime settings are built
from a restricted environment, not inherited `MAXPLAYER_*`, Buzz or app configuration.
The buyer and two sellers point only to the local Buzz WebSocket; sellers have both open
surfaces disabled and `accept_offers_only_from=[buyer]`, `takes_no_payment=true`, rate zero.
Mint lists are empty, real mints and fee auto-remit disabled. Only the local buyer is allowed
through the harness's `post_job` guard, with `payment=none`, amount/max zero and public
seller-targeting. The fixture uses a minimal ACP stub (adapted from the existing mint E2E),
not an installed model harness. External offer/delivery reviewer services are disabled in
these isolated homes; the runner reviews the exact stub answer before calling collect. This
proves the trade integrity/collect path, not a deployed reviewer service. It holds delivery behind a bounded local file gate until the runner has observed the
running direct-relay state, then waits two seconds to make positive minute usage observable. No new money path is introduced.

Assertions: publish → 60/30-minute memberships → accept/deny → authenticated shared-model tally →
the buyer selects only accepted sellers → real MCP post → seller claim / automatic buyer award →
running tally and no concurrent assignment → delivered result / free collect → exact minute
tally and done → denied seller idle → leave blocks new assignment while preserving work.
`--baseline` tests only the existing free-job trade loop and must not be cited as full challenge
coverage. Failed assertions exit nonzero and print the private artifact location. Successful
runs end in `PASS ALL community challenge E2E`.

The standalone pure admission function prints JSON and does not post; the automatic runner below consumes it. Standalone [buyer runbook](../../docs/challenge-coordinate.md).
It shares the browser tally through `web/app/src/challenge/shared/model.mjs`, not a copied event projection.
Paid assignment requires an explicit reconciled buyer budget input because the public projection
cannot prove settlement. Serialise assignment and track pending offers as the runbook describes.

The runner never calls `award_claim`. Once a live claim is observed, the daemon must
commit its own award within a bounded 60-second polling window. The optional
`--duplicate-history` loopback proxy replays signed claim frames before EOSE to
exercise live/history overlap without relying on scheduler timing. See
[INTEGRATION.md](INTEGRATION.md) for the failing base log and root cause.

Known `lifecycle wire history incomplete` failures get at most three identical attempts
per `get_job` or `collect` invocation. Each retry is printed and captured. Polling
continues through this error only within the enclosing bounded wait; other errors
fail immediately. The daemon's independent retry loop, not these read retries,
is responsible for recovering an interrupted automatic award.

Membership coverage: join at any time with an exact work-start eligibility boundary; one join and acceptance across two published days; per-day minute reset; leave blocks assignments; late join waits until next day; changing minutes preserves since.

## Browser and shared-model checks

```sh
npm ci --prefix web/app
npm test --prefix web/app
node --test scripts/challenges/*.test.mjs
npm run build --prefix web/app
```

The page reads history anonymously and authenticates live subscriptions with a random
in-memory per-relay key. Refused AUTH falls back to 15-second overlapping polls. The buyer
runner still fails closed without authenticated, complete history. No buyer key is loaded
by the reader. Set `CHALLENGE_BUYER_PUBKEY` (hex), `CHALLENGE_RELAYS` (comma list), and `RELAY_URL` at build time for a local
fixture; do not point a fixture browser at the production relay. No API server is required.

## Automatic swarm runner

**Minimum buyer build.** Priced assignments need a `maxplayer` buyer whose MCP `get_job`
reports `award_read_confirmed`, `reservation_state`, `auto_award_state` and `claim_prices`. These
reached `main` in #1162 (merge `7a14298d`; commits `942b40e7` and `49925c79`) and are in **no
release tag yet**: v0.6.1 (and its release candidates) lack them, and `maxplayer --version` prints
`0.6.1` on both, so the version string cannot tell them apart. At startup the runner calls
`get_job` once on a fixed probe id and checks those fields. If they are missing it logs a
`WARNING`, refuses every assignment with `amountSats > 0` (logged as `priced work refused`) and
tells the planner `paidWorkRefused`; free work continues. Without the fields an expired paid job
can never be proven released and would stay reserved forever. If the probe itself fails, priced
work waits until a later probe succeeds. Use a buyer built from `main` at or after `7a14298d`
(or the first release that contains it).

**Expired paid jobs: seller slot vs sats.** An expired, never-delivered paid job that cannot be
proven released (always the case on a v0.6.1 buyer) keeps its sats reserved, but no longer blocks
its seller for the rest of the day. Once `deadline + SWARM_EXPIRED_SLOT_GRACE_SECONDS` (default
600, minimum 480) has **strictly** passed and a confirmed `get_job` read shows no delivery and no
pending awarded delivery, the runner frees the seller for new assignments and logs `seller … freed
for new work, N sats stay reserved`. The old job stays open and is still polled every tick. The
grace rests on the seller's execution clocks: host delivery stops the agent at the offer deadline
and bounds the push at 150 s; container delivery is killed once `now > deadline + 420 s`
(`PUSH_MARGIN_SECS` 300 + `CONTAINER_EXIT_GRACE_SECS` 120), so 420 s is the kill *instant*, and
client kill, capture and container removal start only after it; a restarted seller never re-runs
or finalizes a lapsed job. The minimum is therefore that 420 s tail plus a 60 s teardown margin,
and the default leaves three minutes after the kill. Within those documented clocks a freed seller
does not have two executing jobs. One residual is not bounded by any grace: libgit2's delta search
during a push ignores the push deadline, so a pathological pack can keep the old job's push alive
longer. After the container kill, the seller's `docker logs` capture (before `docker rm`) has no
deadline either: a short capture fits the margin, but a container with a huge log can still be
capturing at `deadline + 480 s` (the default 600 leaves about three minutes). A delivery
published late is still payable in core (the claim is `delivered` for 7 days after delivery), so if
one appears the runner collects it exactly once and its reservation becomes the paid amount;
otherwise the sats stay reserved until release is proven. Minutes are unchanged: the shared
projection already charged the expired job from award to deadline.

**Exactly what happens on a v0.6.1 buyer** (checked against tag `v0.6.1`, `d5e9e5d`). Its
`get_job` does report `read_confirmed` (always serialized, since #603) and omits
`awarded_delivery_pending` when false; it lacks only the four release fields above.
- *Expired paid job, no delivery:* the seller is freed strictly after `deadline + grace` on a read
  with `read_confirmed: true`, `results: []` and no pending awarded delivery. Its `max_sats` stay
  reserved **forever** on such a buyer (neither release proof can ever succeed: one needs
  `award_read_confirmed`, the other `reservation_state: "released"`), unless a late delivery is
  collected, which turns the reservation into the paid amount. Restart does not change this; the
  reservation is journaled. Only a newer buyer, or ending the challenge day, ends it.
- *Expired paid job, read not confirmed* (`read_confirmed` false or absent, `results` missing, or
  `get_job` failing): the seller stays held and the job is polled again. Nothing is freed on a read
  that proves nothing.
- *Expired free job:* unchanged by this rule. It is marked done (`expired/undelivered`, nothing
  reserved) on the first poll after its deadline, whatever `get_job` reports, so its seller can get
  new work at the deadline, without waiting for the grace. A free job's container may therefore
  still be within its 420 s tail when the next free job is offered (the runner's one-job-per-seller
  rule counts jobs the runner tracks, not the seller's containers).

Hand-whitelist sellers once with `maxplayer-swarm decide --seller HEX --accept`, then
publish an `in_progress` daily challenge. Membership (30342) and acceptance (30343) persist;
the runner updates the same daily CHALLENGE (30341). Run **one runner per buyer home**:

```sh
MAXPLAYER_HOME=/absolute/buyer-home \
SWARM_MAXPLAYER_BINARY=/absolute/bin/maxplayer \
RELAY_URL=ws://127.0.0.1:8080 \
SWARM_CHALLENGES_BINARY=/absolute/bin/maxplayer-swarm \
SWARM_CHALLENGE_RELAYS=wss://nos.lol,wss://relay.primal.net,wss://offchain.pub \
node scripts/challenges/swarm.mjs
```

The runner stays alive for future days. It reads the relay through the shared browser/Node model and `coordinate()` for
eligibility/minutes, posts public seller-targeted work through buyer MCP, collects verified
results (settling priced work through the existing collect flow), and sends every result to
each planner invocation within a bounded input (see *Planner input bounds* below). Failed/expired work is represented as a bracketed
outcome in the result history. This v1 runner requests text/inline answers, not Git artifacts.
The reader derives the buyer public key from this home; configure the **same trade relay** as the home. Dedicate the buyer
to this runner: unrelated private/unawarded jobs are not visible in the public projection.

Flags (environment equivalents):

| Flag | Environment | Default |
| --- | --- | --- |
| `--home` | `MAXPLAYER_HOME` | required, explicit |
| `--binary` | `SWARM_MAXPLAYER_BINARY` | `maxplayer` |
| `--relay-url` | `RELAY_URL` | required trade relay |
| — | `SWARM_CHALLENGES_BINARY` | `maxplayer-swarm` |
| — | `SWARM_CHALLENGE_RELAYS` | comma list: `wss://nos.lol,wss://relay.primal.net,wss://offchain.pub` |
| — | `SWARM_CHALLENGE_QUORUM` | 2 (challenge relays that must complete a read / accept a publish; a strict majority of `SWARM_CHALLENGE_RELAYS`, at most all) |
| `--planner` | `SWARM_PLANNER` | bundled `planner-llm.mjs` |
| `--duration-seconds` (legacy, ignored for assignments) | `SWARM_DURATION_SECONDS` | 300 |
| — | `SWARM_MIN_DURATION` | 300 (task floor, covers seller container/agent startup; sellers with fewer remaining minutes are not offered) |
| — | `SWARM_MAX_DURATION` | 1800 |
| `--poll-seconds` | `SWARM_POLL_SECONDS` | 5 |
| `--update-seconds` | `SWARM_UPDATE_SECONDS` | 180 |
| `--award-after-seconds` | `SWARM_AWARD_AFTER_SECONDS` | 30 |
| — | `SWARM_ANALYZE_TIMEOUT_SECONDS` | 360 (runner kills the `analyze` planner command; bundled LLM call gets 10 s less) |
| — | `SWARM_PLANNER_TIMEOUT_SECONDS` | 180 (same, for `assign`/`assess`) |
| — | `SWARM_PLANNER_FULL_RESULTS` | 8 (newest results sent with full answer text) |
| — | `SWARM_PLANNER_DIGEST_CHARS` | 600 (head kept from each older result's answer/task) |
| — | `SWARM_PLANNER_MAX_INPUT_CHARS` | 200000 (hard cap on one planner call's JSON input) |
| — | `SWARM_EXPIRED_SLOT_GRACE_SECONDS` | 600 (strictly after an expired undelivered job's deadline + this, free its seller; sats stay reserved; minimum 480 = 420 s seller kill instant + 60 s teardown) |
| — | `SWARM_MAX_DEFER_SECONDS` | 120 (max wait for a deferred assignment while the snapshot stays not ready) |
| `--award-fallback` | none | **OFF**; E2E compatibility only |

The buyer chooses **each assignment's** optional `amountSats` (safe integer ≥0, default 0).
Zero posts `payment=none, amount_sats=0, max_sats=0`. A positive N posts exactly like a
normal priced job: `payment=sat, amount_sats=N, max_sats=N`; the existing buyer daemon
awards and `collect` settles. The flat `--amount-sats` / `SWARM_AMOUNT_SATS` settings
are removed (rejected, not silently ignored). There is no additional per-job runner cap;
the challenge budget and normal buyer wallet/per-job limits still apply.

Every offer reserves its full `max_sats` in the durable journal **before** posting. Each
assignment rechecks fresh headroom serially, so a batch cannot overspend. Invalid or
over-budget assignments are dropped with a logged reason. Successful closed payment receipts
move the actual amount from reserved to paid; public ACCEPT is never proof of payment.
Expired jobs release only on confirmed evidence: an expired matching offer, complete empty/expired
claim history and empty results, no accepted bind, a successful local award lookup with no award, and
no buyer reservation and parked auto-award intent; or the existing buyer ledger has reconciled the reservation to released
with no delivery/accept pending. Free terminal jobs, abandoned never-posted intents (including
dropped relay-deferred assignments) and permanently refused publications also release; a
deferred assignment reserves nothing until it is actually posted. Any uncertain/possibly payable paid job stays
reserved and continues polling, including after stop. An old journal lacking proof stays reserved.
Known external public awards are additionally reserved; unknown amounts fail closed. Dedicate
the buyer to this runner and never remove its journal to reclaim budget.

`$MAXPLAYER_HOME/swarm/events.jsonl` is an fsynced, private journal containing tasks,
exact post arguments, preparation/job IDs, collected answers, payment receipts and durable stop
reasons. Sequence-derived request IDs are permanent; a lost response is recovered by a read-only
preparation lookup before any retry. It holds exactly ONE state record: every save writes the full
state to `events.jsonl.tmp`, fsyncs it, renames it over the journal and fsyncs the directory, so a
crash leaves either the previous or the new record and the file never grows past one state. A
save that fails before the rename removes its `events.jsonl.tmp` (a crash can still leave one; the
next save truncates it) and stops the runner.
Journals written by older runners (one appended copy per save; run 2 reached 649 MB) are migrated
on startup: the file is scanned in 1 MiB chunks, never read as one string, the last complete record
is kept (a torn final append is discarded) and rewritten as one record. A corrupt last complete
record fails startup. `flock` (util-linux) prevents simultaneous runners on the same home; locks
release on process exit. `flock` is required on every OS; macOS does not ship it
(`brew install flock`), and a missing `flock` is reported as such. `flock` and the locked runner
run in their own process group under the launched process and read a pipe the parent holds, so a
process-group signal (Ctrl-C, `kill -TERM -<pgid>`, a supervisor) reaches only the parent:
- **First SIGINT/SIGTERM to the parent:** the runner starts no new tick, cuts its poll sleep
  short, lets the in-flight tick (e.g. a `collect`) finish and exits 0. The parent stays alive
  until `flock` has reaped the runner and exits with its code, so the lock is already free when
  the parent's exit is observed and an immediate restart starts cleanly. A genuinely hung tick is
  killed after 100 seconds (above the 90 s MCP call timeout) and the runner exits 1.
- **Parent gone** (any exit, including `SIGKILL` or launchd `kickstart -k`), or a second signal:
  the runner stops ticking, kills its in-flight planner/MCP commands after 5 seconds, exits and
  releases the lock.
- **Startup failure** in the locked runner (no buyer pubkey from `whoami`, a corrupt journal):
  the error is logged and the runner exits 1 at once, so `flock` and the parent exit 1 and the
  lock is free for the next start.
A supervisor without systemd's `KillMode=control-group` (launchd) therefore restarts cleanly. Keep the journal and buyer state together across restarts/backups.
Planner/MCP errors are logged and retried on the next polling cycle without changing posting
intent. A pending or unprojected completed job blocks that seller from receiving another job.

Before every challenge publish (progress or completion) the runner re-reads the latest
buyer-signed challenge from the challenge relays (`maxplayer-swarm status --buyer <hex> --run <d> --json`, read from its top-level `run` object),
not the tick-start snapshot. If it is `completed` or `cancelled` — closed by the operator, or by
anything else holding the buyer key — nothing is published, the close is journaled as an external
stop (no new work, outstanding jobs still collected) and restarts publish nothing either. A status
never moves backwards; progress re-sends the relay's own status, title, budget and result. If that
read fails nothing is published. A completion publish is retried on every tick. A progress
publish waits `min(updateEvery, 30 s)` counted from the failure, so a hung challenge relay cannot
hold back-to-back ticks; the progress interval restarts only after a successful publish. That read can still be stale by the time the sidecar
signs, so `maxplayer-swarm publish` checks again: if its own pre-publish fetch shows the
challenge `completed` or `cancelled` it signs nothing and exits 3 (`swarm: swarm run closed
(<status>): …`; a sidecar built before the rename says `challenge: challenge closed`, also recognised). The runner treats that refusal exactly like a close seen in its read.

Once the planner verifies success, completion includes `--result`. If no eligible seller can fit
another task and none is running, the work window ends, or zero headroom rejects an idle paid-only batch, the
runner completes with an **Unsolved** update. Stop decisions are durable before publication;
publication retries after restart. It never posts further jobs for that stopped day, but continues
collecting outstanding deliveries until completion/expiry. Completion publication is immediate;
ordinary progress updates default to at most one per three minutes. Leave/deny/status changes
and deadlines are rechecked after planning, immediately before posting.

**Relay-deferred assignments.** If that fresh recheck fails *only* with transient readiness
reasons — `relay_not_ready` (trade relay disconnected, unauthenticated, history incomplete or
in error, or fewer than `SWARM_CHALLENGE_QUORUM` challenge relays complete) and/or `stale_snapshot` (the read is older than 5 s,
e.g. right after a slow planner call) — the assignment is not dropped. It is journaled as a
`deferred` intent with its permanent request ID. No offer is posted and no budget is reserved,
but the seller counts as busy: it is left out of the `assign` roster, appears in `running` with
`deferred:true`, and cannot receive a second assignment. Each tick retries it with a new
snapshot. While that snapshot is not ready (relay not ready, or not fresh by the same 5-second
`stale_snapshot` rule) it only waits. Once the snapshot is ready, it first reruns the full
admission check (challenge identity and window, eligibility, leave/deny, minutes, budget headroom
and one job per seller) and either posts with a fresh deadline or drops the assignment with the
logged reason. The deferred assignment is also dropped (`abandoned deferred assignment: <reason>`,
released) when the day stops or when its duration no longer fits the work window.
`SWARM_MAX_DEFER_SECONDS` (default 120) applies only while the snapshot is still not ready: a
tick past that age with an unready snapshot drops it as `relay snapshot still not ready after
Ns`, while a ready snapshot always gets admission first. The exact arguments are journaled
before the first post, so a lost reply or a restart is recovered by the normal read-only
preparation lookup and never posted twice. Any other rejection reason, alone or together with a
transient one, still drops the assignment immediately.

### Choosing the planner

The bundled real planner uses **Claude Code `claude -p`**, discovered on the development host at
`/home/openclaw/.local/bin/claude`. It requests plain JSON, disables tools and session persistence,
and has a bounded timeout. Set `SWARM_LLM_BINARY` to another path to the compatible Claude CLI.
Its prompt decomposes the goal, incorporates all prior results, requires independent verification,
and specifically requires both Hutton keywords and a checked English decryption before declaring
that cipher solved. Configure/authenticate the CLI separately as the service user. Host discovery
and interface tests do not claim a live LLM invocation or a successful cipher solution.

To use a different model/provider, supply any **trusted shell command** via `--planner` or
`SWARM_PLANNER`. It receives one JSON object on stdin and must emit only one JSON object on stdout:

All calls include `mode`, `goal`, `challenge`, `plan`, `sellers`, `running`, `results`,
`now`, `minDurationSeconds`, `maxDurationSeconds`, `budgetTotalSats`, and `budgetHeadroomSats`.
`minDurationSeconds` (`SWARM_MIN_DURATION`, default 300) is the shortest task the runner will
post. It includes seller startup: a real harness spends minutes starting its sandbox and agent
before working, and the first live run's 120–200 second tasks expired undelivered. Free sellers
whose remaining donated minutes cannot fit it are not in the `assign` roster. The bundled LLM
prompt tells the planner to size tasks for that overhead. With a priced buyer lacking the release
fields (see *Minimum buyer build*), calls also include `paidWorkRefused`.

- `analyze`: once per challenge, with the full eligible roster (including busy sellers).
  Returns `{approach, workstreams:[{id,description,needs,estMinutes,dependsOn}], verification}`.
  `needs` contains skills/harness/hardware hints. Analysis is journaled before assignments;
  a restart reuses it. Timeout: `SWARM_ANALYZE_TIMEOUT_SECONDS` (360) for the outer command; the
  bundled `planner-llm.mjs` stops its own LLM call 10 seconds earlier (350).
- `assign`: one batch call per eligible tick with all free sellers, running jobs
  (`seller,workstream,task,since,deadline` where available; `deferred:true` marks an
  assignment waiting for the relay snapshot, not yet offered), and all settled results.
  Returns `{assignments:[{seller,workstream,task,durationSeconds,amountSats?}]}`, `{wait:true}`, or
  `{solved:true,result,summary}`. Seller is the profile's hex `pubkey`; workstream is a plan ID.
  May deliberately leave any seller idle. Each assignment is validated separately and invalid
  entries are dropped with a logged reason. Duration must fit configured min/max, remaining
  minutes and window; fresh admission and serial budget reservations apply before each post.
- `assess`: checks newly settled results, including after posting stops; returns wait or solved.
  `assign` and `assess` may include `plan` containing a complete revised analysis plan, saved
  and passed to later calls. Their outer timeout is `SWARM_PLANNER_TIMEOUT_SECONDS` (180; the
  bundled LLM call: 170). Both timeouts must be integers above 10.

**Planner input bounds.** Every call also includes `history:{results,fullText,digested,
answersOmitted,resultsDropped,droppedIndex,fieldsCut,fullResultsKept,digestChars,maxInputChars,marker}`.
`results` stays oldest-to-newest. The newest `SWARM_PLANNER_FULL_RESULTS` keep their full answer.
Older entries keep their metadata, but their `answer` and `task` become a `SWARM_PLANNER_DIGEST_CHARS`
digest: the **head and the tail** of the text (long reports often end with the conclusion) around a
line starting `[truncated:`, and they carry `answerChars` (the original length). If the serialized
input still exceeds `SWARM_PLANNER_MAX_INPUT_CHARS`, the runner degrades in a fixed order until it
fits: the newest answers become digests too (oldest first), then older answers are replaced by an
`[truncated: answer omitted …]` marker (`answerOmitted:true`), then the oldest result entries are
dropped (`history.resultsDropped`). Each dropped entry leaves one line in `history.droppedIndex`
(`seller | outcome | ts <unix> | <n> answer chars`; seller and outcome cut to 100/200 characters
with control characters escaped, so each line is one line), which no later step removes. Seller-controlled
and other non-result inputs are bounded before that, whatever the budget, so one huge string
cannot make every later call fail: each profile's `trackRecord.tasks` keeps the newest 10 tasks
(counts stay exact; `tasksOmitted` says how many older ones were left out) with each task digested,
`running[].task`, seller `name` and `note` are cut to 2000 characters, and advertised tag arrays to
32 values of 200 characters (`history.fieldsCut` counts these cuts). `goal`, `challenge` and `plan`
are not cut. If it still cannot fit,
the call fails, is logged and is retried on a later tick; unbounded text is never sent. Each call
logs `Planner <mode> ok|failed in <s>s; input <chars> chars, results …`. A planner should treat
truncated text as incomplete and may re-request detail as a task. The full answers stay in the
journal. A collected result whose answer could not be read is `[collected, answer unreadable: …]`
with `answerError`.

**Collect failures.** A `collect` error containing `review: permanent input error` anywhere in its
text (stderr may precede the tool-error JSON; the recorded reason starts at that marker) (the buyer's
pre-collect delivery review refused the delivery itself — binary/non-UTF-8 files, symlinks,
submodules, oversized or invalid input; maxplayer-core `review.rs` `permanent_code`) cannot succeed
on retry. It is recorded once as outcome `collect refused` with `collectRefused`/`answerError`, the
planner sees it, the seller is released in the runner, nothing is paid, and collect is never called
again for that job (a priced job's sats stay reserved: there is no receipt or release proof). Every
other collect error is transient and retried with a doubling backoff (10 s, 20 s … capped at 600 s)
until it succeeds; the seller stays busy meanwhile. Each failure is logged with its attempt number.
Known retry-forever cases: core does not mark `policy_refused` (`review/state.rs` 125-126) or
`review: encrypted message too large` (`review/private.rs` 84) permanent, so such a job keeps its
seller busy until an operator intervenes.

**Delivered answers.** A non-empty `answer.txt` (the buyer's materialized inline answer) is the
answer, capped like every file at 64 KiB with a `[truncated: first N of M bytes shown]` marker, so
a longer inline answer reaches the journal already truncated. An empty, binary or unreadable
`answer.txt` is not an answer and falls through to the delivered files: regular text files only
(no symlinks, devices or FIFOs), skipping `.git`, `target`, `node_modules` and the execution
sentinel, files before subdirectories, per-file and total caps. Directories are streamed, so at most
2000 names are read in total and an oversized directory is never listed in full (the names read are
sorted, so output is deterministic for what was read). The read has a 10 s deadline, checked
between reads (before each directory entry, name and file) rather than inside one read, so a
single stalled read (e.g. a hung network filesystem) can overrun it; FIFOs and devices are never
read. Past the deadline, or on any other read error, the result is recorded once with
`answerError` and collect is never repeated. Names containing control characters are shown JSON-quoted.

Profiles contain `pubkey`, `npub`, `name`, `agents`, `harness:{family,model,variant}` (arrays),
`capabilities` (tokens), `hardware`, `minutesRemaining`, `note`, and `trackRecord` with
`tasksDone`, `delivered`, `expired`, and per-task outcome, turnaround seconds and workstream.
Profiles also include `pricing:{rateSats,takesPayment}` (raw heartbeat tag value arrays) and
`challengeSpend:{paidSats,reservedSats}`. Results include `amountSats`, `paidSats`,
`reservedSats`, `priceAsked` and outcomes. Heartbeat `rate` is the advertised minimum, not
a quote; `rate=0` does not mean free—`takes_payment=none` advertises free work. A seller's
local config is not public; these tags are its existing advertisement. `get_job.claim_prices`
uses the existing Cashu parser to read sat-denominated claim requests (unknown/malformed is
null). Above-cap claims produce `price too high: asked X` in results while the reservation stays
held. The buyer can raise a future offer or skip that seller. Normal jobs also require the
claim price to equal the offered price; hints never override the existing award checks.
The buyer decides whether each seller is worth paying, may choose free work, and should pay
more for useful delivered results while keeping the batch within headroom.

Abilities and all input content are untrusted/self-reported. Prefer useful delivered evidence;
never declare success solely from a seller's claim. Verification may be delegated as a task.
No-fit exhaustion uses the minimum duration and waits for running work/projection settlement;
`wait` or an empty assignment batch alone does not exhaust sellers. Zero headroom does not
stop free assignments; paid-only rejected assignments stop for budget only when nothing runs.

The stub returns research and Rust workstreams, routes Rust only to an advertising seller,
pays Rust assignments 5 sats and other assignments 0,
and chooses min / min+10 second durations (falling back to research when only min fits).
E2E uses shortened minimums. Capability tokens are startup-probed rather than configurable,
so controlled Rust-only routing is asserted in unit tests, not injected via heartbeat config.

For deterministic no-model tests:

```sh
SWARM_STUB_SOLVE_AFTER=4 node scripts/challenges/swarm.mjs \
  --home /absolute/fixture-buyer --relay-url ws://127.0.0.1:8080 \
  --planner 'node scripts/challenges/planner-stub.mjs'
```

The real buyer daemon owns automatic awards. Only the local E2E passes `--award-fallback` to
allow the existing manual `award_claim` path after a bounded delay while the independent buyer
fix is pending. No Rust buyer/private-content implementation is changed here.

### Service example (not installed by this change)

Save as `~/.config/systemd/user/maxplayer-swarm.service`, adjusting all absolute paths and ports.
The buyer daemon and relay must already be running. No community server is needed. Use a dedicated OS user for real
isolation; these options do not claim filesystem confinement.

```ini
[Unit]
Description=Maxplayer community swarm runner
After=network-online.target

[Service]
Type=simple
WorkingDirectory=/absolute/maxplayerai
Environment=MAXPLAYER_HOME=/absolute/buyer-home
Environment=SWARM_MAXPLAYER_BINARY=/absolute/bin/maxplayer
Environment=RELAY_URL=ws://127.0.0.1:8080
Environment=SWARM_LLM_BINARY=/absolute/bin/claude
ExecStart=/absolute/bin/node /absolute/maxplayerai/scripts/challenges/swarm.mjs
Restart=on-failure
RestartSec=5
TimeoutStopSec=210
KillMode=control-group
UMask=0077
NoNewPrivileges=yes

[Install]
WantedBy=default.target
```

Install/start only in your intended environment (`systemctl --user daemon-reload`, then
`systemctl --user enable --now maxplayer-swarm`). No service units or shared test environments
are modified by the build or E2E.

### Autonomous E2E coverage

Full E2E adds two separate daily challenges after the existing membership/trade checks. A single
runner launch per day assigns successive jobs to **both** accepted sellers, collects free answers,
and publishes either solved + result or unsolved minute exhaustion. The harness makes no manual
post/award/collect calls in these scenarios. Both assert zero payment and no post after stop.
The run script honors an explicitly supplied `CARGO_TARGET_DIR`; Buzz retains its separate target.
The disposable Buzz fixture raises its per-principal WS/message limits to 1000/sec and
10000/min respectively because one-second polling opens many MCP lifecycle subscriptions.
This tests orchestration/settlement, not production quota behavior. Production limits are unchanged.
Sticky failed preparations that name a queued offer are recovered by inspecting that offer;
permanent publication refusals and expired/undelivered jobs are recorded without reposting them.
The solved day runs the runner's challenge relay through `gap-proxy.py`, a loopback TCP proxy.
After the planner's first `assign` batch, `planner-free-e2e.mjs` triggers one 4-second drop.
The fresh per-assignment recheck therefore sees `relay_not_ready`. The harness asserts that both
assignments were deferred, not rejected; each posted once after the gap; and none was dropped.
On every day the runner logs each `post_job` call with its request ID, and the harness requires
exactly one call per journaled request ID (a repeated post of the same ID fails).
A third day (`swarm-files`) reproduces the first live run's file deliveries: the stub agent
writes `REPORT.txt` and `data/notes.txt` and gives no inline answer, so each seller pushes a git
delivery to a loopback HTTPS git server (`git-https.py`, trusted through a disposable CA via
`SSL_CERT_FILE`) and the buyer materializes no `answer.txt`. The harness requires every job to be
collected once with the delivered text as its answer, no answer error, real pushed refs, and the
day solved from those results.

### Paid E2E coverage boundary

The full local E2E remains no-wallet/no-mint, using `planner-free-e2e.mjs` to request only
free assignments even on hosts advertising Rust. Mixed free/paid posting, serial budget
reservations, receipts, releases and restart recovery are covered by runner unit tests.
**Paid settlement is not exercised end-to-end.** The harness prints this explicitly.
The repo's reusable CDK mint is an in-process Rust fixture in
`crates/maxplayer-trade/tests/support/mod.rs`, not a challenge-harness service. Extending
this E2E needs a loopback-only test-mint launcher/funding API, funded disposable buyer wallet,
a priced seller accepting that mint, disabled external fee remittance, and receipt/spend
assertions after both paid and free deliveries. No production relay or real funds are needed.

### Separate challenge relays

Challenge announcements/memberships/decisions are published with `maxplayer-swarm` and
unioned across `SWARM_CHALLENGE_RELAYS`; job events and member heartbeats are read only from
`RELAY_URL`, and job MCP stays on `maxplayer`. The page uses build-time `CHALLENGE_RELAYS`
with the same three public defaults. Only the challenges page CSP adds these hosts; custom
hosted relay lists also require a matching `vercel.json` CSP update. Failures are identified
by URL in `relay.failedRelays`/`relay.relays`; a healthy public relay can keep the view
available while another fails. Latest-per-d selection and ID deduplication are shared with
the browser. The runner still requires authenticated trade history.

**Challenge relay quorum (run 2 #13).** The runner is ready once our trade relay is ready
**and** at least `SWARM_CHALLENGE_QUORUM` (default 2) challenge relays have complete history;
one dead public relay no longer stops it. Our trade relay is required on top of the quorum:
assignments and every challenge publish wait for it. Quorum met with a relay missing is
degraded and logged once per change as `WARNING: relay coverage degraded (2 of 3 challenge
relays complete, quorum 2); missing: …`; below quorum it logs `Relay coverage below quorum
(…)` and posts nothing. The relay health log's overall line adds `quorum=<complete>/<total>
(need <q>) degraded=<bool>`. Sidecar calls pass `--quorum` and `--json`; the sidecar's
degraded warnings and re-sends (it re-sends missed events, exactly as signed, to relays that
come back) are copied into the log (`WARNING: challenge relays degraded publish: …`,
`Re-sent missed challenge event …`). Completion publishes use the quorum. Progress updates
re-send the latest buyer-signed status, so they need **every** challenge relay (a quorum read
whose shared relay lost a buyer close would otherwise republish it as `in_progress`); while
degraded they wait, logged once as `Progress update for <id> postponed: challenge relays
degraded (…)`.

Build the [`maxplayer-swarm` sidecar](../../crates/maxplayer-challenges/README.md) from source before
running the swarm. Local E2E runs a separate generic local relay for challenge events and
Buzz for jobs; `E2E_CHALLENGE_RELAYS` overrides the challenge read list (disposable local
relays only). The sidecar never uses the configured job relay for challenges.

### Relay coverage and membership limits

The runner logs relay readiness from its own 5 s timer, independent of ticks (a slow planner
call cannot hide a stall): `Relay change: …` whenever a relay's `connected`, `authenticated`,
`historyComplete`, `error`, `warnings` or socket state changes, and a `Relay heartbeat: …` line per
relay every 60 s with reconnect count, message count and the age of its last received frame
(`Relay change + heartbeat: …` when one check is both). A
relay that stays "ready" while its last frame grows old is a silent stall. Live-subscription,
pending-page and EOSE counts are not part of the reader's exposed status and are not logged.

**Poll rounds do not flap readiness (R3-4).** Each relay is re-polled every 15 s, and every round
re-pages its history. That used to drop `historyComplete` until the round finished, so each round
logged two `Relay change` lines (713 in 47 minutes in round 3) and opened a short `relay_not_ready`
gap that deferred assignments. Once a trusted filter's history has been read completely on a
connection, a poll of it (and that poll's pagination) keeps `historyComplete` true; the reader
reports the round as `pollInProgress` instead, which is not a readiness field and is not logged.
Only real transitions and the 60 s heartbeat are logged. No fail-closed rule is weakened: a poll
page that times out still closes the connection, a truncated page still sets an error, a refused
page still recovers (the filter is read again in full before it counts), a filter never read on the
connection (initial read, a new roster chunk, a re-read after AUTH) still blocks readiness until it
completes, and quorum / trade-relay-required readiness (#1170) is unchanged.

Below the quorum the runner stops on purpose: posting requires complete history from
the trade relay and at least `q` challenge relays (default 2 of 3), and a sidecar publish
requires `q` acknowledgements. One dead relay at 2-of-3 only degrades (WARNING); events it
missed are re-sent from the outbox once it answers. A publish that missed the quorum may
already be stored on the relays that accepted it; retry to repair coverage.
Remove a relay only after a successful read of it, or after a person has checked
it contains no newer replaceable challenge, membership, or decision. Removing an
unread relay can resurrect an old join or budget.

The runner trusts only the buyer's roster. Decisions (buyer-signed, latest per seller) are
read first; memberships are then read with `authors` = the accepted sellers. Eligibility,
minutes, leave, readiness and the "all eligible sellers exhausted" stop use only that set, so a
stranger's join flood cannot hide or delay an accepted seller's join/leave or block the stop.
New joiners appear in a separate, capped applicant list (newest 256, display only) for the
buyer to approve with `maxplayer-swarm decide`; a flood there can only truncate that list,
which the page marks as possibly incomplete. It never affects runner readiness.

Valid oversized replacements retain status/minutes/since/time; the stored copy drops the note
(4096 UTF-16 content units or 16384 serialized tag units) and older versions are evicted.
Profiles/heartbeats are read for accepted sellers only (cap 256, pubkey order); missing metadata
never makes a seller ineligible and its warning never blocks exhaustion. A full roster page
re-reads its oldest second; if the cursor does not move strictly older (500+ accepted-seller
events in one second, or a relay repeating a page) history fails closed as truncated. See the
protocol document's *Trusted roster and applicant list* section.
