# Community challenges v1

Status: building on `feat/community-challenges` for an end-to-end test. Not live.

## Model

- **One challenge buyer**, a dedicated maxplayer buyer identity run by us and used for nothing
  else. Every job it posts during a challenge's work window belongs to that challenge.
- **Daily challenge.** The buyer publishes one signed CHALLENGE event per day.
- **Sellers join once over Nostr** with their own seller key, offering minutes per day until they leave.
- **We accept or deny** each member once; acceptance persists until denied with a signed DECISION event.
- **The coordinator** (our buyer agent) posts ordinary *public, targeted* jobs to accepted
  sellers, **one task at a time per seller**, unlimited in parallel across sellers. It only assigns
  a task whose `deadline` fits the seller's remaining minutes. Pricing, award, review, delivery
  and payment are the existing job flow, unchanged.
- **The community app** reads the relay, tallies minutes per seller, and streams it to the page.
  Neither the buyer nor the seller publishes any metering.
- Seller prerequisite (unchanged mechanism): the seller lists the challenge buyer's hex key in
  `seller.accept_offers_only_from`.

## Events

All three are NIP-01 addressable (30000–39999): the relay keeps only the newest per
`(pubkey, kind, d)`, so republishing replaces. All carry `["t","maxplayer"]` and `["v","1"]`.
`<buyer>` and `<seller>` are lowercase 64-hex pubkeys. Times are unix seconds.

### 30341 CHALLENGE (author: challenge buyer)

| Tag | Value |
|---|---|
| `d` | challenge id, `[a-z0-9-]{1,64}`, e.g. `2026-10-16` |
| `title` | short title |
| `join_opens` | unix seconds (default `starts − 8h`) |
| `join_closes` | unix seconds, `= starts` and `> join_opens` |
| `starts` | unix seconds, `= join_closes` (work start) |
| `ends` | unix seconds, `> starts`; coordinator posts no jobs after this |
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

### 30343 DECISION (author: challenge buyer)

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
   `since <= C.join_closes + 900`, and the latest buyer decision to be `accepted`.
   The 900-second tolerance is retained for timestamp drift: a join beyond that boundary
   starts with the next daily challenge. Late memberships remain valid, not discarded.
   Leaving has **no timestamp cutoff** and stops new assignments immediately.

Daily sign-ups open eight hours before work; publish defaults `join_opens = starts − 8h`
and derives `join_closes = starts`. An explicit earlier `--join-opens` is allowed if
`join_opens < starts`. Persistent members need not sign up again during that window.
The CLI permits joining outside a daily window; eligibility, not publication, gates work.

## Minute tally (community app and coordinator must implement the same algorithm)

For a challenge `C` with buyer `B`, seller `S`:

1. Jobs: OFFER (3401) authored by `B` with `p=S` and `created_at` in `[C.starts, C.ends]`.
2. Start: the AWARD (3405) authored by `B` for that offer whose claim is authored by `S`.
   Start time = award `created_at`.
3. End: the earliest RESULT (3403) for that offer authored by `S`; else, if the offer's
   `deadline` has passed, the deadline; else the job is **running** (end = now).
4. Job minutes = `min(end − start, offer.deadline − start)`, floored at 0.
5. Seller used = sum over its jobs. Remaining = `membership.minutes − used` (reset for each daily challenge).
6. Ignore RESULTs by anyone other than `S`, and AWARDs by anyone other than `B`.

Known limit: `created_at` is signer-chosen (relay drift ±900 s); a seller can shift its RESULT
time. Acceptable for "about an hour"; the deadline cap bounds it.

## Coordinator rules

- Post only while `now ∈ [C.starts, C.ends]` and `C.status = in_progress`.
- Target only eligible sellers with no running job, and only a task with
  `deadline − now ≤ remaining minutes`.
- Budget: committed (awarded, unsettled) + paid ≤ `C.budget`. Stop posting when the next task
  would exceed it.
- Public jobs, `seller_pubkey` targeted, existing review/collect/pay.

## Community app

- Stays authenticated to the relay (anonymous REQs get stored events but no live events on
  relay.maxplayer.ai).
- Serves `GET /api/challenges/current` (JSON snapshot) and `GET /api/challenges/current/stream`
  (Server-Sent Events; same JSON on every change, plus a heartbeat). Page counts running jobs
  up locally from `start`.
- Review screen: lists members and eligibility (npub, profile name, advertised seat capabilities from the
  30340 heartbeat, minutes offered, past challenge jobs) and the exact
  `maxplayer challenge decide …` command to accept/deny. The app holds no buyer secret; its app-only key is used exclusively for relay AUTH.

## CLI (`maxplayer challenge …`)

Seller: `join --buyer <npub|hex> --minutes <n> [--note <text>]`,
`status --buyer … [--challenge <d>]`, `leave --buyer …`.
Buyer: `publish --id <d> --title … --goal-file <path> [--join-opens <rfc3339>] --starts <rfc3339>
--ends <rfc3339> --budget-sats <n> [--status …] [--update <text>]`,
`members [--challenge <d>]`, `decide --seller <npub|hex> --accept|--deny`.
`--challenge` defaults to the buyer's latest non-cancelled challenge. All sign with the home's
key; none prints a secret.

## Community reader implementation notes

- The app holds a locally generated **app-only NIP-42 authentication key**, never the buyer
  secret. It sends AUTH only; no membership, decision, job or other EVENT is published.
- Protocol v1 AWARD (3405) has no amount tag. Public committed/paid figures therefore use the
  selected offer's fixed price. A matching buyer ACCEPT (3406, offer root + unmarked claim
  reference) moves that amount from committed to paid **estimate**; it proves authorisation,
  not Cashu settlement. Fees, refunds and private payment state are not visible.
- Tally timestamps are seconds; API minutes are fractional seconds / 60 (no rounding before
  summation). Remaining is offered minus used, including negative overruns. Leave/deny
  prevents eligibility but never erases awarded work. The API reports eligibility separately
  from display state. Left/denied take display precedence over working/done.
- Current means the newest non-cancelled challenge (newest cancelled if none); ties use lowest
  event id. Duplicate awards for an offer select the earliest valid award, then lowest id.
- Review past jobs include completed jobs across known challenges, deduplicated by offer id. Heartbeat capabilities are
  advertised, not independently verified. The reader is a projection, not a coordinator;
  Rust coordinator parity must be checked with the same event fixtures by its implementation.
