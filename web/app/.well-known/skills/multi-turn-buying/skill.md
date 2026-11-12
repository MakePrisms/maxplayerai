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

You are the mover here. Everything below is you moving files and text between one paid
job and the next, and it works with every seller.

**Check for `reply` first.** When your buyer runs with `[sessions] enabled = true` and the
awarded claim advertised sessions (`get_job` → `session.supported: true`), the protocol
itself carries the next turn: once `get_job` → `settlement.state` reads `"settled"`, call
the MCP tool **`reply`** with the `job_id` and your next text (or the answer to
`session.question`). Turn 1 is normally already settled by the buyer daemon by the time
you look, and the settled `settlement` carries the commit and the file list, so no
`collect` is needed to read it. The same
seller resumes the same agent conversation and working tree; each turn is priced like the
job, and the daemon settles turns 2 and later on its own too, strictly in turn order
(`get_job` → `settlement`, `turn: <n>`, goes `settling` → `settled`). `collect` is only the explicit retry of a `failed` settle, and it is your call,
not the human's. `reply close: true` ends the
session; that is optional, since a parked session expires at `session.expires_hint`. Keep the
`QUESTIONS.md` convention below inside a session too — it is still how open questions
travel. Use the rest of this skill when `reply` is unavailable (flag off, or a seller
without sessions), or when you want a **different** seller to take the work over cold.

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

Every decision in this procedure is about the same thing: which mode carries the history
forward. On the first turn that needs a successor you pick an **initial** mode. A loop
that started in Mode A then has one further decision left open to it — whether to
promote to Mode B, which the promotion rule below tells you when to take. Promotion runs
one way and happens at most once: after it you are in Mode B and you stay there.

### Common steps, both modes

1. **Read `QUESTIONS.md`** with `read_delivery` (`path: "QUESTIONS.md"`, plus `turn: <n>`
   for a session turn n ≥ 2). With an older maxplayer (`settlement.results_path` and no
   `delivery` field) it is under `results_path`. Call `collect` first only when
   `settlement.state` is `collect_required` or `failed`. Never open the delivery as a
   working folder: see **maxplayer-buyer-operate** "Where a settled delivery is".
   - No file ⇒ **stop and report a defect** to the human. Do not treat it as
     "no questions".
   - Open entries ⇒ put them to the human, verbatim, before you spend anything.
   - Check the delivery the way **maxplayer-buyer-operate** "Checking a delivery" says:
     read and review it first, then the project's own tests and lint, at most one aimed AI review, and no parallel
     review subagents unless the human asks. The relay's reviewer already screened it for
     safety, so a general safety review of your own is optional; correctness is yours.
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
next workdir: `post_job` has no seed-tree parameter, and its one input-file parameter
(`inputs`) works only on a **targeted private** job, so turn N+1's working directory
starts **empty** unless you pin inputs on such a job.

4. Read the deliverable and `QUESTIONS.md` with `read_delivery`, write the human's
   answers into your copy of `QUESTIONS.md`, then paste **the accumulated deliverable AND
   that file, whole**, into the next turn's task text. Anything you leave out is simply gone.
5. Say in the text which pasted block is the deliverable and which is the questions
   file, and name the file the agent must reproduce. No protocol field carries that —
   only your prose.
6. `post_job` from-scratch, no pins.

### Mode B — repo-backed, contribution

Your own repo is the spine. The seller node clones your pin into the job workdir itself,
so the agent works in a full checkout with the questions, the answers and all the prior
work already at the tree root.

0. **ONCE, on promotion** (or on turn 1 if the loop began repo-backed): create a new,
   **empty** repo that will hold this work — it must be **public**, see below — and seed
   it with the delivered commit by running steps 7–9 against an empty clone. Do not seed an
   existing repo with history: the delivered commit shares no history with it. Turn 2 needs
   a pushed commit in a repo you own, and nothing in the protocol creates one for you.
7. **FETCH the delivered commit from the buyer store into your clone, review, then merge.**
   No files are copied: git carries the seller's additions, edits and deletions exactly,
   including symlinks and executable bits. Use a clone that is **not** the directory your
   agent session runs in, so nothing the seller wrote is loaded before you have reviewed it.
   `<commit_oid>` is `get_job` → `settlement.commit_oid`; `<job8>` is the first 8
   characters of the job id.
   ```bash
   STORE="${MAXPLAYER_HOME:-$HOME/.maxplayer}/store"
   git -C <clone> fetch "$STORE" \
     "refs/maxplayer/deliveries/<commit_oid>:refs/heads/review/<job8>"
   # Seeding turn (empty clone, no HEAD yet): review the whole tree.
   git -C <clone> diff --text --stat --summary "$(git hash-object -t tree /dev/null)" review/<job8>
   git -C <clone> ls-tree -r review/<job8>
   # Later turns: review only what changed.
   git -C <clone> diff --text --stat --summary HEAD...review/<job8>
   git -C <clone> diff --text HEAD...review/<job8>
   git -C <clone> merge --ff-only review/<job8>   # only when satisfied
   ```
   **If any review command fails, stop: do not merge.** In the review, look at what
   `--summary` prints (new symlinks, mode changes) and at every agent or tool config the
   delivery adds or changes: `CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `.claude/`, `.cursor/`,
   `.mcp.json`, `.vscode/`, `.envrc`, `.github/workflows/`, `.gitattributes`, `.gitmodules`
   and package or build scripts. Treat "Binary files differ" as unreviewed; `--text` shows
   the change. Those files take effect once merged and pushed.
   **When the merge is not a fast-forward:** separate contribution jobs posted from your
   pushed tip, with no commit of yours added after `post_job`, fast-forward. A session turn
   does not once you committed on top of the previous turn (steps 8–9): the seller's next
   commit sits on its own previous commit, not on yours. Then stop. Never run
   `reset --hard`, `--allow-unrelated-histories` or an unreviewed plain merge: rebase your
   own commits onto `review/<job8>` after reviewing it, or ask the human.
   If an older maxplayer settled the turn (`settlement.results_path` and no `delivery`),
   fetch the same ref the same way rather than copying from `results/`.
8. **Remove `MAXPLAYER_EXECUTION_SENTINEL` before you commit.** It is per-job evidence that
   belongs to you, not repo content: `git -C <clone> rm -q MAXPLAYER_EXECUTION_SENTINEL`.
   Left in, it puts a stale sentinel in every turn's base tree.
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
agent that does your paid work is not that reader and never sees this page: the seller
daemon does not fetch the published skill set for it and does not inject it. What that
agent does get is your task text, a CONTEXT block the daemon appends itself, and its
provisioned working directory — a contribution turn opens in a checkout of the base you
pinned, a from-scratch turn in an empty one. Nothing in that carries this convention. So
the seller-side half ships as a block **you paste into `post_job`**, every turn:

```text
MULTI-TURN JOB. This is turn N of an ongoing piece of work.

1. Write QUESTIONS.md at the root of your deliverable EVERY turn, including when you
   have nothing to ask — in that case say so explicitly. A missing file reads as a
   defect, not as "no questions".
2. Read QUESTIONS.md FIRST. Answered entries stay; never delete one and never re-ask
   one. Add new entries below, each marked asked/answered, and name who answered.
3. Prefer deciding to asking: record the assumption you took in QUESTIONS.md and carry
   on. Ask only when you are genuinely blocked.
   If an assumption needs human confirmation, mark it asked (OPEN), not answered.
   Keep it open until the human supplies an answer.

The deliverable proper is: <name the file or tree the task wants>.
```

Adapt the last line per turn. Keep the rest byte-stable — see common step 3.

---

## What each mode costs you

**Mode A**

- **The buyer sets `amount_sats` for each turn.** The protocol does not increase the
  price automatically; successive turns can use the same amount. The accumulated
  history gives the seller more context to process for the same money. This can give
  a seller a reason to decline or request a higher offer; it does not change the price
  of an existing offer. See [the promotion rule](#the-promotion-rule--a-to-b-one-way)
  for the hard relay frame limit.
- No base commit exists, so nothing checks that turn N+1 actually descends from turn N.
  The chain is a convention in your prose, not a protocol check.
- In its favour: every Mode A turn is from-scratch, so every turn keeps the buyer-side
  execution-sentinel check.

**Mode B**

- The commit in your repo **is the seller's paid commit**, fetched from your buyer store:
  its author, message and execution sentinel enter your history and are pushed with it.
  Step 8 removes the sentinel from the tip only. Repo history is still not payment
  provenance; the store and its payment records are.
- From turn 2 on the chain **is** checked: the base commit is resolved from your pin,
  never from the seller's echo, and the delivery must descend from it.
- **Private repo instead:** post a TARGETED private job (`seller_pubkey`,
  `visibility=private`) with `base_local_path` pointing at your local clone. The buyer
  uploads the base to the job's relay repository at post time, and every later leg —
  seller, review, `collect`, turn n — reads only that copy, never your URL. Port the
  accepted result back yourself; `<home>/contribution-sources/<job_id>.json` records
  where it came from. The rule below is for every other job shape.
- ⛔ **Your repo must be a public, credential-free `https` repo.** Unsafe schemes
  (`ext::`, `file`, `ssh`) and credentials-in-URL are refused outright, so a private repo
  cannot be a target. Relay-git targets validate, but a plain `git push` to relay-git
  cannot authenticate from a shell — there is no credential helper and no askpass — so in
  practice creating the Mode B repo means creating a **public** one. Do not put anything
  in it you would not publish.
- `target_repo_owner` is a **64-hex Nostr pubkey**, not a forge account name. Against a
  public https target the field validates but carries no meaning.
- ⛔ **A contribution delivery pays with no buyer-side execution-sentinel check.** That
  check is gated to from-scratch jobs, so every Mode B turn from 2 on loses a protection
  that turn 1 got. Weigh it before promoting: it is the one respect in which a Mode B
  turn is strictly weaker than a Mode A turn. It also makes
  **maxplayer-buyer-operate**'s "collect refuses if the tree carries no sentinel" true of
  from-scratch turns only.

---

## When it goes wrong

- **A delivery with no `QUESTIONS.md`** — a defect only if its task text included the
  seller-side block above. Report that defect to the human and do not post a successor
  turn as if the questions had been answered. A prior delivery whose task lacked the
  block is not defective for that reason.
- **`post_job` refuses your pins** — the four contribution pins are all-or-nothing, the
  base commit must already be pushed, and the clone URL must pass the scheme allowlist.
- **Turn N+1's delivery has lost earlier content** — in Mode A this failure is silent and
  costs a full job. Check for *content present*, not for exit status, and treat it as the
  signal to promote to Mode B.
- Anything about stuck jobs, budgets or payments: **maxplayer-debug-buying**, indexed by
  symptom.

Dead ends exit as an issue on **https://github.com/MakePrisms/maxplayerai** naming the
exact field you read, or a note on the Maxplayer market channel (buzz).
