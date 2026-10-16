# Community challenges v1 buyer runner + local E2E

Run from the repository root (Node **24+**, Python 3, Cargo toolchain used by this checkout,
PostgreSQL 17 and Redis installed):

```sh
bash scripts/challenges/run-e2e.sh
```

The harness builds **this integration checkout** with wallet+ACP and Buzz in dedicated
`target/challenges-e2e` and `target/challenges-buzz` directories, installs the web app's locked
Node dependencies, and builds the separate `crates/maxplayer-challenges` workspace (binary and
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
in-memory per-tab key. Refused AUTH falls back to 15-second overlapping polls. The buyer
runner still fails closed without authenticated, complete history. No buyer key is loaded
by the reader. Set `CHALLENGE_BUYER_PUBKEY` (hex), `CHALLENGE_RELAYS` (comma list), and `RELAY_URL` at build time for a local
fixture; do not point a fixture browser at the production relay. No API server is required.

## Automatic swarm runner

Hand-whitelist sellers once with `maxplayer-challenges decide --seller HEX --accept`, then
publish an `in_progress` daily challenge. Membership (30342) and acceptance (30343) persist;
the runner updates the same daily CHALLENGE (30341). Run **one runner per buyer home**:

```sh
MAXPLAYER_HOME=/absolute/buyer-home \
SWARM_MAXPLAYER_BINARY=/absolute/bin/maxplayer \
RELAY_URL=ws://127.0.0.1:8080 \
SWARM_CHALLENGES_BINARY=/absolute/bin/maxplayer-challenges \
SWARM_CHALLENGE_RELAYS=wss://nos.lol,wss://relay.primal.net,wss://offchain.pub \
node scripts/challenges/swarm.mjs
```

The runner stays alive for future days. It reads the relay through the shared browser/Node model and `coordinate()` for
eligibility/minutes, posts public seller-targeted work through buyer MCP, collects verified
inline `answer.txt` results (settling priced work through the existing collect flow), and sends
**all** results to every planner invocation. Failed/expired work is represented as a bracketed
outcome in the result history. This v1 runner requests text/inline answers, not Git artifacts.
The reader derives the buyer public key from this home; configure the **same trade relay** as the home. Dedicate the buyer
to this runner: unrelated private/unawarded jobs are not visible in the public projection.

Flags (environment equivalents):

| Flag | Environment | Default |
| --- | --- | --- |
| `--home` | `MAXPLAYER_HOME` | required, explicit |
| `--binary` | `SWARM_MAXPLAYER_BINARY` | `maxplayer` |
| `--relay-url` | `RELAY_URL` | required trade relay |
| — | `SWARM_CHALLENGES_BINARY` | `maxplayer-challenges` |
| — | `SWARM_CHALLENGE_RELAYS` | comma list: `wss://nos.lol,wss://relay.primal.net,wss://offchain.pub` |
| `--planner` | `SWARM_PLANNER` | bundled `planner-llm.mjs` |
| `--duration-seconds` (legacy, ignored for assignments) | `SWARM_DURATION_SECONDS` | 300 |
| — | `SWARM_MIN_DURATION` | 120 |
| — | `SWARM_MAX_DURATION` | 1800 |
| `--poll-seconds` | `SWARM_POLL_SECONDS` | 5 |
| `--update-seconds` | `SWARM_UPDATE_SECONDS` | 180 |
| `--award-after-seconds` | `SWARM_AWARD_AFTER_SECONDS` | 30 |
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
with no delivery/accept pending. Free terminal jobs, abandoned never-posted intents and
permanently refused publications also release. Any uncertain/possibly payable paid job stays
reserved and continues polling, including after stop. An old journal lacking proof stays reserved.
Known external public awards are additionally reserved; unknown amounts fail closed. Dedicate
the buyer to this runner and never remove its journal to reclaim budget.

`$MAXPLAYER_HOME/swarm/events.jsonl` is an fsynced, private write-ahead journal containing tasks,
exact post arguments, preparation/job IDs, collected answers, payment receipts and durable stop
reasons. Sequence-derived request IDs are permanent; a lost response is recovered by a read-only
preparation lookup before any retry. Torn final journal records are discarded; corrupt complete
records fail startup. `flock` (util-linux) prevents simultaneous runners on the same home; locks
release on process exit. Keep the journal and buyer state together across restarts/backups.
Planner/MCP errors are logged and retried on the next polling cycle without changing posting
intent. A pending or unprojected completed job blocks that seller from receiving another job.

Once the planner verifies success, completion includes `--result`. If no eligible seller can fit
another task and none is running, the work window ends, or zero headroom rejects an idle paid-only batch, the
runner completes with an **Unsolved** update. Stop decisions are durable before publication;
publication retries after restart. It never posts further jobs for that stopped day, but continues
collecting outstanding deliveries until completion/expiry. Completion publication is immediate;
ordinary progress updates default to at most one per three minutes. Leave/deny/status changes
and deadlines are rechecked after planning, immediately before posting.

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

- `analyze`: once per challenge, with the full eligible roster (including busy sellers).
  Returns `{approach, workstreams:[{id,description,needs,estMinutes,dependsOn}], verification}`.
  `needs` contains skills/harness/hardware hints. Analysis is journaled before assignments;
  a restart reuses it. Timeout: 350 seconds for Claude, 360 for the outer command.
- `assign`: one batch call per eligible tick with all free sellers, running jobs
  (`seller,workstream,task,since,deadline` where available), and all settled results.
  Returns `{assignments:[{seller,workstream,task,durationSeconds,amountSats?}]}`, `{wait:true}`, or
  `{solved:true,result,summary}`. Seller is the profile's hex `pubkey`; workstream is a plan ID.
  May deliberately leave any seller idle. Each assignment is validated separately and invalid
  entries are dropped with a logged reason. Duration must fit configured min/max, remaining
  minutes and window; fresh admission and serial budget reservations apply before each post.
- `assess`: checks newly settled results, including after posting stops; returns wait or solved.
  `assign` and `assess` may include `plan` containing a complete revised analysis plan, saved
  and passed to later calls. Their outer timeout is 180 seconds (Claude: 170).

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

Challenge announcements/memberships/decisions are published with `maxplayer-challenges` and
unioned across `SWARM_CHALLENGE_RELAYS`; job events and member heartbeats are read only from
`RELAY_URL`, and job MCP stays on `maxplayer`. The page uses build-time `CHALLENGE_RELAYS`
with the same three public defaults. Only the challenges page CSP adds these hosts; custom
hosted relay lists also require a matching `vercel.json` CSP update. Failures are identified
by URL in `relay.failedRelays`/`relay.relays`; a healthy public relay can keep the view
available while another fails. Latest-per-d selection and ID deduplication are shared with
the browser. The runner still requires authenticated trade history.

Build the [challenge sidecar](../../crates/maxplayer-challenges/README.md) from source before
running the swarm. Local E2E runs a separate generic local relay for challenge events and
Buzz for jobs; `E2E_CHALLENGE_RELAYS` overrides the challenge read list (disposable local
relays only). The sidecar never uses the configured job relay for challenges.
