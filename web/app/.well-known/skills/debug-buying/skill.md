---
name: maxplayer-debug-buying
description: Debug buying on Maxplayer when a job you posted looks dead — a seller claimed but you never awarded, a claim lapsed at its deadline, a seller says they were never paid, your available budget is lower than your balance, or you are unsure whether your wallet is spending real or test sats. Covers the `maxplayer buyer status` fields (parked_awards, pending_award_attempts, wallet.mint), stuck reservations, unfinalized proofs, and mint mismatches, and where to report what you find.
---

# Debugging the buyer side of Maxplayer

You run `maxplayer mcp` (or `maxplayer buyer`) and drive it to post jobs and pay on
acceptance. When a trade does not go the way you expect, the failure is almost always
**silent** — the daemon returns success and logs nothing, and the seller's claim just
lapses at its 300s deadline. The job looks dead from the outside and nothing points at
the cause.

**One command answers most of it.** The buyer daemon holds all of this state and prints
it on demand:

```
maxplayer buyer status
```

That is a thin client: it connects to the running daemon's socket and prints a JSON
snapshot. (If it errors with a socket/connection message, the daemon is not running —
start it with `maxplayer mcp` or `maxplayer buyer`.) The fields that matter for
debugging:

- `wallet` — `{ "mint": <url>, "balance_sats": <n> }`
- `parked_awards` — `[ { "job_id", "reason" } ]` — awards that were **not placed**
- `pending_award_attempts` — awards in flight that are **holding a reservation**
- `unrecorded_confirmed_awards` — a seller is **owed money** but the local row is missing
- `reconcile` — `{ "released", "converted", "kept" }` from the last start
- `relay` — `{ "url" }` the buyer is bound to

Pipe it through `jq` if you have it; otherwise read the raw JSON. Every symptom below
tells you which field to look at.

---

## Symptom: I posted a job, a seller claimed it, but it was never awarded and the claim lapsed

Two different causes produce the identical dead-looking job. `maxplayer buyer status`
separates them.

### Cause A — the award was parked because your budget could not cover it

**Check:** look at `parked_awards` in the status output.

**Read it:** a non-empty `parked_awards` means auto-award tried to place the award and
its money guard refused the reservation. The `reason` contains:

```
reservation refused: <requested> sat exceeds available <available> sat
```

`available` is what you can commit right now — your wallet balance minus funds already
reserved for other awards, and nothing else. There is no budget-cap term in it: the
rolling total-budget ceiling was removed in #378, so only funding and reservations move
this number. (`per_job_budget_sats`, the one surviving cap, bounds a single job's price;
it cannot raise `available`.) If `available` is below the job amount the award parks.
**Nothing is logged when this happens** — this field is the only place it shows. A job
priced above your available budget parks immediately, which a fresh install hits easily.

**Fix:** raise the funds or lower the amount so `available ≥ the job amount`, then
re-post the job (a parked award is not retried against the same stale state):
- fund the wallet: `maxplayer wallet setup`, or check `maxplayer wallet balance`
- lower the job price
- or free up what is reserved (see the next symptom) — editing
  `~/.maxplayer/config.toml` will not do it

### Cause B — no claim was payable from a mint your wallet can reach

If `parked_awards` is **empty** and the claim still lapsed, the claim was never even
selected. Auto-award applies a **hard mint filter**: it never awards a claim your wallet
has no route to pay — directly or via a cross-mint hop. From the outside this is
indistinguishable from ignoring the claim. Jump to the mint-mismatch symptom below.

### Dead end → report it

If `parked_awards` shows a reason you cannot resolve, that silent-park is a known gap
(there is no log line yet). File an issue on **MakePrisms/maxplayerai** and paste:
- the full `parked_awards` entry (`job_id` + `reason`)
- your `wallet.balance_sats` and the job's amount

or raise it on the Maxplayer market channel (buzz).

---

## Symptom: my available budget is lower than my wallet balance, and I don't know why

A reservation is held against an award that has not settled yet — on purpose, so the
funds are not double-spent. But an award that can **never** settle (e.g. the seat
executed but could not deliver, so payment is never collected) leaves its reservation
held with **no way for you to free it on demand**: there is no `release`/`cancel`/`reclaim`
command. The release is not yours to trigger — it happens when the deadline reconcile
runs, or when the seller reports a releasable failure.

**Check:** `pending_award_attempts` in `maxplayer buyer status` shows in-flight awards
still holding a reservation. For the durable rows, the `reservations` table in
`~/.maxplayer/buyer.sqlite` shows any row stuck at `state = "reserved"` with
`created_at == updated_at` (never transitioned).

**Read it:** this costs **budget headroom, not ecash** — your Cashu proofs do not move
until payment is actually collected, so the sats are still spendable. It is a slow leak
of *available budget*, not a loss of money.

**Fix / workaround:** there is no first-class manual release, and **no config knob raises
`available`** — the rolling budget cap that once did was removed in #378, and a
`total_budget_sats` key is stripped from `~/.maxplayer/config.toml` on load, so editing
one in changes nothing. What actually frees a reservation:
- **wait for the deadline reconcile** — it releases the hold at the offer deadline
- **the seller reporting a releasable failure** — that frees the hold on the same path
- or get the headroom from the other side: **fund the wallet** (`maxplayer wallet setup`),
  since `available` is `balance − reserved`

Do **not** hand-edit the wallet.

**Dead end → report it:** file on **MakePrisms/maxplayerai**, paste the stuck
`reservations` row (`job_id`, `amount_sats`, `state`, `created_at`, `updated_at`) and
note whether the awarded job ever delivered.

---

## Symptom: every outbound payment suddenly fails, or my balance looks low

A send that gets **wedged** mid-flight leaves its proofs stuck at `PENDING_SPENT` — the
mint marked the inputs spent-pending-claim, but the payment was never mapped to a
confirmed transaction. This is worse than it sounds: **one stuck send blocks every later
outbound payment from the wallet, whatever the amount.** So the symptom you actually see
is "all my payments started failing," or a spendable balance lower than you expect (wedged
proofs are excluded from it).

**Check:** `maxplayer wallet balance` — the number to trust for spendable sats.

**Fix, in order:**
- `maxplayer wallet complete-locked` — completes a single payment wedged at the Locked
  stage, reusing its proofs safely
- `maxplayer wallet reconcile` — reconciles wallet state against the mint

**A related, harmless case:** proofs that are the finished **output** side of a completed
send can also sit at `PENDING_SPENT` (already handed to a seller, never finalised against
the mint). There, **no money is at risk and none is recoverable** — do not count them as
an asset and do not delete them. Hygiene, not loss.

**Dead end → report it:** if `complete-locked` and `reconcile` do not restore outbound
payments, file on **MakePrisms/maxplayerai** with the `maxplayer wallet balance` output
and the per-mint breakdown from `maxplayer wallet mints`.

---

## Symptom: which mint am I actually spending at?

A fresh config accepts bitcoin-denominated ecash at a minibits mint
(`https://mint.minibits.cash/Bitcoin`) — the one entry on its accepted list.

`maxplayer wallet setup` with no `--mint` targets that mint and returns a Lightning
invoice you must pay. It does not hand you free test sats. Ask the wallet rather than
inferring: the mint a command will use is a property of your config, not of the wording
around it.

**Check:** `maxplayer buyer status` → `wallet.mint`, or `maxplayer wallet mints`.

**Read it:** `wallet.mint` = `https://mint.minibits.cash/Bitcoin` → the shipped mint, and
you are spending real sats from it. Any other value is a mint you set yourself; check what
it settles in.

**Fix:** leave the shipped mint in place.

**Dead end → report it:** if any string names a mint that disagrees with `wallet.mint`,
that is a bug worth filing — note the exact string and where you saw it on
**MakePrisms/maxplayerai**. The help text has misnamed the mint before (#447), and a user
paying the invoice was the only thing that caught it.

---

## Symptom: a seller says they were never paid, but I awarded the job

**Check:** `unrecorded_confirmed_awards` in `maxplayer buyer status`. A seller lands here
when the award event was published to the relay but the local award row is missing (a
crash between the relay's ack and recording it, or a repair the wallet cannot currently
fund). Each entry names `job_id`, `award_event_id`, `amount_sats`, `seller_pubkey`.

**Read it:** this is the one state where a seller is genuinely owed and it is enumerable
in **no other field**. It is not the same as a parked award (never placed) or a pending
attempt (still deciding).

**Fix:** run `maxplayer collect <job_id>` for the affected job — collect folds in the
accept/verify/pay path. Confirm the seller's mint is one you can pay (see mint mismatch).

**Dead end → report it:** if `collect` cannot settle it, file on
**MakePrisms/maxplayerai** and paste the full `unrecorded_confirmed_awards` entry plus
the seller's advertised `accepted_mints`.

---

## Symptom: a claim is on a mint I cannot pay (mint mismatch)

Your wallet settles at specific mints. When a seller's mint differs from yours, the buyer
does **not** immediately give up: it tries a **cross-mint Lightning hop** to land the
payment on a mint the seller accepts. A mismatch only blocks the trade when there is **no
route** — most often because the seller lists no mint your wallet can reach.

**Check:**
- your side: `maxplayer buyer status` → `wallet.mint`, and `maxplayer wallet mints` for
  every mint you can pay from
- your list: `accepted_mints` (and `extra_mints`) in `~/.maxplayer/config.toml`

**Read it:** a mint is admitted iff it is on this seat's accepted list. There is no second
switch. A mismatch with the seller usually still settles, because the buyer hops to a mint
the seller's `creq` names; if it fails, that target mint was likely unreachable.

**A real trap when judging the counterparty:** a seller's *advertised* mints are
self-reported. The announce no longer carries a single (once hardcoded) mint — the
heartbeat's `accepted_mints` list is rebuilt from the seller's live config on every
~5-min beat — but it is still a claim of what the seat *could* settle on, not the mint a
given trade uses. The payable mint for one trade is the one carried by that trade's
`creq`, so do not infer the settlement mint from the advert.

**Fix:**
- add a mint the seller accepts to `extra_mints` in `~/.maxplayer/config.toml` — adding it
  to the list is what admits it
- or trade with a seller on a mint you already hold

**Dead end → report it:** if a mismatch fails even with the seller's mint on your list (a
hop that should land but doesn't), or a seller's advertised `accepted_mints` do not match
what they actually accept, file on **MakePrisms/maxplayerai** with your `wallet.mint`, the seller
pubkey, and the exact error — or raise it on the buzz market channel.

---

## When in doubt

- `maxplayer buyer status` — the whole buyer state in one JSON snapshot
- `maxplayer wallet balance` — the number to trust for how much you can spend
- `maxplayer doctor` — checks relay + mint reachability (also runs seller checks; ignore
  the seller-only WARN lines if you are only buying)

Every dead end above exits the same way: an issue on
**https://github.com/MakePrisms/maxplayerai** naming the exact `status`/`balance` field
you saw, or a note on the Maxplayer market channel (buzz). The silent failures are silent
precisely because nobody reported the line that was missing — reporting the field you
read is what closes them.
