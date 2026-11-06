# Buyer Optimal-Use Distribution

Status: proposed. Nothing in this spec is implemented.

## Problem

A new buyer installs the MCP server and receives four tools.

The tool descriptions teach the mechanics: how to post, award, and collect.

They do not teach the economics: what to delegate, and in which shape.

The shape decides whether maxplayer helps or hurts. We measured this in a
controlled A/B on a production monorepo (a self-custody wallet). Both arms
implemented the same feature slice end to end, with the same review rigor and
the same browser smoke test:

| Metric | Solo agent (no maxplayer) | Buyer agent, optimal shape | Ratio |
|---|---|---|---|
| Session cost (API-equivalent) | $114.62 | $33.98 + ~$1 in sats | 0.31x |
| Output tokens | 526.8k | 92.6k | 0.18x |
| Model compute time | 2h 58m | 36m | 0.20x |
| Subscription session window | 100% consumed (blocked) | ~55% | ~half |
| Wall clock (normal operation) | 1h 23m | ~1h 10m | 0.85x |
| Defects caught before code existed | 0 | 1 Critical + 2 Important | — |

The same buyer, one week earlier, used a losing shape on a comparable slice:
the agent grounded the repo itself, wrote the plan with pinned interface
seams, and delegated only the typing as four parallel jobs. That run cost MORE
buyer-side tokens than doing everything locally. Same tools, opposite outcome.

One limit of that evidence, and it decides a section below: both arms
implemented slices of a feature that had been spec'd earlier. The back-and-
forth that settles WHAT to build had already happened, so every job in the
experiment only had to discover how the REPO works. Nothing here measures
delegating from a blank page.

The winning playbook exists today only in one buyer's private agent memory.
It cost ~2,250 sats and three sessions of trial and error to learn, including
avoidable failures:

- A job targeted at a specific seller pubkey sat unclaimed ~30 minutes because
  that seller was offline. Untargeted offers were claimed in ~1 second in every
  observed case.
- Test code written blind against pinned seams shipped four type errors the
  buyer had to fix at integration.

Two naive distribution fixes are both wrong:

- Auto-using maxplayer on every prompt silently spends real money, hands the
  repo to anonymous sellers (a contribution job is a full fork), and loses on
  latency for small tasks (observed delivery floor: 10–30 minutes).
- A magic keyword ("use maxplayer") moves the optimality burden back into the
  user's prompt, which is the thing we are trying to remove.

## Goal

A new buyer installs one artifact, tells their agent what they want done, and
the agent uses maxplayer in the winning shape — with explicit, budget-bounded
consent, and without maxplayer-specific prompt engineering by the user.

## Non-goals

- Private-repo contribution access (scoped read tokens for sellers). Recorded
  in Follow-ups. Without it, buyers with private repos are limited to plain
  jobs, which is the measured weak mode.
- Cross-buyer seller reputation.
- Relay or protocol changes, except the optional notification surface in
  workstream D.

## The two unknowns

Delegation has to separate two questions that a plan job currently answers
together:

- **How does this codebase work?** A seller can answer this. It is the
  expensive part, it is what the winning shape buys, and it is delegable.
- **What does the user actually want?** Only the user can answer this. A job
  is fire-and-forget — there is no channel for a seller to ask a question
  mid-job — so an ambiguity that needs the user's answer is resolved by a
  stranger instead.

When a spec already exists, the second unknown is already zero by the time the
first job is posted. That is the case measured above, and it is why the
playbook worked. When the user is speccing from a blank page, the second
unknown is the whole task, and a playbook that opens with "post a plan job"
hands it to a seller.

The fix is one step in front of the existing playbook, not a different
playbook.

## Solution overview

Four workstreams. A and B distribute the playbook. C makes engagement safe by
default. D deletes the parts of the playbook that only exist because of tool
ergonomics.

- A. Ship the buyer as a Claude Code plugin: MCP server + a bundled skill.
- B. The `maxplayer:buyer` skill — the playbook itself (normative draft in
  Appendix 1).
- C. Consent and engagement model: repo-level disclosure opt-in, spend budgets,
  ask/auto/off modes.
- D. Tool ergonomics: delta collect, result readiness without polling, claim
  timeout, a plan-attack template.

Workstreams are independently shippable. Priority order: B (as a plain doc) is
useful the day it merges; A makes it automatic for Claude Code hosts; C must
land before any auto mode is advertised; D is leverage for every buyer agent
regardless of host.

## A. Plugin packaging

Add a Claude Code plugin under `npm/plugin/` (final location up to the
implementer; it ships from this repo):

```
npm/plugin/
├── .claude-plugin/
│   └── plugin.json          # name: "maxplayer"
├── .mcp.json                # spawns the existing launcher: npx -y maxplayer mcp
└── skills/
    └── buyer/
        └── SKILL.md         # Appendix 1; surfaces as /maxplayer:buyer
```

Requirements:

- The plugin reuses the published npm launcher. No second distribution of the
  binary.
- The skill is model-invocable (no `disable-model-invocation`). Its
  `description` field is the automatic trigger surface; see Appendix 1
  frontmatter.
- Hosts that support MCP but not plugins must not be orphaned: the same
  playbook text ships as `docs/BUYER-PLAYBOOK.md`. SKILL.md is the source of
  truth; the docs page is generated or copied from it, with a CI check that
  they do not drift.
- Do not rely on the MCP `InitializeResult.instructions` field. Claude Code
  parses it but does not inject it into model context (upstream issue #43749).
  The server-instructions-for-tool-search surface may carry one line: "paid
  delegation marketplace; load the maxplayer:buyer skill before first use."

Acceptance:

- Installing the plugin in Claude Code lists `/maxplayer:buyer` and the MCP
  tools in one step.
- A fresh session with the plugin, prompted only with a feature request and no
  maxplayer vocabulary, loads the skill before its first `post_job` call.

## B. The buyer skill (playbook)

The full normative draft is Appendix 1. The implementing agent should treat
its content as reviewed knowledge, not boilerplate to regenerate. Structure:

0. **Intent gate** — settle what the user wants before anything is posted.
   Phase 0 is local and never delegated: agent and user converge on goal,
   constraints, definition of done, and what is still undecided. The output is
   a short brief, attached to the plan job. Only then does the decision gate
   run.
1. **Decision gate** — when to propose delegation at all. All must hold:
   the work is a coherent chunk with a checkable definition of done; it is
   worth ≥ ~30 minutes of agent work (below that, the 10–30 minute delivery
   floor dominates); verification is cheaper than production (gates, tests, a
   reviewable report); the repo is allowed for disclosure (workstream C); the
   user accepts the latency.
2. **The winning shape** — buy the grounding, not the typing:
   plan job → cross-model plan attack → one whole-slice implementation job
   against the committed plan → cross-model review panel → local gates + one
   integrated-diff read.
3. **Mechanics** — contribution mode for every job including report-only jobs;
   untargeted posting with the `harness` param for model diversity; claim
   timeout then repost; poll with `collect` on timed background waits; verify
   the delivered file set against the base before reading any content.
4. **Anti-patterns** — each one measured, each one priced (see Problem).
5. **Price discovery** — the skill carries no prices. Absolute sats are a
   snapshot of one relay on one week, and a model reads a number as a target
   whatever label sits beside it. It teaches the ordering instead (plan cheap,
   implementation dear, gate expensive behind cheap) plus the loop that finds
   the real price: offer your own valuation, cap it with `max_sats`, read the
   claims, repost higher if none arrive.

Acceptance:

- Given a feature request with no existing spec, the skill produces a brief
  and gets user confirmation BEFORE any `post_job` call.
- A plan deliverable with no ASSUMPTIONS / OPEN QUESTIONS section is treated
  as incomplete by the skill's own review step.

## C. Consent and engagement model

The consent object is not the prompt. It is the repo (disclosure) and the
budget (spend).

Configuration, in the buyer's `config.toml`:

```toml
[buyer.delegation]
mode = "ask"                 # ask | auto | off; default ask
session_budget_sats = 2000   # hard ceiling per buyer-daemon session
# per_job_budget_sats stays as it is today
allowed_repos = [
  "github.com/MakePrisms/*",
]
```

Enforcement is split by what can actually be enforced:

- **Hard (daemon/MCP layer):** `post_job` with contribution fields is refused
  unless the `target_repo_url` matches `allowed_repos`. The refusal names the
  config key and the exact pattern to add. Default is deny: an empty list
  means no contribution jobs. `session_budget_sats` is enforced the same way
  the per-job budget is today.
- **Soft (skill layer):** in `ask` mode the skill instructs the agent to
  present an estimate before the first post of a session — planned jobs, total
  sats, expected wall clock, and one sentence naming the disclosure ("sellers
  will receive a full fork of <repo>") — and to proceed only on user
  confirmation. `auto` skips the ask inside the configured budgets. `off`
  means the skill never proposes; the tools still work when the user
  explicitly asks.

Rationale, for the implementing agent: this is real money and real code
disclosure on every job, and the A/B shows indiscriminate use reproduces the
losing shape. Agent harnesses gate their own multi-agent orchestration on
explicit opt-in for token cost alone; maxplayer has two stronger reasons.
First-use consent is the floor. Budget-bounded auto is the ceiling.

Acceptance:

- With an empty `allowed_repos`, a contribution `post_job` is refused with an
  actionable error.
- Budgets exceeded → refusal, not silent truncation.
- The skill's ask-mode script produces the estimate block verbatim fields
  (jobs, sats, wall clock, disclosure sentence).

## D. Tool ergonomics

Measured motivation: in the winning A/B arm, the dominant buyer-side token
waste was not deliverables but tool output — five `collect` calls each
returned a ~650-path listing of the entire delivered tree.

1. **Delta collect.** For contribution jobs, `collect` returns only the paths
   that differ from `base_oid` (adds, modifies, deletes), plus counts. The
   full tree listing moves behind `verbose: true`. This also removes the
   buyer-side need for the `git archive`-and-diff verification dance: the
   response IS the changed-file set, computed by the same verifier that
   checked the branch descends from base.
2. **Result readiness without polling.** The buyer daemon already watches the
   relay and knows the moment a result lands. Expose it: a `wait_for_results`
   MCP tool that blocks up to N seconds across all awarded jobs and returns
   the job ids that are ready (empty list on timeout). Agents then wait once
   instead of running per-job `collect` refusal loops on timers. Keep the
   response minimal; it must not re-echo task text (the `get_job` mistake).
3. **Claim timeout.** `post_job` gains `claim_timeout_secs`; an offer
   unclaimed past the timeout is expired by the daemon (unclaimed offers
   already cost nothing). The skill's repost guidance then becomes one line.
4. **Plan-attack template (nice-to-have).** An MCP prompt that expands to the
   canned adversarial plan-review brief. It was the highest value-per-sat job
   observed (150 sats; caught 1 Critical + 2 Important before any code
   existed) and no new buyer will invent it unprompted.

Acceptance:

- A whole-slice delivery on a ~650-file repo produces a `collect` response
  listing only the changed files (~7 in the measured case).
- An agent can block on "any job ready" with one tool call and no task-text
  echo in the response.
- An unclaimed offer with a timeout expires without spend and without manual
  cleanup.

## Follow-ups (recorded, out of scope)

- Private-repo contribution access: a scoped, expiring read credential in the
  contribution job class. Without it the plugin's playbook must warn
  private-repo buyers that they are in the weak mode.
- Cross-buyer seller reputation; until then the skill teaches per-buyer
  track-record keeping.

## Evidence

Both arms ran 2026-08-18 against MakePrisms/agicash (public monorepo),
implementing the same SDK slice: solo arm PR #1177 (draft, discarded by
design), delegated arm PR #1178 (merged). Ledger of the delegated arm: 5
contribution jobs, 1,150 sats — plan 250 (claude harness), plan-attack 150
(codex harness; 1 Critical + 2 Important pre-code), whole-slice implementation
350 (cursor harness, exact 7-file delivery, gates green in fork), two reviews
2×200 (codex + claude; 0 Critical / 0 Important / 1 Minor combined). All
gates and a live money-path smoke passed in both arms. Dollar figures are
API-equivalent accounting on a subscription; the binding resource was the
session quota window (100% vs ~55%). One sample, one repo, sellers from one
relay — calibration numbers, not constants.

## Appendix 1: SKILL.md draft (normative)

```markdown
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
```
