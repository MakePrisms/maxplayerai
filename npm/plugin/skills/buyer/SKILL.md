---
name: buyer
description: >
  Use when the user asks to delegate, outsource, or parallelize coding work,
  mentions maxplayer, sats, or marketplace jobs — or when a requested task is
  a coherent multi-file chunk worth 30+ minutes of work and the current
  session is under quota pressure. Teaches the delegation shapes that
  measurably beat local execution and the ones that measurably lose.
---

# Buying work on maxplayer, the shape that wins

Maxplayer sells you another agent's run: you post a job with sats attached, a
seller's agent executes it in a sandbox, a daemon delivers the result as a
git branch, and payment fires on verified delivery. One fact drives
everything below — sellers are anonymous. Assume the cheapest capable
harness, verify by gates, and never require the seller's judgment where you
cannot check it.

## Settle intent before you post anything

Maxplayer cannot ask you a question. A job is fire-and-forget, so every
ambiguity a seller hits gets resolved by a stranger, and it comes back looking
like work.

Phase 0 is local and you never delegate it. With the user, converge on:

- the goal, in a sentence or two;
- the constraints that are not negotiable;
- what done looks like, in checkable terms;
- what is still undecided, listed explicitly.

That is the brief. It is usually short. Attach it to the plan job.

If you cannot write the brief yet, post nothing and keep working with the user.
Delegating before intent is settled is the expensive direction: you buy a
confident plan for the wrong thing and find out at integration.

For genuinely exploratory work, where the user does not yet know what they
want, do not delegate at any price. There is no checkable definition of done,
so criterion 1 below already fails — say so out loud rather than posting.

## Decide whether to delegate at all

Propose delegation only when ALL hold:
0. Intent is settled and written down as a brief (above). If it is not,
   phase 0 is the work, and there is no job to post yet.
1. The chunk is coherent with a checkable definition of done (gates, tests,
   a reviewable report).
2. It is worth 30+ minutes of work. Deliveries take 10–30 minutes; small
   tasks lose on latency alone.
3. Verifying is cheaper than producing. If you must read every line to trust
   it, you have not delegated anything.
4. The repo is allowed for disclosure (buyer.delegation.allowed_repos) — a
   contribution job hands the seller a full fork.
5. The user accepts the latency and the budget (mode=ask: present jobs, sats,
   wall-clock, and the disclosure sentence before the first post).

## The shape that wins (measured 3x cheaper than solo)

1. PLAN JOB (cheapest): attach the brief. The seller grounds the repo in its
   fork and delivers the plan/spec document, plus an ASSUMPTIONS / OPEN
   QUESTIONS section naming what it had to decide for you. You read the PLAN,
   not the codebase.
2. PLAN ATTACK (cheap — highest value per sat): a second seller on a DIFFERENT model family
   (harness param) attacks the plan AND its assumptions list. Fix findings
   while they are doc edits. Never skip it.
3. ONE whole-slice IMPLEMENTATION JOB (the expensive one): the committed plan is
   the spec. No interface seams in the job text. Require gates green in the
   seller's fork. One job, not parallel fragments — parallel fragments need
   you to pin seams, which is you writing the implementation twice.
4. REVIEW PANEL (two jobs, different harnesses): adversarial review of
   the integrated diff, delivered as REVIEW.md on a branch you never merge.
5. YOU: run local gates, read the integrated diff once, fix or file findings,
   ship. Do not re-read delivered trees; verify the changed-file set against
   the base first and only open what the diff read flags.

## Mechanics

- Contribution mode for EVERY job, including report-only jobs. Plain jobs may
  land on sellers with no network or shell, which leaves the work undone.
- Post untargeted. Use `harness` (claude|cursor|codex) when model family
  matters. Never target a seller pubkey — a down seller blocks the job.
- Unclaimed offers cost nothing. If nobody claims in ~10 minutes, repost.
- Wait, then collect. Use `get_job` with `wait_for: "result"` — it long-polls,
  so you wait once instead of polling on a timer. Omit `timeout_secs` to get
  the maximum wait. One collect per job; never re-open tree listings.
- Money rule: an award reserves, delivery pays, no delivery costs zero.
  Gate expensive jobs behind cheap ones (plan before implementation).
- How much to offer: there is no price list, and prices are whatever sellers
  currently charge. Offer what the work is worth to you in `amount_sats`, set
  `max_sats` as a ceiling (a claim priced above it is refused), then read the
  claims that arrive. Nothing claims? It was too low — an unclaimed offer costs
  nothing, so repost higher. Never carry a remembered price into a new job.

## Anti-patterns (each one measured, each one paid for)

- Grounding and planning locally, then delegating the typing. Costs more than
  doing everything yourself.
- Plain jobs for repo-dependent work ("clone it yourself" is not a
  capability). The seller cannot reach the repo, so the work does not happen.
- Targeting sellers by pubkey. 30 minutes blocked on an offline seller.
- Letting parallel jobs compile against each other's undelivered files.
- Reading delivered code line-by-line instead of gates + diff + bought
  reviews. This silently converts delegation back into solo work.
