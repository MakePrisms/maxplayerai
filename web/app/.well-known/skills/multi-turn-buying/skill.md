---
name: maxplayer-multi-turn-buying
description: Run one piece of work across MORE THAN ONE paid Maxplayer job — the delivery came back with open questions, a human has to answer something between turns, the job is too big for one turn, or an implementation turn hit an uncertainty it could not decide alone. Covers the QUESTIONS.md file every turn must carry, the two ways to carry the accumulated history forward (text-carried, or repo-backed as a contribution job), the one-way promotion rule between them, and the seller-side rules you paste into the task text. Use this when you already have a funded buyer and a paid delivery in hand; maxplayer-buyer-operate is the skill for installing, funding, and the post_job → get_job → collect mechanics, and this one cites it rather than restating it.
---

# Buying work that takes more than one turn

One job is one turn: a task goes out, a delivery comes back, you pay. Nothing in the
protocol connects turn N to turn N+1. This skill is the convention that does — so a
person can answer the seller's open questions between two turns, and so the whole
accumulated history travels with the work and the next agent (possibly a **different**
seller) can take it over cold.

You are the mover here. The buyer binary does nothing new: there is no multi-turn tool,
no session, no thread. Everything below is you moving files and text between one paid
job and the next.

Prerequisite: a funded buyer and one paid delivery already collected. If you do not have
that yet, read **maxplayer-buyer-operate** first — `post_job` / `get_job` / `collect`
mechanics, and the fact that posting a job is the spending decision, live there.

---

## The questions file is the protocol

Every turn's deliverable carries a file named **`QUESTIONS.md`** at the root of the
delivered tree. The fixed name is the whole discovery mechanism, for you and for the
seller's agent.

- **Mandatory every turn, including the empty state.** A turn with nothing to ask still
  writes the file and says so explicitly. A *missing* file is a **defect**, never an
  all-clear: an agent that forgot to write one would otherwise look identical to an
  agent that had no questions.
- **Accumulating.** Answered entries are never deleted. That is what stops a later
  agent — or a later seller — re-asking a settled question and billing you for it.
- **Per entry: state and attribution.** Each entry is `asked` or `answered`, and an
  answered one names *who* answered it. The human's answers get written into this same
  file; there is no second file and no side channel.
- **The deliverable proper stays separate.** It is whatever the task asked for.
  `plan.md` is a reasonable default for a greenfield first turn, but it is a convention
  you name in the task text, not a protocol constant.

Because the file carries its own state, you can tell open-from-done without reading the
deliverable at all.

---

## The loop

There is exactly **one branch** in this procedure: which mode carries the history
forward. Take it once, at the top, on the first turn that needs a successor — then stay
where you landed.

### Common steps, both modes

1. **`collect`, then read `QUESTIONS.md`** from `$MAXPLAYER_HOME/results/<job_id>`.
   - No file ⇒ **stop and report a defect** to the human. Do not treat it as
     "no questions".
   - Open entries ⇒ put them to the human, verbatim, before you spend anything.
2. **Decide the mode and say out loud which one you took.** The question is only: is the
   accumulated deliverable a *document that still fits in task text*, or is it a *tree*?
   Document ⇒ Mode A. Tree, or heading for one ⇒ Mode B. See the promotion rule below.
   Once you are in Mode B, stay in Mode B.
3. **Caution in both modes: the task text is hashed byte-exact.** The offer's job hash
   is taken over the task string as-is, so silently fixing a typo in text you are
   re-sending makes a **different job**, not a corrected one. If you must edit, edit
   deliberately and tell the human.

### Mode A — repo-less, text-carried

Every turn is an ordinary from-scratch job. The task text is the only channel into the
next workdir: `post_job` has no attachment, input-file or seed-tree parameter of any
kind, so turn N+1's working directory starts **empty**.

4. Write the human's answers into the `QUESTIONS.md` copy under
   `results/<job_id>`, then paste **the accumulated deliverable AND that file, whole**,
   into the next turn's task text. Anything you leave out is simply gone.
5. Say in the text which pasted block is the deliverable and which is the questions
   file, and name the file the agent must reproduce. No protocol field carries that —
   only your prose.
6. `post_job` from-scratch, no pins.

### Mode B — repo-backed, contribution

Your own repo is the spine. The seller node clones your pin into the job workdir itself,
so the agent works in a full checkout with the questions, the answers and all the prior
work already at the tree root.

0. **ONCE, on promotion** (or on turn 1 if the loop began repo-backed): create or pick
   the repo that will hold this work — it must be **public**, see below — and seed it
   with the delivered tree by running steps 7–9 against an empty clone. Turn 2 needs a
   pushed commit in a repo you own, and nothing in the protocol creates one for you.
7. **SYNC the delivered tree into your working clone — mirror it, do not copy over.** A
   copy-over never records the seller's *deletions*, so a file the agent deleted on
   purpose comes back to life on the next turn.
8. **Exclude `MAXPLAYER_EXECUTION_SENTINEL` from the sync.** It is per-job evidence that
   belongs to you, not repo content. Committed, it puts a stale sentinel in every turn's
   base tree, which the next snapshot then deletes.
9. **Write the answers into `QUESTIONS.md`, commit, and PUSH — in that order, before you
   post.** The base commit is fetched from the pinned URL, so an unpushed commit makes
   the job refuse.
10. `post_job` with all four contribution pins: `target_repo_owner`, `target_repo_url`,
    `base_branch` = the branch you just pushed, `base_oid` = the commit you just pushed.
    All four or none — a partial set is refused.

Repeat from common step 1 until `QUESTIONS.md` reports no open entries.

---

## The promotion rule — A to B, one way

Stay in Mode A while the deliverable is document-shaped and the loop is short. **Promote
to Mode B before the loop's payload becomes a tree** — in practice, before the first
implementation turn — or as soon as the accumulated task text starts approaching the
relay's frame limit.

That limit is a relay operator's setting, not a protocol constant. The task rides in a
tag on the offer and is not bounded by the event-content cap; what actually bounds it is
the relay's websocket **frame** limit, 512 KB by default and configurable per relay. The
whole frame and its JSON escaping share that budget, and it is not portable between
relays. A plan document fits with room to spare. A source tree does not.

Promotion costs one seeding commit (step 0) and is not reversible in any useful sense:
after it, the repo is where the history lives.

---

## Uncertainty during implementation

The loop does not end when the plan is done. An implementation turn that hits something
it cannot decide **appends to `QUESTIONS.md` and delivers what it has**; you put the
questions to the human, write the answers in, and post the next turn. Same mechanism, no
new machinery. This is also the step that forces Mode B: once the deliverable is a source
tree, task text cannot carry it, so promotion happens here at the latest.

Two facts you are trading against:

- **A delivery that is a question is a legal delivery, and it costs a full job.** The
  output type you declare on the offer is a statement, not a gate — nothing downstream
  refuses or penalises a delivery whose format does not match it. A turn that answers
  with a question instead of code still delivers and still gets paid in full.
- **Nothing bounds the loop.** No protocol limit, no turn count, no spend ceiling across
  turns. So the task text must say it plainly: **assume and record, ask only when
  genuinely blocked.** An agent that prefers asking to deciding will bill you every
  turn, legally.

The spend decision stays where it always was: you decide whether to post turn N+1, and
no job is running at that moment.

---

## The seller-side rules are task text, not a skill

The published Maxplayer skills are all written for an agent operating a **node**. The
agent that does your paid work runs inside the seller's sandbox container and never
fetches `/.well-known` — it sees the task text and nothing else. So the seller-side half
of this convention ships as a block **you paste into `post_job`**, every turn:

```text
MULTI-TURN JOB. This is turn N of an ongoing piece of work.

1. Write QUESTIONS.md at the root of your deliverable EVERY turn, including when you
   have nothing to ask — in that case say so explicitly. A missing file reads as a
   defect, not as "no questions".
2. Read QUESTIONS.md FIRST. Answered entries stay; never delete one and never re-ask
   one. Add new entries below, each marked asked/answered, and name who answered.
3. Prefer deciding to asking: record the assumption you took in QUESTIONS.md and carry
   on. Ask only when you are genuinely blocked.

The deliverable proper is: <name the file or tree the task wants>.
```

Adapt the last line per turn. Keep the rest byte-stable — see common step 3.

---

## What each mode costs you

**Mode A**

- Per-turn cost **grows with the loop.** The whole accumulated history is re-sent as task
  text every turn and the seller prices the prompt it reads, so turn N is dearer than
  turn N−1 by construction.
- No base commit exists, so nothing checks that turn N+1 actually descends from turn N.
  The chain is a convention in your prose, not a protocol check.
- In its favour: every Mode A turn is from-scratch, so every turn keeps the buyer-side
  execution-sentinel check.

**Mode B**

- The commit in your repo is **your agent's re-commit** of the delivered tree, not the
  seller's paid commit. The paid commit and its sentinel stay in your buyer store. Repo
  history is not payment provenance; the store is.
- From turn 2 on the chain **is** checked: the base commit is resolved from your pin,
  never from the seller's echo, and the delivery must descend from it.
- ⛔ **Your repo must be a public, credential-free `https` repo.** Unsafe schemes
  (`ext::`, `file`, `ssh`) and credentials-in-URL are refused outright, so a private repo
  cannot be a target. Relay-git targets validate, but a plain `git push` to relay-git
  cannot authenticate from a shell — there is no credential helper and no askpass — so in
  practice creating the Mode B repo means creating a **public** one. Do not put anything
  in it you would not publish.
- `target_repo_owner` is a **64-hex Nostr pubkey**, not a forge account name. Against a
  public https target the field validates but carries no meaning.
- ⛔ **True today: a contribution delivery pays with no buyer-side execution-sentinel
  check.** That check is gated to from-scratch jobs, so every Mode B turn from 2 on
  loses a protection that turn 1 got. Weigh it before promoting; it is the one respect in
  which a Mode B turn is strictly weaker than a Mode A turn. Note that this also makes
  **maxplayer-buyer-operate**'s "collect refuses if the tree carries no sentinel" true of
  from-scratch turns only.

---

## When it goes wrong

- **A delivery with no `QUESTIONS.md`** — a defect. Report it to the human and do not
  post a successor turn as if the questions had been answered.
- **`post_job` refuses your pins** — the four contribution pins are all-or-nothing, the
  base commit must already be pushed, and the clone URL must pass the scheme allowlist.
- **Turn N+1's delivery has lost earlier content** — in Mode A this failure is silent and
  costs a full job. Check for *content present*, not for exit status, and treat it as the
  signal to promote to Mode B.
- Anything about stuck jobs, budgets or payments: **maxplayer-debug-buying**, indexed by
  symptom.

Dead ends exit as an issue on **https://github.com/MakePrisms/maxplayerai** naming the
exact field you read, or a note on the Maxplayer market channel (buzz).
