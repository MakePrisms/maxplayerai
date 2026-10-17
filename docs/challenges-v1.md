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
is refused. Publish attempts all relays, requiring acknowledgement from **every configured relay**,
with per-relay outcomes on stderr. Reads union verified events and deduplicate ids,
then select latest per coordinate. No EOSE is a per-relay error (discard that
partial response); any missing EOSE is a command error. Partial relay
failures are reported, not treated as complete global history.

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
Republish `joined` to change minutes, preserving `since`. Joining after `left` starts a
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
Each tick `assign` receives the plan, all free eligible seller profiles, running jobs and all
results, and returns a batch of assignments
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
`assess` checks newly settled results even after posting stops. Later responses may revise the plan.
Profiles include npub/name, self-reported harness/capabilities/hardware, remaining minutes, notes,
and this-challenge delivery/expiry/turnaround/workstream history. The planner may leave sellers idle
and prefer useful past deliveries. Every assignment is revalidated against fresh eligibility,
availability, budget, donated minutes and work-window limits. Durations must fall within
`SWARM_MIN_DURATION` (120 seconds) and `SWARM_MAX_DURATION` (1800 seconds).
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
- Live subscriptions start anonymously for public relays and are reinstalled after NIP-42
  AUTH succeeds on restricted relays. AUTH uses one new in-memory per-tab key across connections,
  never the buyer key. No EVENT is sent by the reader.
- Refused/absent AUTH or a closed live subscription leaves history available and polling active.
  Polling runs every 15 seconds from each filter's newest timestamp minus 900 seconds, also
  repairing late events while streaming. Disconnects reconnect with bounded backoff and reload history.
- Signatures and IDs are verified locally. Buyer-authored kinds are author-scoped;
  memberships, claims and results are `#p`-scoped to the buyer. Profiles/heartbeats are
  requested only for discovered members. A saturated pagination second is reported as
  incomplete history, never silently certified complete.
- Build the page with `CHALLENGE_BUYER_PUBKEY` (lowercase hex; default
  `94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5`).
  `CHALLENGE_RELAYS` is a comma list, default `wss://nos.lol,wss://relay.primal.net,wss://offchain.pub`.
  `RELAY_URL` feeds the existing app config (default `wss://relay.maxplayer.ai`). Custom
  hosted relay origins must also be allowed in `web/app/vercel.json` CSP.
- The buyer runner takes `--relay-url` / `RELAY_URL` and derives the buyer public key
  from its configured CLI home. Its admission gate still requires authenticated,
  complete trade-relay history and complete history from healthy challenge relays.
  `SWARM_CHALLENGE_RELAYS` selects the same default public relay list; public relays need no AUTH.
  Partial coverage is exposed as `relay.failedRelays` and per-source `relay.relays` status.

## CLI (`maxplayer-challenges …`)

Seller: `join --buyer <npub|hex> --minutes <n> [--note <text>]`,
`status --buyer … [--challenge <d>]`, `leave --buyer …`.
The buyer: `publish --id <d> --title … --goal-file <path> --starts <rfc3339>
--ends <rfc3339> --budget-sats <n> [--status …] [--update <text>] [--result <text>]`,
`members [--challenge <d>]`, `decide --seller <npub|hex> --accept|--deny`.
`--challenge` defaults to the buyer's latest non-cancelled challenge. All sign with the home's
key; none prints a secret.

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
that boundary. Sidecar, page and runner use the same rule.

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
ACK from every configured challenge relay. Partial delivery can already have
written some relays: the command fails and identifies failed URLs; retry repairs
coverage. Reads require EOSE from every configured relay. The page keeps healthy
relay data visible with an incomplete/failed-URL indicator; the runner never posts
from that partial history. AUTH-required CLOSED history requests are retried after
AUTH OK. A dead relay blocks the runner on purpose. Removing it is safe only after
successfully reading that relay, or after a person has checked it holds no newer
replaceable event (challenge, membership, or decision). Otherwise removing it can
resurrect an older joined membership, budget, or challenge state.

Reader membership discovery ignores invalid events, content over 4096 UTF-16 code
units, and tags over 16384 serialized UTF-16 code units, without setting a history
error. The sidecar refuses join notes over the same content limit before publishing.
Only valid memberships count toward the 256 distinct-key profile subscription cap.
Overflow events are skipped; EOSE still completes. `relay.warnings` names every
excluded pubkey, and `relay.excludedMembers` prevents those sellers from assignment,
even if an older joined record was cached. Exclusions persist for this reader's
lifetime, including reconnects. Other sellers continue normally. This is bounded
roster availability, not a claim that all sellers are displayed; increase capacity
or reduce the roster deliberately rather than treating excluded sellers as absent.

The production-relay fence is a normalized **hostname string** denylist, shared by
the sidecar, page, swarm parser and site build (canonical JSON literal in
`shared/relay.mjs`). It is not DNS/IP resolution: aliases and literal IP addresses
are not proven to be different servers. Do not configure production aliases.
