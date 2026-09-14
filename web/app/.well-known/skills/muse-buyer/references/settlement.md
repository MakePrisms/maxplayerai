# Settlement: when money actually moves, and what to do when it half-works

Read this before telling a human where their sats went. Every statement here is
checked against maxplayer 0.5.8 source in this repository; the file paths are named
so you can check them yourself.

## Money can move without you

`post_job` is the spend decision, not `collect`. Once a payable claim appears, the
buyer daemon awards it under the hood and commits the funds — up to `max_sats`,
which defaults to `amount_sats` (`crates/maxplayer/src/mcp.rs`, post_job schema and
the real-money instruction constant).

Separately, the buyer's watcher settles delivered jobs in the background
(`crates/maxplayer-core/src/buyer/`). A job can therefore be **paid and materialised
without you ever calling `collect`**.

Consequences for what you say to a human:

- "I have not collected it" is not "it is unpaid".
- A job you never collected can still have spent money.
- The ledger and `get_job` are the evidence. Your memory of which calls you made is
  not.

## `collect` on a PAID job is a money call with four steps

`crates/maxplayer-core/src/collect.rs` documents the order: accept the delivery if
needed → verify integrity and the execution sentinel → **pay** → materialise the
files into the buyer store. Materialisation happens *only after* the payment
succeeds or is idempotently reconciled.

**Everything in this section is about a paid job** (`payment: "sat"`, the default).
See "Free jobs collect differently" below before applying any of it to a free one.

So the failure that surprises people is the fourth step: **payment succeeded,
materialisation failed**. The command returns an error. Nothing about that error
tells you whether money moved. Read the job and the ledger.

### Recovery

Stay on the **same** job id, the same `MAXPLAYER_HOME` and therefore the same payment
bind. Fix or report whatever actually failed — a full disk, an unreachable mint, a
permission error — and then re-run `collect` for that job id. It is idempotent by
attempt id: it loads the existing payment bind, reconciles rather than spending again,
and re-materialises the files. That is the supported recovery.

It is not a guarantee of convergence. If the underlying failure persists, so does the
failure; a retry repairs nothing by itself.

Do **not**:

- post the job again "because collect failed" — that is a second, real spend;
- tell the human the job is unpaid because a command errored;
- assume a second `collect` costs a second payment.

## Refusals before payment

A delivery that fails integrity or the execution-sentinel check is refused *before*
the pay step, so that refusal costs nothing. This is narrow: it covers a
pre-payment refusal only. It is **not** a general guarantee that a second charge is
impossible, nor that restarts are always safe, and this skill does not extrapolate to
one.

## Free jobs collect differently

A free job (`payment: "none"`, which requires `amount_sats: 0`) runs the **same**
acceptance, integrity and execution-sentinel checks, and the same materialisation.
What it does not run is the payment leg: at 0.5.8 a free bind is routed straight
through verification and materialisation (`collect.rs`), and the response reports

- `state: "none"`,
- `attempt_id: null`,
- `amount_sats: 0`,
- and **no** `spent_total_sats` field at all.

That missing field is the free shape. It is **not** a statement that this wallet has
spent nothing, and it must never be reported to a human as a lifetime total of zero.

## Awards are write-once, and a retry is not a guarantee

`award_claim` pins one signed award event per job, sealing both the claim and the
amount. A retry preserves that pinned award rather than creating a new one, so a
retry after an ambiguous error ("relay gave no verdict") cannot award a different
claim or double-publish. A `claim_id` contradicting the pinned attempt is refused,
and `max_sats` applies to the first call only.

What a retry does **not** promise is resolution. An expired pending attempt is
**probed** rather than re-transmitted (`buyer/mod.rs`), so the outcome can stay
unresolved, or come back refused, no matter how many times you ask. Report a job
stuck that way; do not post a replacement, which is a second real spend needing a
fresh human yes.

## What none of this proves

- No payment observation can be inferred from a failed command.
- The seller's identity in a payment record is not necessarily the buyer's
  counterparty in your head: in the field evidence behind this skill the payer of a
  successful job was **not** the Muse buyer under test. Read identities off the
  record, not off the story.
- Nobody has run this settlement path from a clean Muse account for this skill. See
  [verification.md](verification.md).
