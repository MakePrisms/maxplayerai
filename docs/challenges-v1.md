# Community challenges v1

Status: opt-in `maxplayer-challenges` sidecar, built from source. Not live.

See [the crate README](../crates/maxplayer-challenges/README.md) for installation.
It has its own Cargo workspace and lockfile; build with `--manifest-path
crates/maxplayer-challenges/Cargo.toml`. The main `maxplayer` CLI has no challenge
subcommand. The buyer auto-award and job protocol are unchanged.

## Model

- **One buyer**, a pinned maxplayer buyer identity. Only jobs carrying the signed
  challenge task prefix belong to a challenge; ordinary buyer jobs are excluded.
- **Daily challenge.** The buyer publishes one signed CHALLENGE event per day.
- **Sellers join once over Nostr** with their own seller key, offering minutes per day until they leave.
- **We accept or deny** each member once; acceptance persists until denied with a signed DECISION event.
- **The buyer** (through its swarm runner) posts ordinary *public, targeted* jobs to accepted
  sellers, **one task at a time per seller**, unlimited in parallel across sellers. It only assigns
  a task whose `deadline` fits the seller's remaining minutes. Pricing, award, review, delivery
  and payment are the existing job flow, unchanged.
- **The browser** reads the relay directly and tallies minutes with the same shared model as the buyer runner. No community server is needed.
  Neither the buyer nor the seller publishes any metering.
- Seller prerequisite (unchanged mechanism): the seller lists the buyer's hex key in
  `seller.accept_offers_only_from`.

## Relay separation

Kinds 30341–30343 use public relays, by default `wss://nos.lol`,
`wss://relay.primal.net`, and `wss://offchain.pub`. Repeat `--relay URL` after the
sidecar command to override. Only loopback permits `ws://`; `relay.maxplayer.ai`
is refused. Publish attempts all relays and requires acknowledgement from a **quorum**
of them (default 2; `--quorum N`, `SWARM_CHALLENGE_QUORUM`, `CHALLENGE_QUORUM`), with
per-relay outcomes on stderr. Reads union verified events from every relay that completed and
deduplicate ids, then select latest per coordinate. No EOSE is a per-relay error (discard that
partial response); fewer than the quorum completing is a command error. See
[Challenge relay quorum](#challenge-relay-quorum-run-2-13).

Jobs remain on the configured trade relay. Buzz's allowlist is unchanged from main:
no 30341–30343 support is added and production relay deployment is unnecessary.
The local E2E runs a separate generic Nostr relay for challenge metadata.

## Events

All three are NIP-01 addressable (30000–39999): the relay keeps only the newest per
`(pubkey, kind, d)`, so republishing replaces. All carry `["t","maxplayer"]` and `["v","1"]`.
`<buyer>` and `<seller>` are lowercase 64-hex pubkeys. Times are unix seconds.

### 30341 CHALLENGE (author: the buyer)

| Tag | Value |
|---|---|
| `d` | challenge id, `[a-z0-9-]{1,64}`, e.g. `2026-10-16` |
| `title` | short title |
| `starts` | unix seconds (work start) |
| `ends` | unix seconds, `> starts`; the buyer posts no jobs after this |
| `budget` | `<sats>`, `"sat"` — total spend cap, non-negative integer |
| `status` | `scheduled` \| `in_progress` \| `completed` \| `cancelled` |

`content`: the job description shown in the page's "The job" section. Optional first line `# <headline>`; paragraphs separated by blank lines; a paragraph whose lines all start with `- ` is a list; bare `https://` URLs become links. Nothing else is interpreted (no HTML). `title` is for tools and the review screen only; the public page always shows "Community Swarm" (no numbering). Optional `["update", <text>]`: latest progress note.
Optional `["result", <text-or-url>]`: final outcome.

Coordinate: `30341:<buyer>:<d>`.

### 30342 MEMBERSHIP (author: seller)

| Tag | Value |
|---|---|
| `d` | `<buyer>` (one per seller per buyer, independent of daily challenges) |
| `p` | `<buyer>` |
| `minutes` | integer `1..=480` — active minutes offered **per day** |
| `status` | `joined` \| `left` |
| `since` | unix seconds of first join in the current membership period |

No `a` tag or challenge id. `content`: optional seller note (untrusted).
Republish `joined` to change minutes, preserving `since`. Republishing replaces the event,
so its `created_at` moves to now: a change after a challenge's start makes the seller
ineligible for that challenge (rule below) and takes effect from the next one. The sidecar
therefore refuses minutes/note changes while such a challenge is running (see the CLI
section below). Joining after `left` starts a
new membership period with a fresh `since`. Leave at any time by republishing `left`:
no new assignments, but already-awarded jobs finish, deliver and settle normally.

### 30343 DECISION (author: the buyer)

| Tag | Value |
|---|---|
| `d` | `<seller>` (persistent per seller) |
| `p` | `<seller>` |
| `decision` | `accepted` \| `denied` |

No `a` tag. Optional `["e", <membership-event-id>]`. `content`: optional reason.
Acceptance persists across challenges until the buyer republishes `denied`.

### Validation and eligibility (every reader)

1. Verify event id + signature locally; never trust the relay.
   The web/runner reader (`events.mjs` `verify`) recomputes the id from every received event,
   then caches a successful Schnorr check on (id, pubkey, sig). The cache is process-wide (all
   relays share it) and holds at most 100,000 entries (`VERIFY_CACHE_LIMIT`, ~40 MB of heap
   measured on Node 24). While the distinct verified events fit, a poll overlap or a full
   re-read after reconnect costs one hash per event and no signature check. When full, a new
   entry replaces a random one, so a stable re-scan larger than the cap verifies only some
   events again (about 2 per pass at cap + 1) instead of all of them, as LRU would. Failed
   checks are never cached.
2. CHALLENGE and DECISION count only if authored by the configured buyer.
3. MEMBERSHIP requires `d = p = buyer`, valid minutes, status and `since`; it does not
   require any published daily challenge. MEMBERSHIP and DECISION must have no `a` tag.
4. Ignore invalid events before replacement. For each `(pubkey, kind, d)` keep the highest
   `created_at`; ties use the lowest id (NIP-01).
5. Unknown values, missing/duplicate required tags, or out-of-range numbers are invalid.
6. For challenge C, eligibility requires the latest membership to be `joined`,
   `created_at <= C.starts` and `since <= C.starts`, and the latest buyer decision to be `accepted`.
   Sellers can join at any time. Joining after work starts takes effect from the next
   daily challenge; there is no grace period after starts. Late memberships remain valid.
   Leaving has **no timestamp cutoff** and stops new assignments immediately.
   The ±900-second drift tolerance applies only to event `created_at` validation,
   never to membership eligibility.

## Minute tally (one shared browser/Node model)

For a challenge `C` with buyer `B`, seller `S`:

1. Jobs: OFFER (3401) authored by `B` with `p=S`, the challenge task prefix described below, and `created_at` in `[C.starts, C.ends]`.
2. Start: the AWARD (3405) authored by `B` for that offer whose claim is authored by `S`.
   Start time = award `created_at`.
3. End: the earliest buyer-signed ACCEPT for the selected claim at/after AWARD,
   otherwise now, capped at deadline. A seller RESULT never closes the minute meter;
   the job stays **running** (seller occupied) until ACCEPT or deadline, even after RESULT.
   A RESULT at or before AWARD is untrusted: use the full deadline only when no valid
   ACCEPT exists. ACCEPT takes precedence. A future RESULT never charges beyond now.
4. Job minutes = `min(end − start, offer.deadline − start)`, floored at 0.
5. Seller used = sum over its jobs. Remaining = `membership.minutes − used` (reset for each daily challenge).
6. Ignore RESULTs by anyone other than `S`, and AWARDs by anyone other than `B`.

Known limit: `created_at` is signer-chosen, not independently measured CPU time.
Backdated results at/before award without ACCEPT charge the full deadline; otherwise only a
buyer-signed ACCEPT stops accrual. All durations are capped at deadline.

## The buyer’s rules

- Post only while `now ∈ [C.starts, C.ends]` and `C.status = in_progress`.
- Target only eligible sellers with no running job, and only a task with
  `deadline − now ≤ remaining minutes`.
- Skip a seller whose newest heartbeat says `status=not_serving` or `serving=0/N` (reason
  `seller_not_serving`). A missing heartbeat or tag is unknown and stays assignable. Such a seller
  holds the day open (not "exhausted") only while `seller_not_serving` is its sole reason and a
  minimum task still fits the work window; short of minutes or out of window, it does not.
  The wire cannot yet tell a re-probed dropped harness from a never-probed incapable one, so a
  permanently incapable seat that would otherwise be a candidate (its sole reason is
  `seller_not_serving` and its minimum task fits before the window ends) holds the day open
  until the window ends.
- Budget: committed (awarded, unsettled) + paid ≤ `C.budget`. Stop posting when the next task
  would exceed it.
- Public jobs, `seller_pubkey` targeted, existing review/collect/pay.

The buyer’s executable runner is `node scripts/challenges/swarm.mjs`; `coordinator.mjs`
remains its pure admission decision function. Sellers are manually accepted once, then the
runner automatically assigns successive tasks until solved or exhausted. Deadlines must fit
both the remaining donated minutes and the remaining work window. There is at most one
pending/running task per seller, including offers not yet visible in the public projection.

The planner is an external JSON stdin/stdout command. Once per challenge, `analyze` receives
its goal and full eligible roster and returns an approach, workstreams (needs, estimates,
dependencies), and verification plan. The durable journal preserves this analysis across restarts.
Each tick `assign` receives the plan, all free eligible seller profiles, running jobs and the
result history (bounded: the newest results in full, older ones as marked head-and-tail digests,
under a hard per-call character budget described in the runner README; a result dropped to fit
still leaves a seller/outcome/time index line, and seller-supplied profile text is capped), and returns a batch of assignments
`{seller,workstream,task,durationSeconds,amountSats?}`, wait, or verified solved answer.
The buyer decides whether each seller is worth paying. `amountSats` is a safe integer ≥0,
default 0: zero posts `payment=none, amount_sats=0, max_sats=0`; N>0 posts a normal
priced job with `payment=sat, amount_sats=N, max_sats=N`. Flat `--amount-sats` and
`SWARM_AMOUNT_SATS` are removed. There is no extra runner per-job cap beyond budget.
Planner input includes `budgetTotalSats`, `budgetHeadroomSats`, per-seller
`challengeSpend:{paidSats,reservedSats}`, outcomes, and advertised heartbeat `rate` /
`takes_payment` values in `pricing`. Claim Cashu requests are parsed by the existing buyer
parser; above-cap prices appear in results as `price too high: asked X`. Prices are hints,
not award authority; normal exact-price/mint/payment-mode checks remain unchanged.
Free is fine; pay more for useful results, raise a future price or skip a seller, and keep
all assignments in the batch within headroom.
The runner enforces each paid seller's advertised floor in code: a seller is free only when its
heartbeat says `takes_payment=none`; otherwise an assignment below its `rate` (and any 0-sat
assignment, `rate=0` included) would never be claimed, so it is rejected with `below seller
minimum N`, or, with the default-off `SWARM_AUTO_RAISE_TO_RATE`, raised to the minimum within
headroom. Rejected assignments of the last `assign` batch, with their reasons, are returned to
the planner as `rejectedAssignments` on its next calls.
A seller whose assignments keep expiring with no claim (default 2 in a row on confirmed reads; a
dropped harness looks exactly like this from the buyer) is benched for a cooldown (default 30 min):
excluded from the assign roster and listed to the planner as `benchedSellers` with the reason. Any
claim by it, or the cooldown ending, un-benches it; a benched seller is not counted as exhausted.
`assess` checks newly settled results even after posting stops. Later responses may revise the plan;
an assignment's workstream is checked against the plan sent with that `assign` call or the revision
returned with it, never only the plan saved after the call.
Profiles include npub/name, self-reported harness/capabilities/hardware, remaining minutes, notes,
and this-challenge delivery/expiry/turnaround/workstream history. The planner may leave sellers idle
and prefer useful past deliveries. Every assignment is revalidated against fresh eligibility,
availability, budget, donated minutes and work-window limits. An assignment whose only failing
checks are transient readiness (`relay_not_ready` and/or `stale_snapshot`) is journaled as
deferred rather than dropped: it posts nothing and reserves no budget, but it still blocks a
second assignment to that seller. It is retried with full admission once a new snapshot is ready
and fresh. It is dropped with a logged reason when another check fails, the day stops, the work
window no longer fits, or `SWARM_MAX_DEFER_SECONDS` (120) passes while the snapshot is still not
ready. An expired, never-delivered job that cannot yet be proven
released keeps its sats reserved, but frees its seller for new work after
`SWARM_EXPIRED_SLOT_GRACE_SECONDS` (600; minimum 480, the seller's 420 s container kill instant
plus a teardown margin) strictly past its deadline once a confirmed read shows no delivery;
a late delivery is still collected exactly once. Minutes are not recounted. Durations must fall within
`SWARM_MIN_DURATION` (300 seconds, a floor that covers seller sandbox/agent startup; a seller
whose remaining minutes cannot fit it gets no assignment) and `SWARM_MAX_DURATION` (1800 seconds).
Exhaustion means no free seller can fit the minimum duration and no work is running; choosing
wait alone never exhausts a challenge. Bundled planners are a deterministic capability-routing
stub and non-interactive Claude; verified-solution rules remain, including both Hutton keywords
and checked English decryption. Verification can itself be a delegated task.

Tasks/receipts/results and stop decisions are journaled under the buyer home. Permanent
request IDs derive from (buyer, challenge ID, seller, sequence); exact arguments survive
restarts, including a lost post response. The existing buyer daemon awards automatically;
a default-off E2E-only fallback permits bounded manual award while its separate fix is pending.
`collect` uses the existing verification/settlement path; no new payment protocol is introduced.

Progress republishes the same CHALLENGE fields with a short update (default every 180 seconds).
Solved completes with result; exhausted minutes, work window or budget completes with an
unsolved reason. A stopped day never receives new posts, but outstanding work is still
collected. The process remains alive for subsequent days. All local offers reserve their full
price serially before posting, including unawarded offers. Closed collect receipts move
actual sats from reserved to paid. Timeout alone does not release: an expired matching offer
needs confirmed empty/expired claims and empty results, no accepted bind, a successful no-award lookup and no
buyer reservation and parked auto-award intent, or the buyer's existing ledger must have reconciled it to released with
no pending delivery/accept. Free terminal jobs, abandoned never-posted intents and refused
publications release too. Uncertainty retains reservations across restarts and keeps polling
possibly payable work. At zero headroom, free work remains possible; budget stops only on
an idle paid-only rejected batch. Wait/empty assignments alone never imply exhaustion.
Public ACCEPT is not payment proof. Use a dedicated buyer; unknown public amounts fail closed.
A paid collect refused by the buyer wallet's reconciliation (`wallet reconciliation refused: …`)
is a distinct `payment stuck` state, not a retry loop: logged and journaled once, shown in progress,
seller freed, sats kept reserved, and re-checked once after `maxplayer wallet reconcile --home <home>`
changes the wallet, after a later paid collect succeeds, or on restart.

See [runner configuration, planner contract, restart behavior and service example](../scripts/challenges/README.md#automatic-swarm-runner).

## Direct relay reader

`web/app/src/challenge/shared/{events,model,relay}.mjs` is used by the browser,
the buyer runner and local E2E. Install its dependencies with `npm ci --prefix web/app`.
The community HTTP/SSE server and review page have been removed; use
`maxplayer-challenges members` and `maxplayer-challenges decide` for membership review.

- Challenge kinds 30341/30342/30343 are read from every configured public challenge relay.
  Jobs 3401–3406 and member profiles/heartbeats stay on the trade relay. Signed events are
  unioned and deduplicated by ID before latest-per-d projection. Each relay has independent
  paging/reconnect state; unavailable relay URLs are reported without discarding healthy sources.
- History starts anonymously, paged separately by kind with overlapping timestamp boundaries.
- Live subscriptions start anonymously on relays that send no AUTH challenge. Once a relay
  challenges, live subscriptions wait for the AUTH outcome and are sent after AUTH OK; one that
  already left anonymously is replaced under a new id (`live-<kind>~<n>`) and the old one is
  closed, so an EOSE for a replaced subscription never counts. AUTH uses one new in-memory
  per-tab key across connections, never the buyer key. No EVENT is sent by the reader.
- Refused AUTH, or no AUTH challenge at all, leaves history available and polling active; so
  does a relay CLOSED of a live subscription on such an anonymous connection. Refused AUTH adds an
  `AUTH refused (…)` warning; a challenge that is never answered within 15 seconds is treated like
  a refusal, with an `AUTH not answered (…)` warning. A relay CLOSED of a
  display-only feed (applicants, profiles/heartbeats) never affects readiness; it adds a warning
  with the relay's reason. Polling runs every 15 seconds from each filter's newest timestamp minus
  900 seconds, also repairing late events while streaming. Disconnects reconnect with bounded
  backoff and reload history.
- A poll round re-reads history the reader already holds, so once a trusted filter's history has
  been read completely on a connection, its poll pages (and their pagination) keep
  `historyComplete` true; the round is reported as `pollInProgress`, which is not a readiness field
  (R3-4). Poll pages keep every fail-closed rule: a silent page closes the connection, a truncated
  page sets an error, a refused page recovers and must be read again in full, and any filter not
  yet read on the connection (initial read, a new roster chunk, a re-read after AUTH) blocks
  readiness until it completes. The quorum and trade-relay rules are unchanged.
- When a relay sent an AUTH challenge that was not refused, and while it is recovering from a
  CLOSED, `historyComplete` also requires AUTH OK and an EOSE for every trusted filter's live
  subscription sent after AUTH OK on this connection, whether or not that subscription is still
  installed. A newly sent replacement subscription makes the reader not ready until its own EOSE.
- Once a relay has accepted the reader's AUTH (and has not since refused it, or stopped
  challenging, outside recovery), a recovery round on it is ready only with AUTH OK: AUTH refused
  on the reconnect, even with every live subscription installed and answered, or no AUTH challenge
  within 15 seconds of the reconnect, reconnects again instead of completing on anonymous EOSEs.
  Relays that never challenge, or refuse AUTH outside recovery, still recover anonymously.
- A relay CLOSED on a trusted read is never a dead end. When the relay closes an authenticated
  live subscription, closes any trusted live subscription while AUTH is expected or the reader is
  recovering (even before AUTH OK), refuses a trusted history page, or answers `auth-required:`
  when AUTH is then refused or does not complete within 15 seconds, the reader sets `error` to the
  relay's reason and closes the socket; the page shows that reason at once. While recovering, AUTH
  that does not complete within 15 seconds also reconnects. The backoff reconnect then runs a full
  fresh round (AUTH, trusted history, live). Until a recovered connection has stayed ready for 60
  seconds without interruption (the period restarts whenever a trusted live subscription becomes
  unconfirmed), CLOSED-driven reconnects keep doubling the backoff (cap 5 minutes) and a
  `Recovering after relay CLOSED (…)` warning names the last reason. A relay that closes every
  live subscription therefore stays not ready without a reconnect storm. An `auth-required:`
  refusal of a REQ sent before AUTH is re-read in place after AUTH OK.
- Signatures and IDs are verified locally. Buyer-authored kinds are author-scoped;
  claims and results are `#p`-scoped to the buyer. Memberships are read in two separate ways
  (see *Trusted roster and applicant list* below): the trusted roster read is scoped by
  `authors` to buyer-accepted sellers, and a capped applicant read feeds only the
  "waiting for approval" list. Profiles/heartbeats are requested only for accepted sellers.
  A saturated pagination second is reported as incomplete history, never silently certified complete.
- Build the page with `CHALLENGE_BUYER_PUBKEY` (lowercase hex; default
  `94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5`).
  `CHALLENGE_RELAYS` is a comma list, default `wss://nos.lol,wss://relay.primal.net,wss://offchain.pub`.
  `RELAY_URL` feeds the existing app config (default `wss://relay.maxplayer.ai`). Custom
  hosted relay origins must also be allowed in `web/app/vercel.json` CSP.
- The buyer runner takes `--relay-url` / `RELAY_URL` and derives the buyer public key
  from its configured CLI home. Its admission gate still requires authenticated,
  complete trade-relay history and complete history from at least the quorum of challenge relays.
  `SWARM_CHALLENGE_RELAYS` selects the same default public relay list; public relays need no AUTH.
  Partial coverage is exposed as `relay.failedRelays`, `relay.missingRelays`, `relay.quorum`
  (`required`, `complete`, `total`), `relay.degraded` and per-source `relay.relays` status.

## CLI (`maxplayer-challenges …`)

Seller: `join --buyer <npub|hex> --minutes <n> [--note <text>]`,
`status --buyer … [--challenge <d>]`, `leave --buyer …`.
The buyer: `publish --id <d> --title … --goal-file <path> --starts <rfc3339>
--ends <rfc3339> --budget-sats <n> [--status …] [--update <text>] [--result <text>]`,
`members [--challenge <d>]`, `decide --seller <npub|hex> --accept|--deny`.
`--challenge` defaults to the buyer's latest non-cancelled challenge. All sign with the home's
key; none prints a secret.

**Minutes changes during a running challenge.** `join` on an existing `joined` membership
with different `--minutes` (or a different `--note`) republishes the membership. The sidecar
refuses it, publishing nothing and exiting non-zero, when it would forfeit an eligible day:
some buyer-signed challenge's latest version is `scheduled`/`in_progress`, the seller's
latest buyer decision is `accepted`, the current membership's `created_at` and `since` are at
or before `starts`, the replacement would be dated after `starts` (its `created_at` is
`max(now, previous + 1)`), and `now <= ends + 900`. The buyer posts through `ends` inclusive,
and the 900-second `created_at` tolerance lets a fast seller clock publish before the
buyer's `ends`, so the refusal lasts until 15 minutes after `ends`:
"You're in today's challenge (<id>, ends <time>). Change your minutes (or note) after
<ends + 15 min> …; they apply from the next day. Re-publishing now would make you ineligible
for today. Do not leave and rejoin to change minutes: that also forfeits today."
The same minutes with `--note` omitted or equal is a no-op (nothing published,
`"unchanged": true`). On a real change, omitting `--note` keeps the current note and
`--note ''` clears it. Pending or denied sellers, and memberships already replaced after the
start, are not counted that day, so their changes publish. The challenge read is the
trusted, paged, every-relay read (`Relays::fetch_all`), not the quorum read; if it is
incomplete (refused, timed out, truncated) on any relay the change fails closed (nothing
published): the relay left out could be the only one holding the running challenge. First joins, rejoins after `left`, `leave` and `status` are unaffected.
This is client policy only: the event format and the eligibility rule are unchanged, and
another client can still republish (and forfeit the day).

## Community reader implementation notes

- The browser holds a locally generated **ephemeral per-tab NIP-42 authentication key**, never the buyer
  secret. It sends AUTH only; no membership, decision, job or other EVENT is published.
- Protocol v1 AWARD (3405) has no amount tag. Public committed/paid figures therefore use the
  selected offer's fixed price. A matching buyer ACCEPT (3406, offer root + unmarked claim
  reference) moves that amount from committed to paid **estimate**; it proves authorisation,
  not Cashu settlement. Fees, refunds and private payment state are not visible.
- Tally timestamps are seconds; snapshot minutes are fractional seconds / 60 (no rounding before
  summation). Remaining is offered minus used, including negative overruns. Leave/deny
  prevents eligibility but never erases awarded work. The shared model reports eligibility separately
  from display state. Left/denied take display precedence over working/done.
- Current means the newest non-cancelled challenge (newest cancelled if none); ties use lowest
  event id. Duplicate awards for an offer select the earliest valid award, then lowest id.
- Review past jobs include completed jobs across known challenges, deduplicated by offer id. Heartbeat capabilities are
  advertised, not independently verified. The reader is a projection; it never posts jobs.
  Rust parity must be checked with the same event fixtures by its implementation.

Paid settlement is not covered by the local challenge E2E (no test mint/funded wallet).
Priced posting/accounting are unit-tested; the full E2E exercises free deliveries/receipts.
See the runner README for the loopback test-mint/funding infrastructure needed to extend it.

### Advisor r1: membership and minute safety

Eligibility requires the latest membership event's `created_at` **and** `since`
to be no later than the challenge start. A late replacement (including a minutes
change or rejoin) waits for the next challenge; an old acceptance cannot override
that boundary. Sidecar, page and runner use the same rule. The sidecar refuses a
minutes/note change while a challenge that still counts the membership is running,
rather than forfeit that day (run 2, issue #14).

A RESULT timestamp at or before its AWARD is untrusted for duration: charge the
full deadline-minus-award interval, not zero. Other results continue charging until buyer ACCEPT (or now), capped at deadline. The coordinator and swarm stop assigning when
no task fits the remaining offered minutes.

Challenge offers carry the exact buyer-signed task prefix
`[challenge:<buyer hex>:<challenge id>]` followed by a newline. Only offers with
that prefix enter that challenge's tally, minutes and public budget estimate.
Ordinary jobs from the same buyer in an overlapping window do not count. This
uses the existing signed `i` task field, not a new trade/MCP parameter. Pre-prefix
offers are intentionally excluded; existing runner journal reservations remain
conservative. Challenge IDs must not be reused for a different challenge.

All sidecar publishes (not only leave, deny, cancel and budget updates) require
ACK from the challenge relay quorum. Below quorum, partial delivery can already have
written some relays: the command fails and identifies failed URLs; retry repairs
coverage. Reads require EOSE from the quorum. The page keeps healthy relay data
visible with an incomplete/failed-URL indicator; the runner never posts from history
below quorum. AUTH-required CLOSED history requests are retried after AUTH OK.
Removing a relay from the list is safe only after successfully reading that relay, or
after a person has checked it holds no newer replaceable event (challenge, membership,
or decision). Otherwise removing it can resurrect an older joined membership, budget,
or challenge state. With the quorum a dead relay no longer needs removing.

### Challenge relay quorum (run 2 #13)

Run 2 showed one dead public relay (offchain.pub) blocking joins, leaves, decisions,
publishes and the whole runner, because every read and every publish needed every relay.

**Setting.** One number, the quorum `q`, for reads and publishes: `--quorum N` on the
sidecar, `SWARM_CHALLENGE_QUORUM` for the runner, `CHALLENGE_QUORUM` at page build. Default
2 for the three default relays; for other list sizes the default is a strict majority
(`floor(n/2)+1`, at least 2, at most `n`). An explicit value must be a strict majority and at
most `n`; anything else is refused at startup. `q = n` restores the old every-relay rule.

**Why a majority.** A publish reached at least `q` relays and a read completed on at least
`q`; with `q > n/2` the two sets always share at least one relay, so a quorum read sees every
quorum-published leave, deny or close as long as that shared relay does not hide or lose it.

**Our trade relay is required.** It is not part of the quorum and is never optional: reader
readiness (`historyComplete`), the runner's `relay_not_ready` gate and every runner challenge
publish (checked on a fresh snapshot) need it, quorum or not.

**Degraded.** Quorum met with a relay missing is ready but degraded, and says so: the
sidecar prints `Warning: degraded read|publish: …` on stderr and adds `relay_quorum` to
`--json`; the reader adds `Degraded: <n> of <m> challenge relays complete (quorum q);
missing: …` to `relay.warnings`, which the page's status line shows; the runner logs
`WARNING: relay coverage degraded (…)` and copies the sidecar's warnings.

**Re-send.** The sidecar keeps every quorum publish that missed a relay, exactly as signed,
in `<home>/challenges/outbox.json` and re-sends it (never re-signed: same id and signature)
to that relay once it answers a later command. Bounded (64 entries, 10 failed attempts per
relay, 7 days), idempotent (a delivered relay is removed; re-sending to a relay that already
has it is harmless), and superseded by a newer event for the same coordinate. Only events
signed by the home's own key, with valid signatures, are loaded. A publish below quorum
fails and is never queued: the caller was told it failed.

**Fail-closed cases kept.** Below quorum, or without our trade relay: reads fail ("challenge
relay read incomplete; quorum not met …"), publishes fail, the reader is not ready and the
runner posts and publishes nothing. The minutes-change refusal keeps its every-relay
challenge read. The closed-challenge refusal (exit 3) still runs on the pre-publish read; a
`scheduled`/`in_progress` update over an existing challenge while degraded re-reads every
relay first. Runner progress updates (which re-send the latest buyer-signed status) need
every relay and wait while degraded; completion publishes at the quorum.

**A completed read that omits an accepted event.** The quorum assumes the relay a read shares
with a publish returns what it accepted. If that relay lost or hides the event, a read that
completed on the quorum omits it, and nothing in the read shows that. With 2 of 3 an
attacker (or a lossy relay) needs only that shared relay; with every relay required it needed
all of them to agree. Consequences and mitigations:
- A seller's `leave` or a buyer's deny/close could be missed by one quorum read until the
  missing relay is back (re-send) or the shared relay serves it again; the runner could
  assign one more task to a seller who left. The leave still takes effect on the next read
  that includes an honest holder, and a left seller never claims (seller-side check).
- Reads union every relay that answers: with all three up, one honest relay still suffices,
  exactly as before. The weaker guarantee applies only while degraded, which is visible.
- The home that published the event keeps it in its outbox until every relay has it and
  counts it in its own trusted reads, so the sidecar never republishes over its own pending
  leave/close (e.g. an `in_progress` update over its own pending completion is refused).
- Reads whose mistake cannot be undone stay every-relay: the minutes-change challenge read
  (and, while degraded, its decision read: a quorum read that omits the buyer's acceptance
  must not skip the challenge check), a degraded non-terminal challenge update, and runner
  progress re-sends.
- Routine polls never count a relay as missing: a relay that finished its round keeps
  counting toward the quorum while a poll is in flight (R3-4); it drops out only on a
  disconnect, a recover, or a failed (refused, truncated, timed-out) round.
- Not covered: another client reading at a bare quorum, or a relay that acknowledged and then
  dropped the event before re-send. Operators who need the old guarantee set `q = n`.

### Trusted roster and applicant list (advisor r4)

The runner's safety never depends on data strangers can publish. The **trusted roster** is
defined only by the buyer's signed decisions (kind 30343, latest per seller, `accepted`).
Readers fetch decisions first, then memberships with `authors` = those accepted pubkeys
(chunked at 100 authors per filter), and re-subscribe whenever decisions change. Eligibility,
minutes, leave and the runner's exhaustion stop are computed only from decisions plus these
accepted-author memberships. `historyComplete` (and therefore runner readiness) covers exactly
that set; a roster relay is not complete until its decision history defined the roster.

Rust `eligible_sellers` and the sidecar's `status`/`members` give the same eligible set: latest
buyer decision `accepted` and the latest valid membership *from that pubkey* `joined`, with
`since` and `created_at` at or before the challenge start. Memberships from other authors are
dropped before replacement. The sidecar reads decisions first and then memberships with an
`authors` filter (accepted sellers; `join`/`leave`/`status` add the caller, `decide` adds the
`--seller` being decided). Its trusted reads page like the web reader (`limit` 500, inclusive
`until` re-read of the oldest second, fail closed when the cursor does not move), so a
truncated history makes the command fail and publish nothing rather than act on a subset.
Like the web reader, the sidecar counts every `EVENT` frame the relay sent toward the page
limit, including frames its nostr-sdk client drops before delivery (expired, bad signature,
oversized), and it stops after 20 pages per relay with `history truncated`.
`web/app/test/fixtures/challenge-roster.json` is checked by both stacks.

The **applicant list** shows people who asked to join and have no decision yet, so the buyer can
approve them. It comes from one separate, untrusted read per round: newest-first, `limit` 256,
latest per pubkey, bounded note. It is never paged, never stored with trusted events and never
affects `historyComplete`, `error` or the runner. A refused, silent or saturated applicant read
sets only `applicantsIncomplete`; the page then marks the list as possibly incomplete. The
`members` command reports the same flag as `applicants_incomplete`.

What a membership flood can and cannot do:

- It **can** clutter or truncate the waiting-for-approval list (newest 256 shown), so an older
  applicant may be missing until the buyer approves newer ones or the flood ages out.
- It **cannot** hide, delay, replace or block an accepted seller's join or leave, block the
  exhaustion stop, or keep the runner from becoming ready: strangers' events never match the
  roster filter. Only an accepted seller can affect its own roster entries.

Paging the trusted roster: the reader filters locally on `kinds`/`authors`/`#p`/`since`/`until`.
A full 500-event page re-reads its oldest second with an inclusive `until` (no second that could
still hold unread events is skipped). The cursor must move strictly older; a page that repeats
the same oldest second (500+ events from accepted sellers in one second, or a relay repeating a
page), or a full page with no in-window event, fails closed as `History truncated`. Saturation
counts every frame the relay sent, valid or not.

Store hygiene: the store keeps only the latest valid membership per `(seller, buyer)`; an older
or invalid version never replaces it. A stored membership whose note the projection would drop
(content over 4096 UTF-16 units or tags over 16384 serialized units) is kept with content `''`;
tags, status, minutes, since and `created_at` are unchanged. Memory is about one membership per
accepted seller plus at most 256 applicants. Join refuses oversized notes; leave drops an
oversized prior note.

Profiles/heartbeats are requested for accepted sellers only, at most 256 in pubkey order. They
are display/planner metadata: a missing profile or heartbeat never makes an accepted seller
ineligible (only a heartbeat that states `status=not_serving` or `serving=0/N` pauses assignments to it), and the cap warning never blocks readiness or exhaustion. The former
`excludedMembers` field is removed.

The production-relay fence is a normalized **hostname string** denylist, shared by
the sidecar, page, swarm parser and site build (canonical JSON literal in
`shared/relay.mjs`). It is not DNS/IP resolution: aliases and literal IP addresses
are not proven to be different servers. Do not configure production aliases.
