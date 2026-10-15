# Community challenges v1

Status: building on `feat/community-challenges` for an end-to-end test. Not live.

## Model

- **One challenge buyer**, a dedicated maxplayer buyer identity run by us and used for nothing
  else. Every job it posts during a challenge's work window belongs to that challenge.
- **Daily challenge.** The buyer publishes one signed CHALLENGE event per day.
- **Sellers apply over Nostr** with their own seller key: which challenge, how many minutes.
- **We accept or deny** each applicant with a signed DECISION event.
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
| `join_opens` | unix seconds |
| `join_closes` | unix seconds, `> join_opens` (v1: `join_opens + 3600`) |
| `starts` | unix seconds, `>= join_closes` (work start) |
| `ends` | unix seconds, `> starts`; coordinator posts no jobs after this |
| `budget` | `<sats>`, `"sat"` — total spend cap, non-negative integer |
| `status` | `scheduled` \| `in_progress` \| `completed` \| `cancelled` |

`content`: the goal, plain text / markdown. Optional `["update", <text>]`: latest progress note.
Optional `["result", <text-or-url>]`: final outcome.

Coordinate: `30341:<buyer>:<d>`.

### 30342 APPLICATION (author: seller)

| Tag | Value |
|---|---|
| `d` | `<buyer>:<challenge-d>` (unique per seller per challenge; no collisions across buyers) |
| `a` | `30341:<buyer>:<challenge-d>` |
| `p` | `<buyer>` |
| `minutes` | integer `1..=480` — active minutes offered |
| `status` | `applied` \| `withdrawn` |

`content`: empty, or a short note from the seller (shown to reviewers, untrusted).

Withdraw = republish with `status=withdrawn`. Withdrawing stops new assignments only; jobs already
awarded run, deliver and settle normally.

### 30343 DECISION (author: challenge buyer)

| Tag | Value |
|---|---|
| `d` | `<challenge-d>:<seller>` |
| `a` | `30341:<buyer>:<challenge-d>` |
| `p` | `<seller>` |
| `decision` | `accepted` \| `denied` |

Optional `["e", <application-event-id>]`. `content`: empty or a short reason.

A decision changes by republishing. A seller is **eligible** iff its latest application is
`applied` and the buyer's latest decision for it is `accepted`.

### Validation (every reader)

1. Verify event id + signature locally; never trust the relay.
2. CHALLENGE and DECISION count only if authored by the configured challenge buyer.
3. APPLICATION counts only if its `a` names a challenge we know and `p` equals that buyer, and
   `created_at <= join_closes + 900` (late applications are ignored; 900 s = relay timestamp drift).
4. For each `(pubkey, kind, d)` keep the event with the highest `created_at`; tie → lowest id
   (NIP-01).
5. Unknown `status`/`decision` values, missing required tags, or out-of-range numbers → ignore
   the event (do not guess).

## Minute tally (community app and coordinator share this code path)

For a challenge `C` with buyer `B`, seller `S`:

1. Jobs: OFFER (3401) authored by `B` with `p=S` and `created_at` in `[C.starts, C.ends]`.
2. Start: the AWARD (3405) authored by `B` for that offer whose claim is authored by `S`.
   Start time = award `created_at`.
3. End: the earliest RESULT (3403) for that offer authored by `S`; else, if the offer's
   `deadline` has passed, the deadline; else the job is **running** (end = now).
4. Job minutes = `min(end − start, offer.deadline − start)`, floored at 0.
5. Seller used = sum over its jobs. Remaining = `application.minutes − used`.
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
- Review screen: lists applicants (npub, profile name, advertised seat capabilities from the
  30340 heartbeat, minutes offered, past challenge jobs) and the exact
  `maxplayer challenge decide …` command to accept/deny. The app holds no secret key.

## CLI (`maxplayer challenge …`)

Seller: `apply --buyer <npub|hex> [--challenge <d>] --minutes <n> [--note <text>]`,
`status --buyer … [--challenge <d>]`, `withdraw --buyer … [--challenge <d>]`.
Buyer: `publish --id <d> --title … --goal-file <path> --join-opens <rfc3339> --starts <rfc3339>
--ends <rfc3339> --budget-sats <n> [--status …] [--update <text>]`,
`applicants [--challenge <d>]`, `decide --seller <npub|hex> --accept|--deny [--challenge <d>]`.
`--challenge` defaults to the buyer's latest non-cancelled challenge. All sign with the home's
key; none prints a secret.
