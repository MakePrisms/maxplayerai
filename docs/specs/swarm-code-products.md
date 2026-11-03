# Swarm code products — a swarm run that ships a small app

**Status:** design spec. No code written. **Anchor commit: `b08fc1e84473fb2f3bd4fd71dd0c655fb1080325`
(= `origin/main` at the time of writing, 2 Nov 2026).** Every line number below was re-derived by
`git grep -n -F` at that commit and is written `file:line @b08fc1e8`. A script printed each cited line
at the anchor before publication.

**Scope.** Today a swarm run answers a question: every job is text, and the run ends when the planner
says a verified answer exists. This spec turns a run into a small software project. Sellers deliver
commits against a repository we own. The runner tests each commit in a sandbox, merges it or hands it
back to the planner, and the run ends when the acceptance tests pass on the repository head.

---

## 0. Settled inputs (not relitigated here)

1. **Bob, #swarm-updates, 2 Nov 2026 23:02 UTC:** "yes lets spec it out". Start with a small pilot: one
   small app with a written spec, a repository we own, the runner in contribution mode with
   one-at-a-time merges, on 2–3 of our own seats. Decide afterwards whether to open it to members.
2. **Bob's standing preference (#1108, Oct 2026):** keep v1 as simple as possible. One implementation
   PR behind a default-off flag where possible, only bounds that stop unbounded spend or a leak, and a
   "Deliberately left out" section.
3. **Bob, 31 Oct 2026:** the runner operator decides what the planner sends. Unusable deliveries are
   still paid.
4. **Standing rule:** no swarm PR is merged while a swarm day is live.

---

## 1. Load-bearing checks (done before writing the design)

### 1.0 A premise correction first: the swarm posts PUBLIC targeted jobs, not private ones

The brief assumed the swarm posts private targeted jobs. At the anchor it does not. Every assignment
is posted with `visibility:'public'` and a `seller_pubkey`
(`scripts/challenges/swarm.mjs:1857 @b08fc1e8`), and the local E2E asserts exactly that
(`scripts/challenges/e2e.py:119 @b08fc1e8`). This matters because the board and the runner snapshot
only see a job through the public `[swarm:<buyer>:<id>]` prefix in the offer's `i` tag, and they drop
offers without a hex `p` tag (`web/app/src/challenge/shared/model.mjs:84 @b08fc1e8`). A targeted
private offer must not carry an `i` tag at all
(`crates/buzz/crates/maxplayer-private-protocol/src/wire.rs:548 @b08fc1e8`). A private code job would
therefore drop out of the board, the minutes meter and the seller busy tracking.

**Decision:** v1 posts **public targeted contribution jobs**, so the board, metering and budget keep
working unchanged. Private targeted contribution was still checked (1.1), because it is the route
for a later private product repository.

### 1.1 (a) Does contribution mode work end to end?

**Private targeted (what the brief asked about).** The docs call it "the supported combination"
(`docs/PRIVATE-JOBS.md:64 @b08fc1e8`). After posting, the job's repository is the relay copy and
nothing else (`docs/PRIVATE-JOBS.md:68 @b08fc1e8`).

- **Proven by running, at the anchor.** Two existing integration tests were run in the repo devshell
  (cargo 1.95.0, `--features wallet`). Both passed, 1/1 each:
  - `private_contribution_reads_only_the_relay_job_repo_after_posting`
    (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:398 @b08fc1e8`). It does a real
    private targeted post from a local checkout. A **paid** collect then clears every delivery gate
    and stops only at a dead mint, with zero spend
    (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:527 @b08fc1e8`). A **free** collect
    materializes the delivered files
    (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:606 @b08fc1e8`).
  - `local_base_post_uploads_only_pin_and_falls_back_without_leaking_or_bypassing_policy`
    (`crates/maxplayer-core/tests/local_contribution_base.rs:44 @b08fc1e8`).
  - The first attempt failed before any test logic ran: this host exports `MAXPLAYER_CONTENT_PRIVATE_KEY`,
    and the home bootstrap refused the unknown field. With that variable unset, both tests passed.
    This is a host environment quirk, not a product failure.
- **What those tests do NOT prove.** The seller is simulated. `deliver()` is a plain git clone,
  commit and push straight into the fixture repository
  (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:352 @b08fc1e8`). The claim and
  award are signed by the test, and the accept-bind is written directly
  (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:516 @b08fc1e8`). No relay, seller
  node or agent runs. **Buyer post and buyer collect/verify are proven. The seller side is read only.**
- **Seller side (read only).** A private contribution is served only when `[seller]
  contribution_enabled` is on (`crates/maxplayer-core/src/seller_node/run.rs:7482 @b08fc1e8`), which
  defaults to on (`crates/maxplayer-core/src/home.rs:1493 @b08fc1e8`). Execution requires the
  pre-claim input cache with an `input-baseline` ref
  (`crates/maxplayer-core/src/seller_node/privacy.rs:493 @b08fc1e8`). The container mounts that cache
  read-only (`crates/maxplayer-core/src/seller_node/run.rs:9859 @b08fc1e8`) and clones the base from it
  (`crates/maxplayer-core/src/seller_node/run.rs:9710 @b08fc1e8`,
  `crates/maxplayer-core/src/delivery_orchestrator.rs:235 @b08fc1e8`). The delivery goes to the per-job
  relay repository on `refs/heads/delivery/<id>`
  (`crates/maxplayer-core/src/seller_node/privacy.rs:506 @b08fc1e8`,
  `crates/maxplayer-core/src/seller_node/privacy.rs:507 @b08fc1e8`).
- **Not run:** no E2E in the repository drives a real seller node plus a real relay through a
  contribution job. `scripts/` contains no contribution post, and the swarm E2E only posts text
  jobs. The swarm `run-e2e.sh` was not run, because it would not touch this path.

**Public targeted (what v1 posts).** Read only. The buyer validates a public base once, before
publication (`crates/maxplayer-core/src/job_lifecycle.rs:940 @b08fc1e8`). The seller clones it at
`base_oid` onto `maxplayer/contribution/<job_id>`
(`crates/maxplayer-core/src/seller_node/run.rs:2651 @b08fc1e8`). It signs the authorship tuple into
the result (`crates/maxplayer-core/src/seller_node/run.rs:2792 @b08fc1e8`). A contribution can never
settle inline (`crates/maxplayer-core/src/job_lifecycle.rs:1887 @b08fc1e8`;
`docs/protocol-v1.md:699 @b08fc1e8`), and it must be exactly one commit parented on the base
(`docs/protocol-v1.md:917 @b08fc1e8`).

Two gaps decide the design:

- **A relay-git base fails on a default docker seat.** The container clones the base with no
  credential (`crates/maxplayer-core/src/delivery_orchestrator.rs:245 @b08fc1e8`;
  `docs/SELLER-QUICKSTART.md:859 @b08fc1e8`). Relay-git requires NIP-98 for reads
  (`crates/maxplayer-core/src/seller_git.rs:209 @b08fc1e8`). Container delivery is the default on a
  docker seat (`crates/maxplayer-core/src/home.rs:791 @b08fc1e8`). Only the host path passes the
  seller key (`crates/maxplayer-core/src/seller_node/run.rs:2703 @b08fc1e8`). The buyer's post-time
  check waves relay-host repositories through as member-readable
  (`crates/maxplayer-core/src/job_lifecycle.rs:906 @b08fc1e8`), so the post succeeds and the seller
  then fails at clone. **A public code job therefore needs an anonymously readable https base, such as a
  public GitHub repository.** This is read from source and was not reproduced with docker, which is
  absent on this VPS.
- **A FREE public contribution is only tip-matched at collect.** Accept still checks that the
  seller's echoed pins equal the signed offer (`crates/maxplayer-core/src/job_lifecycle.rs:2430 @b08fc1e8`),
  but `collect_free` runs the
  descendant/content check only when private evidence exists
  (`crates/maxplayer-core/src/collect.rs:432 @b08fc1e8`). It skips the sentinel for contributions,
  with the note "not served free yet" (`crates/maxplayer-core/src/collect.rs:469 @b08fc1e8`). A PAID
  collect runs the full `verify_contribution` (`crates/maxplayer-core/src/authorize_pay.rs:462 @b08fc1e8`).
  **So the runner checks ancestry and shape itself, whatever the payment mode (§4.3).**

### 1.2 (b) What git access does a seller need?

**Nothing beyond a default seat.** Delivery defaults to the seller's own repository on the hosted
relay-git (`docs/SELLER-QUICKSTART.md:1089 @b08fc1e8`). Announce, probe and push run in-process with
NIP-98 signed by the seller key. No external `git`, credential helper or `git-credential-nostr` is
needed (`docs/SELLER-QUICKSTART.md:1102 @b08fc1e8`). `--git-remote` is optional and only for bringing
your own public https remote. **No GitHub credentials are needed.** The public base is cloned
anonymously, and the seller never pushes to the product repository.

What a member seat does need:

- `contribution_enabled` left at its default, on. A seat that turned it off logs "contribution offers
  disabled" and never claims (`crates/maxplayer-core/src/seller_node/run.rs:7497 @b08fc1e8`). A
  targeted job to it simply expires.
- Egress to the base host (github.com). The sandbox network policy denies LAN and host services and
  leaves the public internet open (`crates/maxplayer-core/src/sandbox_net.rs:1 @b08fc1e8`).
- A seller version that serves contributions. Public contribution serving predates v0.6.0, and
  private serving arrived in v0.6.0 (first tag containing the private baseline route). Our seats run
  0.7.x.
- A `node` capability for a Node product. Capability tokens are the closed list `node|python|rust`
  (`crates/maxplayer-core/src/capability.rs:36 @b08fc1e8`). They prove the probe binary existed at
  seat start, nothing more.

**Conclusion:** a typical swarm member already has everything. Joining a code run needs no new
setup, so opening it to members is a product decision, not a technical blocker.

### 1.3 (c) Where does the delivered branch land, and can the runner build and test it safely?

- **Where it lands.** For a public job, the commit is on `maxplayer/contribution/<job_id>` in the
  seller's relay-git repository. The buyer's collect fetches it into the buyer store `<home>/store`
  (`crates/maxplayer-core/src/collect.rs:571 @b08fc1e8`) under
  `refs/maxplayer/deliveries/<commit_oid>` (`crates/maxplayer-core/src/delivery_git.rs:568 @b08fc1e8`).
  It checks the files out into `<home>/results/<job_id>`
  (`crates/maxplayer-core/src/collect.rs:583 @b08fc1e8`) and returns `commit_oid` and `path`
  (`crates/maxplayer-core/src/collect.rs:101 @b08fc1e8`). The runner already calls `collect`
  (`scripts/challenges/swarm.mjs:1549 @b08fc1e8`) with the same home. **The runner can therefore read
  the exact commit from a local bare repository, with no network and no seller credentials.**
- **The security problem.** Testing means executing seller-written code. That must never happen on the
  runner host. The repository's sandbox is the seller image
  `ghcr.io/makeprisms/maxplayer-sandbox:v<version>`
  (`crates/maxplayer-core/src/seller_exec.rs:260 @b08fc1e8`), built `FROM node:22…`
  (`docker/maxplayer-sandbox/Dockerfile:69 @b08fc1e8`), and run under gVisor `runsc` where configured
  (`crates/maxplayer-core/src/home.rs:680 @b08fc1e8`).
- **Options:**
  1. **Local docker on the runner host** (chosen): a throwaway container with `--network none`, a
     read-only mount of an exported tree, no credentials, and resource limits (§4.4).
  2. **A verification job on a swarm seat:** reuses the seller sandbox, but its verdict is seller
     testimony. That is fine for our own seats but not for members, and it adds a job round trip per
     merge. Left out (§8).
  3. **Bare host execution:** refused.
- Code mode **refuses to start** when docker is unusable. It never falls back to the host.

### 1.4 (d) Job duration bounds versus coding tasks

- **Runner:** `durationSeconds` must be within `[min, max]`
  (`scripts/challenges/swarm.mjs:1831 @b08fc1e8`). The defaults are 300 s
  (`scripts/challenges/swarm.mjs:522 @b08fc1e8`) and 1800 s, set by `SWARM_MAX_DURATION`
  (`scripts/challenges/swarm.mjs:1976 @b08fc1e8`). The deadline must also fall before the run window
  ends (`scripts/challenges/swarm.mjs:1420 @b08fc1e8`).
- **Seller:** the job deadline is the offer deadline, unless the seat sets `job_timeout_secs`. That
  override wins even when it is shorter (`crates/maxplayer-core/src/seller.rs:145 @b08fc1e8`). The 600 s
  default applies only when the offer deadline has already passed
  (`crates/maxplayer-core/src/seller.rs:20 @b08fc1e8`). The agent timeout is the remaining deadline
  (`crates/maxplayer-core/src/seller_exec.rs:2253 @b08fc1e8`). The container push token caps at 6 h
  (`crates/maxplayer-core/src/seller_exec.rs:313 @b08fc1e8`), well above a 3 h window.
- **Fit:** a scoped task (one module plus its tests) takes an agent about 10–25 minutes, plus clone and
  container setup. 1800 s fits if tasks stay small. **v1 keeps the defaults** and tells the planner to
  size tasks to about 20 minutes. The join note asks seats not to set `job_timeout_secs` below 1800.

---

## 2. Problem and goals

The swarm can split a question, but it cannot build anything. Every job starts in a fresh empty
container (`crates/maxplayer-core/src/delivery_orchestrator.rs:248 @b08fc1e8`). Earlier work reaches a
later job only as quoted answer text (`includeAnswers`, `scripts/challenges/swarm.mjs:225 @b08fc1e8`).
A delivery is flattened to capped text (`scripts/challenges/swarm.mjs:52 @b08fc1e8`). A run ends when
the planner returns `solved` with a result (`scripts/challenges/swarm.mjs:1792 @b08fc1e8`). Its check is
the planner's own judgement (`scripts/challenges/planner-llm.mjs:84 @b08fc1e8`), not a test the runner
runs.

**Goals for v1:**

1. Work accumulates in a git repository. Each job starts from the current head, not from an empty
   directory.
2. "Done" is mechanical: the acceptance tests pass on the repository head, as run by the runner in a
   sandbox.
3. Seller code never executes outside a sandbox on the runner host.
4. A crash or restart never merges twice and never posts against a stale base.
5. Budget, window, roster, admission and payment behavior stay unchanged.

**What "finished product" means:** at the end of the run, the repository head passes every acceptance
test in the sandbox. The head is tagged `swarm/<run-id>/done`, and the README states the run command
(for the pilot, `node bin/tallycsv.js --help`). The tagged tree is the build artifact. A zero-dependency
Node app needs no separate build output. The run's final update names the repository, tag, commit
and run command.

## 3. The product repository

- **Owner:** the runner operator (MakePrisms for the pilot). The runner's buyer pubkey is the
  contribution `target_repo_owner` pin.
- **Host (default):** a **public GitHub repository**, e.g. `MakePrisms/swarm-tallycsv`. The reason is
  §1.1: a relay-git base fails at the seller's credential-free container clone. Its public read also
  passes the buyer's post-time anonymous probe.
- **How the spec and tests get in:** before the run, the operator commits `SPEC.md` (the app spec), the
  skeleton (`package.json` with no dependencies, an empty `src/`, `bin/`), and `acceptance/*.test.js`.
  The acceptance suite defines done. The run's goal text links `SPEC.md`.
- **Protected paths:** `acceptance/`, `package.json`, `SPEC.md` and `.github/` are never assigned to a
  seller. A delivery that touches them is rejected (§4.3).
- **How the base moves:** only the runner pushes, and only fast-forward pushes of a merge it tested
  (§4.5). It authenticates with a credential scoped to that one repository, held in the runner's
  environment, and never passed to a sandbox or a seller. Nothing in core moves a contribution target
  forward after accept, so this is runner work.
- **Content limits:** text files only. The buyer's delivery review permanently refuses binary files,
  symlinks and submodules (`crates/maxplayer-core/src/review.rs:324 @b08fc1e8`). The runner already
  treats that as an unusable, paid delivery (`scripts/challenges/swarm.mjs:839 @b08fc1e8`).

## 4. Runner changes

### 4.1 Code mode in the challenge config

Code mode is enabled by `SWARM_CODE_MODE=1`, default off. With the flag off, nothing changes. With it
on, four more settings are required, and startup fails if any is missing:

| Setting | Meaning |
|---|---|
| `SWARM_CODE_REPO` | https clone URL of the product repository (public) |
| `SWARM_CODE_BRANCH` | branch to build on (default `main`) |
| `SWARM_CODE_PUSH_CREDENTIAL_FILE` | file holding the repo-scoped push credential (mode 0600) |
| `SWARM_CODE_IMAGE` | test image pinned by digest (default: the `node:22` digest the sandbox image uses) |

Startup checks: `docker info` succeeds, a smoke container runs `node --version` with
`--network none`, `git ls-remote` reaches the repository, and the branch head is reachable. Any
failure stops startup. There is no host fallback.

### 4.2 Posting contribution jobs against the current head

Each assignment is still posted as a public targeted offer (§1.0). The args change from
`scripts/challenges/swarm.mjs:1857 @b08fc1e8` as follows:

- `output: 'git'` and `accepts_delivery: []`. A contribution cannot be inline (§1.1).
- All four contribution pins, which are all-or-nothing
  (`crates/maxplayer-core/src/buyer/mod.rs:602 @b08fc1e8`): `target_repo_owner` = buyer pubkey,
  `target_repo_url` = `SWARM_CODE_REPO`, `base_branch`, and `base_oid` = **the journaled head**.
  `accepts` stays the default `["fork"]` (`crates/maxplayer-core/src/job_lifecycle.rs:1211 @b08fc1e8`).
- `capabilities: ['node']`. The planner may only assign sellers whose snapshot advertises it.
- The `[swarm:<buyer>:<id>]` task prefix stays as is, so the board sees the job.
- **Stale-base guard:** immediately before posting, the runner compares `git ls-remote` with
  `day.code.head`. If they differ, or a merge intent is open (§4.6), it defers the post. It never posts
  with a `base_oid` it has not journaled as head.

Several jobs may run at once against the same head. Merges are serialized (§4.5).

### 4.3 Collect, then check the delivery

After a successful `collect`, the runner takes `commit_oid` and fetches
`refs/maxplayer/deliveries/<oid>` from `<home>/store` into its own working repository
(`<home>/swarm/code.git`). It only reads the buyer store. It then runs, in order, as plain git
operations with hooks disabled (`core.hooksPath=/dev/null`) and no checkout filters:

1. **Shape:** the commit has exactly one parent, equal to the job's journaled `base_oid`. The runner
   checks this itself because a free public collect does not (§1.1).
2. **Ownership:** every changed path matches the job's `files` globs and none is protected (§3). The
   buyer-wide `ContributionPolicyConfig` (`crates/maxplayer-core/src/home.rs:1966 @b08fc1e8`) is global,
   not per job, so it cannot do this.
3. **Merge:** if `base_oid` is still head, the candidate is the commit itself. Otherwise the runner
   tries a three-way merge onto head. A conflict ends the check.

Any failure gives verdict `rejected` with reason `shape`, `scope` or `conflict`. The delivery is still
paid (the buyer daemon pays on delivery, `crates/maxplayer-core/src/buyer/mod.rs:2153 @b08fc1e8`;
settled input 3).

### 4.4 Sandboxed test step

The runner exports the candidate tree with `git archive` into a new runner-owned temp directory. It
then runs:

```text
docker run --rm --network none [--runtime runsc if registered] --cap-drop ALL \
  --security-opt no-new-privileges --read-only --tmpfs /tmp --memory 2g --cpus 2 \
  --pids-limit 256 -v <export>:/app:ro -w /app <SWARM_CODE_IMAGE> \
  node --test --test-reporter=tap acceptance/ test/
```

- The container has no network, credentials, home directory or writable mount except `/tmp`.
- A 300 s wall-clock limit kills the container. TAP output is read with a 64 KiB cap, and the temp
  directory is deleted afterwards.
- The runner parses TAP into a set of passing test names. It never executes anything from the tree
  itself.
- It journals the container id for every run. This makes "no seller code on the host" auditable.

**Merge gate (the ratchet).** Merge only if both hold:

- every test named in the job's `tests` passes; and
- no test that passes at head fails on the candidate.

Otherwise the verdict is `rejected`, reason `red` or `regression`, with up to 2 KiB of failing output.
A failed run is retried once, to absorb a flaky test, before rejection.

### 4.5 Merge, one at a time

One merge is in progress at most (`day.code.intent`). The steps:

1. Journal the intent `{job, from: head, to: candidateOid}` and fsync it. The journal is already one
   atomic fsynced record (`scripts/challenges/swarm.mjs:602 @b08fc1e8`).
2. Push `candidateOid:refs/heads/<branch>` **without force**. GitHub accepts it only as a fast-forward
   from `from`, which serves as the lease.
3. Journal `head = to`, append to `merges`, record the new passing set, clear the intent, and set the
   job's verdict to `merged`.
4. If every acceptance test passes, tag `swarm/<run>/done` and stop the run as solved with result
   `<repo>@<oid>`. In code mode the planner's `solved` is ignored. Only the runner declares done.

A push that is refused because the remote moved stops the runner with "product head moved outside
the runner". The operator resolves it. The runner never force-pushes.

### 4.6 Restart and crash safety

At startup in code mode, before any post, the runner compares `git ls-remote` with the journal:

| Journal | Remote head | Action |
|---|---|---|
| no intent | `= head` | resume |
| intent | `= intent.to` | the push landed: finish step 3, then resume |
| intent | `= intent.from` | the push did not land: clear the intent and re-run 4.3–4.5 for that job (deterministic) |
| anything else | — | stop with "product head moved outside the runner" |

A job whose verdict is already set is never merged again. The verdict is journaled before the intent
clears. A job collected but not yet checked is re-checked from the store. Posting waits until this
reconciliation finishes (§4.2).

### 4.7 Journal fields

- `jobs[i].code = {base, files, tests, commit, verdict: 'merged'|'rejected', reason, mergeOid,
  failing: [names], output: '<≤2 KiB>', container: '<id>'}`
- `days[k].code = {repo, branch, head, passing: [names], merges: [{job, oid, at}], intent, doneTag}`

### 4.8 What stays unchanged

Budget accounting (`scripts/challenges/swarm.mjs:826 @b08fc1e8`), admission and the work window
(`scripts/challenges/swarm.mjs:1420 @b08fc1e8`), seller busy tracking
(`scripts/challenges/swarm.mjs:1403 @b08fc1e8`), roster, payment and collect retry classes all stay as
they are. No new event kinds, tags or relay changes are needed: the contribution tags already exist on
the wire.

## 5. Planner changes for code

**Plan.** Workstreams keep `id`, `description`, `needs`, `estMinutes` and `dependsOn`
(`scripts/challenges/swarm.mjs:1002 @b08fc1e8`). `dependsOn` is validated as today
(`scripts/challenges/swarm.mjs:743 @b08fc1e8`). Code mode adds two required fields:

- `files`: globs the workstream owns.
- `tests`: test names or files that define its done.

The plan is rejected if two workstreams own overlapping globs, or if a glob covers a protected path.
`verification` (`scripts/challenges/swarm.mjs:730 @b08fc1e8`) becomes "the acceptance suite" and is
fixed by the runner.

**Planner input (added).** The planner also receives:

- the head oid;
- the passing and failing acceptance test names at head;
- the merge list;
- each rejected job with its reason and failing names, plus a 2 KiB output excerpt.

`includeAnswers` is not used, because the code is in the repository.

**Assignments.** The planner may only assign a workstream whose `dependsOn` are all merged. The runner
also refuses an assignment whose `files` overlap any in-flight job. This keeps parallel jobs from
conflicting.

**Task text the seller receives** (built by the runner; the planner writes only the middle):

```text
[swarm:<buyer>:<id>]
You are working in a git checkout of <repo> at commit <base>. Read SPEC.md first.
Task: <planner text>
You own only: <files>. Do not modify any other path (acceptance/, package.json and SPEC.md are protected).
Done when these tests pass: <tests>. Run `node --test` before you finish. Use no dependencies.
Leave your changes in the working tree; the seller delivers them as one commit.
```

The inline `DELIVERY_NOTE` (`scripts/challenges/swarm.mjs:124 @b08fc1e8`) is not appended in code mode,
because a contribution cannot settle inline. The 8 KiB task cap
(`scripts/challenges/swarm.mjs:187 @b08fc1e8`) still applies.

## 6. Seller and member requirements, /swarm and the join skill

- **Seats:** maxplayer 0.7.x, `contribution_enabled` on (the default), the `node` capability,
  egress to github.com, and no `job_timeout_secs` below 1800. Docker seats are preferred, since the
  agent then runs sandboxed on the seller side too. No GitHub account or git credentials are needed
  (§1.2).
- **/swarm:** no code change in v1. Jobs stay public and keep the task prefix, so the board shows
  them as today. The runner's progress updates carry "head <oid>, N/M acceptance tests passing,
  K merges". The goal text links the repository.
- **Join skill (`web/app/.well-known/skills/swarm-join/skill.md`):** add one short "Code runs"
  paragraph listing the seat requirements above. It ships with the implementation PR, not before.

## 7. Pilot plan

- **App:** `tallycsv`, a zero-dependency Node 22 CLI that does three things:
  - reads a CSV file (RFC 4180 quoting);
  - filters with `--where col=value`, then sorts, groups, and sums;
  - prints a table, JSON or CSV.

  Modules: `src/parse.js`, `src/query.js`, `src/format.js`, `bin/tallycsv.js`. The operator writes
  about 40 acceptance tests across those modules and the CLI, so expect about 6–10 jobs.
- **Seats:** 2–3 team seats with docker and `node`, named by the operator on the day. The October
  test fleet included `chef-flay` (claude, docker/runc) and `worker-argon` (gemini, docker/runsc). This
  is from team notes and is unverified at this anchor.
- **Runner:** one operator host with docker (and `runsc` if available), a 3 h window, the same budget
  as a normal run, and paid jobs at seat rates (open question 5).
- **Success criteria:**
  1. All acceptance tests pass on head within one 3 h window.
  2. Zero seller code runs outside a container: every test run has a journaled container id.
  3. One deliberate runner kill during a merge leads to no double merge and no stale post on restart.
- **Measured:** merges per hour (target ≥ 2), share of deliveries merged (target ≥ 50%), sats per
  merged change, median job time against `durationSeconds`, and reasons for rejection.
- **What we learn:** whether planners can split code by file ownership, whether 1800 s is enough,
  how often conflicts and regressions happen, and the cost per merged change. That is the input for
  the member-rollout decision.

## 8. Staged implementation and effort

There is **one implementation PR**, behind `SWARM_CODE_MODE` (default off). It contains the runner,
planner and docs changes. Bob's rule applies: it is not merged while a swarm day is live. The stages
below are review checkpoints inside that PR, plus the work around it.

| Stage | What | Failure tests it must cover | Effort |
|---|---|---|---|
| 0 | Pilot repo: `SPEC.md`, skeleton, acceptance tests, scoped push credential (operator work, not this repo) | acceptance suite fails on the empty skeleton and passes on a reference solution kept off-repo | 1 day — writing 40 good tests is the work |
| 1 | Code-mode config, startup checks, posting contribution jobs against the journaled head | flag off posts exactly today's args; missing setting or no docker refuses to start; head mismatch or open intent defers the post; pins equal the journal | 2 days — new args, but the post path exists |
| 2 | Collect, store fetch, shape/ownership/merge checks | two parents, wrong parent, protected path, path outside `files`, conflict each give the right rejection reason; the buyer store is never written | 2 days — plain git, many cases |
| 3 | Sandboxed test step and ratchet | no docker means no run; network is unavailable inside the container; timeout kills it; output cap holds; a regression is rejected; a flaky test passes on the one retry; a TAP parse error rejects the delivery | 2 days — docker plumbing plus TAP parsing |
| 4 | Merge, push, journal, restart reconciliation | each row of the §4.6 table, using a kill between intent and push and between push and journal; push refused when the remote moves; never a force push; a merged job is never re-merged | 2 days — the crash cases are the cost |
| 5 | Planner input/output, `planner-llm.mjs` code prompt, stub planner for tests | overlapping `files` rejected; dependency not merged rejected; planner `solved` ignored in code mode; run solves only when all acceptance tests pass | 1.5 days |
| 6 | E2E: extend `run-e2e.sh` with a base repo on the existing git-https fixture (`scripts/challenges/e2e.py:189 @b08fc1e8`) and the stub agent's file mode (`scripts/challenges/stub-acp-agent.mjs:50 @b08fc1e8`) | one merged job, one rejected job, restart mid-merge, final tag | 2 days — the stub must write into a cloned base |
| 7 | Join-skill paragraph, runner README section | docs only | 0.5 day |
| 8 | Rehearsal on one seat, then the pilot day and a write-up | the success criteria in §7 | 2 days |

**Total: about 15 working days** (13 for the PR including tests, 2 for rehearsal and pilot).

## 9. Deliberately left out (each addable later without a wire change)

- **Parallel merges and a merge queue.** v1 merges one at a time. Jobs still run in parallel.
- **Private targeted code jobs.** These need a run job list the board can read, because private
  offers carry no `i` tag (§1.0). Contribution itself works there (§1.1).
- **Session jobs** for fix-up turns. A rejected job becomes a new job.
- **Opening code runs to members.** Decided after the pilot.
- **Auto-deploy, hosting and releases** beyond the done tag.
- **Dependencies** (`npm install`) in the test sandbox. The pilot is zero-dependency, and the sandbox
  has no network.
- **Verification jobs on seats** instead of local docker.
- **Planner-written acceptance tests.** The operator writes them.
- **Review jobs** (a second seller reviewing a diff).
- **Relay-git hosting** of the product repository, and private repositories.
- **Non-Node stacks** (`python`, `rust` capabilities).
- **A code panel on /swarm.**

## 10. Self-review: flaws fixed and risks of settled decisions

**Flaws found and fixed in this design:**

- *Free public contributions are only tip-matched.* The runner's own shape check (§4.3) closes this
  for both payment modes.
- *A relay-git base breaks the container clone.* The default host is public GitHub (§3).
- *Two jobs editing the same file.* Ownership globs and the in-flight overlap refusal (§5) fix this.
- *Silent stale base.* The `ls-remote` guard before each post and the restart table (§4.2, §4.6) fix
  this.
- *Planner declaring done early.* In code mode, only the runner's test result ends the run (§4.5).

**Risks that come with settled decisions or defaults:**

- **Paid rejected work.** Unusable or rejected deliveries are paid (settled input 3). This is bounded
  by the unchanged run budget.
- **Sandbox escape.** The test container runs seller code, and `runc` shares the host kernel. Use
  `runsc` where registered. A kernel escape remains a residual risk on a `runc`-only host.
- **Push credential.** A leak lets someone write to one public repository. It is scoped to that
  repository, read only by the runner, and never mounted anywhere.
- **Public code.** Pilot code and tasks are public, like every swarm job today.
- **Seat timeouts.** A seat with a short `job_timeout_secs` cuts jobs silently (§1.4). This is
  handled by the join note only.
- **Flaky tests.** They can block merges. One retry only; the operator fixes the test.
- **Single relay.** No new failure mode: collect and posting depend on the relay exactly as today.
  GitHub becomes a second dependency for seller clones and runner pushes.

## 11. Open questions (each with a proposed default)

1. **Product repository host.** *Default: public GitHub under MakePrisms.* Relay-git needs a
   credentialed container clone first (§1.1).
2. **Who writes the acceptance tests.** *Default: the operator, before the run, as protected paths.*
   A planner-written suite could let the run grade itself.
3. **Sandbox for the test step.** *Default: local docker on the runner host, `--network none`, `runsc`
   when registered.* The alternative is verification jobs on our own seats (§8).
4. **Pilot app.** *Default: `tallycsv` (§7).* Any zero-dependency Node app with about 40 tests works.
5. **Paid or free pilot.** *Default: paid at seat rates.* A paid collect runs the full buyer
   contribution check, and it gives the sats-per-merge number. Free would rely on the runner check
   alone (§1.1).
6. **Merging when head moved.** *Default: three-way merge plus a test re-run (§4.3).* The alternative is
   to reject and re-post against the new head, which is simpler but costs a paid job per conflict.

## Appendix — verification at the anchor

- Tests run (devshell, `--features wallet`, `MAXPLAYER_CONTENT_PRIVATE_KEY` unset):
  `private_contribution_relay_only` 1/1 and `local_contribution_base` 1/1 passed.
- Read only, not executed: the seller node's contribution serving and delivery, the container clone
  path, the free public collect path, and relay-git read authentication.
- Not run: `run-e2e.sh`; anything with docker (absent on this VPS); a live relay or seller.
