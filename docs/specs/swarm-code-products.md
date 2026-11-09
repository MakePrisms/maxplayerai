# Swarm code products — a swarm run that ships a small app

**Status:** design spec. No code written. **Anchor commit: `9423d18bbc4c032f2e765e4bfd9f96787a5ab898`
(= `origin/main` at the time of writing, 9 Nov 2026).** The first draft was anchored at
`b08fc1e84473fb2f3bd4fd71dd0c655fb1080325` (2 Nov 2026). This revision re-anchors every citation from
`b08fc1e8` to `9423d18b`. Every line number below was re-derived by `git grep -n -F` at the new anchor
and is written `file:line @9423d18b`. A script printed each cited line at the anchor before publication.

**Scope.** Today a swarm run answers a question: every job is text, and the run ends when the planner
says a verified answer exists. This spec turns a run into a small software project. Sellers deliver
commits against a repository we own on our relay. The runner tests each commit in a sandbox, merges it
or hands it back to the planner, and the run ends when the acceptance tests pass on the repository head.

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
5. **Petar, #swarm-updates (channel 1564741523197796373), 9 Nov 2026 15:17 UTC:** "yeah let's use our
   relay", answering Bob's question there of 3 Nov 2026 01:17 UTC ("do we need github, can't we use the
   buzz relay's git feature?"). **The product repository lives on our relay's git (buzz relay-git on
   the launch relay), owned by the runner's buyer key. Not GitHub.** This closes the first draft's open
   question 1 (product repository host).

---

## 1. Load-bearing checks (done before writing the design)

### 1.0 A premise correction first: the swarm posts PUBLIC targeted jobs, not private ones

Every assignment is posted with `visibility:'public'` and a `seller_pubkey`
(`scripts/challenges/swarm.mjs:1857 @9423d18b`), and the local E2E asserts exactly that
(`scripts/challenges/e2e.py:119 @9423d18b`). The board and the runner snapshot only see a job through
the public `[swarm:<buyer>:<id>]` prefix in the offer's `i` tag, and they drop offers without a hex `p`
tag (`web/app/src/challenge/shared/model.mjs:84 @9423d18b`). A targeted private offer must not carry an
`i` tag at all (`crates/buzz/crates/maxplayer-private-protocol/src/wire.rs:548 @9423d18b`). A private
code job would therefore drop out of the board, the minutes meter and the seller busy tracking.

**Decision:** v1 posts **public targeted contribution jobs**, so the board, metering and budget keep
working unchanged.

### 1.1 (a) Does contribution mode work end to end, with a relay-git base?

**Private targeted contribution (the route the seller change reuses).** The docs call it "the
supported combination" (`docs/PRIVATE-JOBS.md:64 @9423d18b`). After posting, the job's repository is
the relay copy and nothing else (`docs/PRIVATE-JOBS.md:68 @9423d18b`).

- **Proven by running, at `9423d18b`** (repo devshell, cargo 1.98.1, `--features
  acp,gateway,git-delivery,wallet`, `MAXPLAYER_CONTENT_PRIVATE_KEY` and proxy variables unset):
  - `private_contribution_reads_only_the_relay_job_repo_after_posting`
    (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:398 @9423d18b`), 1/1. A real
    private targeted post from a local checkout; a **paid** collect clears every delivery gate and
    stops only at a dead mint, with zero spend
    (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:527 @9423d18b`); a **free**
    collect materializes the delivered files
    (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:606 @9423d18b`).
  - `local_base_post_uploads_only_pin_and_falls_back_without_leaking_or_bypassing_policy`
    (`crates/maxplayer-core/tests/local_contribution_base.rs:44 @9423d18b`), 1/1.
  - The seller's input-cache helpers: `private_input_baseline_is_not_agent_execution_and_full_ref_is_preserved`
    (`crates/maxplayer-core/src/private_content/tests.rs:1248 @9423d18b`) and
    `input_preflight_counts_orphan_objects_not_just_current_snapshot`, both pass.
- **What those tests do NOT prove.** The seller is simulated: `deliver()` is a plain git clone, commit
  and push (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:352 @9423d18b`), and the
  accept-bind is written directly (`crates/maxplayer-core/tests/private_contribution_relay_only.rs:516 @9423d18b`).
  No relay, seller node or agent runs.
- **Seller side (read only).** Execution requires the pre-claim input cache with an `input-baseline`
  ref (`crates/maxplayer-core/src/seller_node/privacy.rs:493 @9423d18b`). The container mounts that
  cache read-only (`crates/maxplayer-core/src/seller_node/run.rs:9943 @9423d18b`) and clones the base
  from it (`crates/maxplayer-core/src/seller_node/run.rs:9790 @9423d18b`,
  `crates/maxplayer-core/src/delivery_orchestrator.rs:235 @9423d18b`). The seller key never enters the
  container: the host fetches, the container only reads git objects.

**Public targeted contribution on a relay-git base (what v1 posts).** At the anchor it **fails on a
default docker seat**:

- The container clones the base with no credential
  (`crates/maxplayer-core/src/delivery_orchestrator.rs:245 @9423d18b`;
  `docs/SELLER-QUICKSTART.md:898 @9423d18b`). Container delivery is the default on a docker seat
  (`crates/maxplayer-core/src/home.rs:791 @9423d18b`).
- Relay-git requires NIP-98 for every read (`crates/maxplayer-core/src/seller_git.rs:209 @9423d18b`).
  `BUZZ_GIT_PUBLIC_READ` only lifts the NIP-43 membership check for read routes; the signed request is
  still verified (`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:127 @9423d18b`,
  `crates/buzz/crates/buzz-relay/src/api/git/transport.rs:306 @9423d18b`). It defaults off
  (`crates/buzz/crates/buzz-relay/src/config.rs:856 @9423d18b`) and `nix/relay-host.nix` does not set
  it.
- Only the host path passes the seller key (`crates/maxplayer-core/src/seller_node/run.rs:2745 @9423d18b`),
  so a host-path seat (no docker, or `container_delivery = false`) already reads a relay-git base today.
- The buyer's post-time check waves relay-host repositories through as member-readable
  (`crates/maxplayer-core/src/job_lifecycle.rs:910 @9423d18b`), so the post succeeds and a docker
  seller then fails at clone. The job expires unpaid and burns the seat's window.

This is read from source; docker is absent on this VPS, so the failing clone was not reproduced.

#### 1.1.1 Can the private host-side base fetch be reused for public targeted jobs? Yes.

**What gates it to private jobs today (read).** Three places, all keyed on "is this a private offer":

1. The pre-claim staging runs only when the offer parsed as private:
   `private` is `Some` only if `is_private(&draft)` (`crates/maxplayer-core/src/seller_node/run.rs:7490 @9423d18b`,
   `crates/maxplayer-core/src/seller_node/privacy.rs:5 @9423d18b`), and the off-loop gate fires only
   for that `private` value (`crates/maxplayer-core/src/seller_node/run.rs:7514 @9423d18b`).
2. `preflight` itself needs the private content context and runs `validate_private` on the offer
   (`crates/maxplayer-core/src/seller_node/privacy.rs:268 @9423d18b`,
   `crates/maxplayer-core/src/seller_node/privacy.rs:295 @9423d18b`), which a public offer fails.
3. `execution`, which hands the cached baseline to both execution paths, looks the job up in the
   private content store only (`crates/maxplayer-core/src/seller_node/privacy.rs:476 @9423d18b`,
   `known` at `crates/maxplayer-core/src/seller_node/privacy.rs:16 @9423d18b`) and returns `None` for
   a public job. So the host path takes the baseline only for private jobs
   (`crates/maxplayer-core/src/seller_node/run.rs:2723 @9423d18b`), and the container path plans its
   base from the stored public pin (`crates/maxplayer-core/src/seller_node/run.rs:9762 @9423d18b`)
   and overrides it with the cache, and mounts the cache, only for a private baseline
   (`crates/maxplayer-core/src/seller_node/run.rs:9789 @9423d18b`,
   `crates/maxplayer-core/src/seller_node/run.rs:9943 @9423d18b`).

**What already fits a public relay-git base (read).** Nothing in the fetch itself is private:

- The signing rule already covers a base on the configured relay host: the seller signs the base
  fetch when the pinned clone URL starts with the seat's `[privacy] git_base`
  (`crates/maxplayer-core/src/seller_node/privacy.rs:323 @9423d18b`), which defaults to the launch
  relay (`crates/maxplayer-core/src/home.rs:1750 @9423d18b`, `crates/maxplayer-core/src/home.rs:188 @9423d18b`).
- The fetch is the bounded seller-input fetch (`crates/maxplayer-core/src/seller_node/privacy.rs:364 @9423d18b`),
  which only ever sends the header to a relay-git locator
  (`crates/maxplayer-core/src/git_transport.rs:2674 @9423d18b`,
  `crates/maxplayer-core/src/delivery_transport.rs:116 @9423d18b`). Proven by running:
  `bounded_fetch_sends_no_header_off_relay_git` (`crates/maxplayer-core/tests/private_repo_transport.rs:190 @9423d18b`)
  passes, as do the other 8 tests in that binary.
- With no attachments, the baseline is the base commit itself, not a synthetic commit
  (`crates/maxplayer-core/src/seller_node/privacy.rs:434 @9423d18b`), stored create-only under
  `refs/heads/input-baseline` (`crates/maxplayer-core/src/seller_node/privacy.rs:456 @9423d18b`) in
  `<home>/private-inputs/<offer id>` (`crates/maxplayer-core/src/seller_node/privacy.rs:258 @9423d18b`).
  So the delivery still descends from `base_oid`, which is what the buyer's descendant check needs.
- The container side imports objects from the mounted cache and checks out the pinned oid
  (`crates/maxplayer-core/src/seller_git.rs:2697 @9423d18b`).

**The smallest seller change.** Add a public twin of the staging step and let the two execution paths
look it up:

- In `on_offer`, for a **public contribution offer targeted to this seller** whose pinned clone URL is
  on the seat's own `git_base` host and is not a reserved 64-hex private job route, run a new
  `public_base_preflight` through the same off-loop `private_preflight` gate. It signs the base URL
  with the seat's signer, fetches `base_oid` with `fetch_bounded_objects` into the same
  `private-inputs/<offer id>` cache, runs the same object checks, and writes `input-baseline =
  base_oid` create-only. A failure is "inputs unavailable": the seller does not claim.
- A `public_baseline(home, job_id)` lookup returns the cache and oid when the stored pin is on the
  relay host. The container path uses it exactly where it uses the private baseline (base =
  `PRIVATE_INPUT_CACHE_DIR`, read-only mount). The host path uses it too, so both paths start from
  the object verified before the claim.
- Every other public base (GitHub or any https host, or a `/git/` URL on a foreign host) takes
  today's path unchanged. The private path is not touched.

**Buyer side: no change needed (read; partly proven).**

- Post: the public pre-publication check refuses only the per-job private route
  (`crates/maxplayer-core/src/job_lifecycle.rs:903 @9423d18b`) and waves the rest of the relay host
  through (`crates/maxplayer-core/src/job_lifecycle.rs:910 @9423d18b`). It then fetches the pinned
  base once, signed with the buyer key for the relay host
  (`crates/maxplayer-core/src/job_lifecycle.rs:950 @9423d18b`). A product repo named
  `swarm-<run>` is not a 64-hex job route, so it passes. Proven by running: the refusal of a
  per-job route (`post_job_refuses_seller_unreadable_private_base_for_open_pool_and_public`) and the
  base-validation error mapping (`public_base_validation_maps_auth_to_the_exception_and_checks_quota_and_seed`).
  Not run: a post against a real relay with a non-job relay-git base.
- Collect: a paid collect signs relay-git reads with the buyer secret
  (`crates/maxplayer-core/src/authorize_pay.rs:436 @9423d18b`) and fetches the base from the public
  pin (`crates/maxplayer-core/src/authorize_pay.rs:971 @9423d18b`); the buyer owns the repo, so it can
  read it. A free collect only tip-matches the seller's fork (below), as it does today.

**Are sellers relay members? Yes, on the launch relay (read).** Writes are members-only
(`nix/relay-host.nix:69 @9423d18b`); a seller that can publish a claim is a member. NIP-OA owner
delegation defaults off (`crates/buzz/crates/buzz-relay/src/config.rs:849 @9423d18b`). Git reads
require membership too, because public read is off (`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:306 @9423d18b`),
and a non-member gets "restricted: not a relay member"
(`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:319 @9423d18b`).

**Who can read a relay-git product repo? Any relay member, not only the owner and the p-tagged
seller.** The read gate resolves the repo's kind-30617 announcement; Maxplayer announces repos without
a `buzz-channel` tag, and an unbound repo is readable by every caller the auth step admitted
(`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:755 @9423d18b`). Anonymous readers cannot
clone it: `openRead` (`nix/relay-host.nix:62 @9423d18b`) opens event reads, not git. Proven by running
(buzz-relay Postgres tests against a scratch database on the test Postgres, 37/37 passed):
`read_gate_allows_unbound_repo_for_every_caller`
(`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:3799 @9423d18b`) and
`read_gate_allows_current_member_denies_removed_and_owner_bypass`. **This is acceptable for v1**: swarm
tasks are already public, every swarm seat is a member, and a member-only read is narrower than the
public GitHub repo of the first draft. Showing the finished code to non-members is left out (§9).

**Can the runner's buyer key create and push the repo? Yes (read).**

- Create: publish a kind-30617 announcement as the buyer key, as sellers already do for their
  delivery repo (`crates/maxplayer-core/src/profile.rs:951 @9423d18b`; announce before any push,
  `crates/maxplayer/src/sell.rs:216 @9423d18b`), then push the first commit. Repo names are
  `[a-zA-Z0-9._-]{1,64}` (`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:567 @9423d18b`).
- Push: push requires membership (`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:107 @9423d18b`).
  The announcement author gets the owner role (`crates/buzz/crates/buzz-relay/src/api/git/policy.rs:445 @9423d18b`);
  any other pusher on an unbound repo is refused (`crates/buzz/crates/buzz-relay/src/api/git/policy.rs:474 @9423d18b`).
  So only the runner's key can move the product branch, and sellers can only push to their own repos.
- Force push: the default rules let the owner force-push
  (`crates/buzz/crates/buzz-core/src/git_perms.rs:449 @9423d18b`). The announcement therefore carries
  `["buzz-protect","refs/heads/main","no-force-push","no-delete"]`, which applies to every role
  (`crates/buzz/crates/buzz-core/src/git_perms.rs:586 @9423d18b`,
  `crates/buzz/crates/buzz-core/src/git_perms.rs:366 @9423d18b`). Proven by running only for the
  neighbouring cases: `push_gate_denies_owner_through_broken_binding` and the ref-scope tests pass.
  The owner's successful push to an unbound repo was not run.
- How the base advances: the runner fast-forward-pushes a tested merge (§4.5) and posts the next job
  with `base_oid` = the new head. Sellers fetch the base by commit id, which the relay serves because
  upload-pack allows reachable sha1 wants
  (`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:607 @9423d18b`).
- Tooling gap: core has a signed push (`crates/maxplayer-core/src/git_transport.rs:647 @9423d18b`)
  and `ls_remote` (`crates/maxplayer-core/src/git_transport.rs:1121 @9423d18b`), but the challenges
  sidecar builds `maxplayer-core` with `gateway` only (`crates/maxplayer-challenges/Cargo.toml:17 @9423d18b`).
  The runner PR adds three sidecar subcommands with the `git-delivery` feature (§4.1).

**A FREE public contribution is only tip-matched at collect.** Accept still checks the seller's echoed
pins against the signed offer (`crates/maxplayer-core/src/job_lifecycle.rs:2430 @9423d18b`), but
`collect_free` runs the descendant/content check only with private evidence
(`crates/maxplayer-core/src/collect.rs:432 @9423d18b`) and skips the sentinel for contributions
(`crates/maxplayer-core/src/collect.rs:469 @9423d18b`). A PAID collect runs the full
`verify_contribution` (`crates/maxplayer-core/src/authorize_pay.rs:462 @9423d18b`). **So the runner
checks ancestry and shape itself, whatever the payment mode (§4.3).**

Other public contribution facts (read): the seller checks the base out onto
`maxplayer/contribution/<job_id>` (`crates/maxplayer-core/src/seller_node/run.rs:2693 @9423d18b`), signs
the authorship tuple into the result (`crates/maxplayer-core/src/seller_node/run.rs:2834 @9423d18b`), a
contribution never settles inline (`crates/maxplayer-core/src/job_lifecycle.rs:1887 @9423d18b`;
`docs/protocol-v1.md:700 @9423d18b`), and it is exactly one commit parented on the base
(`docs/protocol-v1.md:918 @9423d18b`).

**Not run:** no E2E drives a real seller node plus a real relay through a contribution job; the swarm
E2E's relay has a dead S3 endpoint, so relay-git does not work there
(`scripts/challenges/e2e.py:164 @9423d18b`), and CI does not run the E2E
(`.github/workflows/ci.yml:323 @9423d18b`). Nothing with docker ran.

### 1.2 (b) What git access does a seller need?

**A relay member key and the seller release with the change in §8 stage S.** Delivery defaults to the
seller's own repo on the hosted relay-git (`docs/SELLER-QUICKSTART.md:1128 @9423d18b`), with NIP-98
signed in-process; no external `git`, helper or `git-credential-nostr`
(`docs/SELLER-QUICKSTART.md:1141 @9423d18b`). The base fetch adds no new secret: the host signs it with
the same key, and the container never sees it. **No GitHub account or credential is needed.**

What a member seat needs:

- The release with the seller change. An older docker seat claims the job and then fails at clone
  (§1.1). An older host-path seat would work, but v1 does not rely on that.
- `contribution_enabled` left at its default, on (`crates/maxplayer-core/src/home.rs:1493 @9423d18b`).
  A seat that turned it off logs "contribution offers disabled" and never claims
  (`crates/maxplayer-core/src/seller_node/run.rs:7571 @9423d18b`).
- Membership of the launch relay (every claiming seller already is).
- A `node` capability for a Node product. Tokens are the closed list `node|python|rust`
  (`crates/maxplayer-core/src/capability.rs:36 @9423d18b`) and prove only that the probe binary
  existed at seat start.

The container needs no egress to any git host for the base any more; it still reaches the relay to
push its delivery, as today. The sandbox network policy leaves the public internet open
(`crates/maxplayer-core/src/sandbox_net.rs:1 @9423d18b`).

### 1.3 (c) Where does the delivered branch land, and can the runner build and test it safely?

- **Where it lands.** The commit is on `maxplayer/contribution/<job_id>` in the seller's relay-git
  repo. The buyer's collect fetches it into the buyer store `<home>/store`
  (`crates/maxplayer-core/src/collect.rs:571 @9423d18b`) under `refs/maxplayer/deliveries/<commit_oid>`
  (`crates/maxplayer-core/src/delivery_git.rs:568 @9423d18b`), checks the files out into
  `<home>/results/<job_id>` (`crates/maxplayer-core/src/collect.rs:583 @9423d18b`) and returns
  `commit_oid` and `path` (`crates/maxplayer-core/src/collect.rs:101 @9423d18b`). The runner already
  calls `collect` (`scripts/challenges/swarm.mjs:1549 @9423d18b`) with the same home. **The runner can
  read the exact commit from a local bare repository, with no network and no seller credentials.**
- **The security problem.** Testing means executing seller-written code, which must never happen on
  the runner host. The repository's sandbox is the seller image
  `ghcr.io/makeprisms/maxplayer-sandbox:v<version>` (`crates/maxplayer-core/src/seller_exec.rs:260 @9423d18b`),
  built `FROM node:22…` (`docker/maxplayer-sandbox/Dockerfile:69 @9423d18b`), run under gVisor `runsc`
  where configured (`crates/maxplayer-core/src/home.rs:680 @9423d18b`).
- **Options:**
  1. **Local docker on the runner host** (chosen): a throwaway container with `--network none`, a
     read-only mount of an exported tree, no credentials, and resource limits (§4.4).
  2. **A verification job on a swarm seat:** its verdict is seller testimony, and it adds a job round
     trip per merge. Left out (§9).
  3. **Bare host execution:** refused.
- Code mode **refuses to start** when docker is unusable. It never falls back to the host.

### 1.4 (d) Job duration bounds versus coding tasks

- **Runner:** `durationSeconds` must be within `[min, max]` (`scripts/challenges/swarm.mjs:1831 @9423d18b`).
  The defaults are 300 s (`scripts/challenges/swarm.mjs:522 @9423d18b`) and 1800 s, set by
  `SWARM_MAX_DURATION` (`scripts/challenges/swarm.mjs:1976 @9423d18b`). The deadline must also fall
  before the run window ends (`scripts/challenges/swarm.mjs:1420 @9423d18b`).
- **Seller:** the job deadline is the offer deadline unless the seat sets `job_timeout_secs`, which
  wins even when shorter (`crates/maxplayer-core/src/seller.rs:145 @9423d18b`). The 600 s default
  applies only when the offer deadline has passed (`crates/maxplayer-core/src/seller.rs:20 @9423d18b`).
  The agent timeout is the remaining deadline (`crates/maxplayer-core/src/seller_exec.rs:2257 @9423d18b`).
  The container push token caps at 6 h (`crates/maxplayer-core/src/seller_exec.rs:313 @9423d18b`).
- **Fit:** a scoped task takes an agent about 10–25 minutes plus setup. The base fetch now happens
  before the claim, so it does not eat into the job deadline. **v1 keeps the defaults** and tells the
  planner to size tasks to about 20 minutes. The join note asks seats not to set `job_timeout_secs`
  below 1800.

---

## 2. Problem and goals

The swarm can split a question, but it cannot build anything. Every job starts in a fresh empty
container (`crates/maxplayer-core/src/delivery_orchestrator.rs:248 @9423d18b`). Earlier work reaches a
later job only as quoted answer text (`includeAnswers`, `scripts/challenges/swarm.mjs:225 @9423d18b`).
A delivery is flattened to capped text (`scripts/challenges/swarm.mjs:52 @9423d18b`). A run ends when
the planner returns `solved` (`scripts/challenges/swarm.mjs:1792 @9423d18b`), on the planner's own
judgement (`scripts/challenges/planner-llm.mjs:84 @9423d18b`), not a test the runner runs.

**Goals for v1:**

1. Work accumulates in a git repository on our relay. Each job starts from the current head.
2. "Done" is mechanical: the acceptance tests pass on the repository head, run by the runner in a
   sandbox.
3. Seller code never executes outside a sandbox on the runner host, and no seller key enters a seller
   container.
4. A crash or restart never merges twice and never posts against a stale base.
5. Budget, window, roster, admission and payment behavior stay unchanged.

**What "finished product" means:** at the end of the run the repository head passes every acceptance
test in the sandbox. The head is tagged `swarm/<run-id>/done`, and the README states the run command
(for the pilot, `node bin/tallycsv.js --help`). The run's final update names the repository URL, tag,
commit and run command.

## 3. The product repository

- **Host and owner (settled input 5):** relay-git on the launch relay, owned by the runner's buyer
  key: `https://relay.maxplayer.ai/git/<buyer hex>/swarm-<run>.git`. The buyer pubkey is also the
  contribution `target_repo_owner` pin. The name must not be 64 hex characters: that namespace is
  reserved for private job repos (`crates/buzz/crates/buzz-relay/src/api/git/private_jobs.rs:46 @9423d18b`).
- **Creation:** the runner (or an operator command, §4.1) announces the repo with kind 30617 as the
  buyer key, with the `no-force-push`/`no-delete` protection on the build branch (§1.1.1), and pushes
  the first commit.
- **Readers:** any relay member (§1.1.1). Sellers read the base on their host with their own key.
- **How the spec and tests get in:** before the run, the operator commits `SPEC.md` (the app spec), the
  skeleton (`package.json` with no dependencies, an empty `src/`, `bin/`), and `acceptance/*.test.js`.
  The acceptance suite defines done. The run's goal text names the repo URL.
- **Protected paths:** `acceptance/`, `package.json` and `SPEC.md` are never assigned to a seller. A
  delivery that touches them is rejected (§4.3).
- **How the base moves:** only the runner's key can push (§1.1.1), and it only fast-forward-pushes a
  merge it tested (§4.5). Each push token is ref-scoped to the build branch
  (`crates/buzz/crates/buzz-relay/src/api/git/transport.rs:96 @9423d18b`). There is no separate push
  credential. Nothing in core moves a contribution target forward after accept, so this is runner
  work.
- **Size:** relay repos cap at 1 GiB compressed
  (`crates/buzz/crates/maxplayer-private-protocol/src/lib.rs:26 @9423d18b`). A pilot app is far below.
- **Content limits:** text files only. The buyer's delivery review permanently refuses binary files,
  symlinks and submodules (`crates/maxplayer-core/src/review.rs:324 @9423d18b`), which the runner
  already treats as an unusable, paid delivery (`scripts/challenges/swarm.mjs:839 @9423d18b`).

## 4. Runner changes

### 4.1 Code mode in the challenge config

Code mode is enabled by `SWARM_CODE_MODE=1`, default off. With the flag off, nothing changes. With it
on, these settings apply, and startup fails if a required one is missing:

| Setting | Meaning |
|---|---|
| `SWARM_CODE_REPO` | relay-git URL of the product repo; must be on the home's `git_base` host and owned by the buyer key (required) |
| `SWARM_CODE_BRANCH` | branch to build on (default `main`) |
| `SWARM_CODE_IMAGE` | test image pinned by digest (default: the `node:22` digest the sandbox image uses) |

Relay-git access goes through three new sidecar subcommands, built with `maxplayer-core`'s
`git-delivery` feature and signing NIP-98 with the home key the sidecar already reads
(`crates/maxplayer-challenges/src/cli.rs:314 @9423d18b`): `code-init` (announce with protection + push
the operator's skeleton), `code-head` (signed `ls-remote` of the branch) and `code-push` (non-force,
ref-scoped push of one commit from the runner's working repo). The key stays in the sidecar process.

Startup checks: `docker info` succeeds, a smoke container runs `node --version` with `--network none`,
`code-head` returns the branch head, and the repo owner in the URL equals the buyer pubkey. Any failure
stops startup. There is no host fallback.

### 4.2 Posting contribution jobs against the current head

Each assignment is still a public targeted offer (§1.0). The args change from
`scripts/challenges/swarm.mjs:1857 @9423d18b` as follows:

- `output: 'git'` and `accepts_delivery: []`. A contribution cannot be inline (§1.1).
- All four contribution pins, which are all-or-nothing
  (`crates/maxplayer-core/src/buyer/mod.rs:615 @9423d18b`): `target_repo_owner` = buyer pubkey,
  `target_repo_url` = `SWARM_CODE_REPO`, `base_branch`, and `base_oid` = **the journaled head**.
  `accepts` stays the default `["fork"]` (`crates/maxplayer-core/src/job_lifecycle.rs:1211 @9423d18b`).
- `capabilities: ['node']`, and only seats that advertise the seller change (§6, open question 6).
- The `[swarm:<buyer>:<id>]` task prefix stays as is, so the board sees the job.
- **Stale-base guard:** immediately before posting, the runner compares `code-head` with
  `day.code.head`. If they differ, or a merge intent is open (§4.6), it defers the post. It never posts
  with a `base_oid` it has not journaled as head.

Several jobs may run at once against the same head. Merges are serialized (§4.5).

### 4.3 Collect, then check the delivery

After a successful `collect`, the runner takes `commit_oid` and fetches
`refs/maxplayer/deliveries/<oid>` from `<home>/store` into its own working repo
(`<home>/swarm/code.git`). It only reads the buyer store. It then runs, as plain git operations with
hooks disabled (`core.hooksPath=/dev/null`) and no checkout filters:

1. **Shape:** exactly one parent, equal to the job's journaled `base_oid`. The runner checks this
   itself because a free public collect does not (§1.1.1).
2. **Ownership:** every changed path matches the job's `files` globs and none is protected (§3). The
   buyer-wide `ContributionPolicyConfig` (`crates/maxplayer-core/src/home.rs:1966 @9423d18b`) is global,
   not per job, so it cannot do this.
3. **Merge:** if `base_oid` is still head, the candidate is the commit itself. Otherwise the runner
   tries a three-way merge onto head. A conflict ends the check.

Any failure gives verdict `rejected` with reason `shape`, `scope` or `conflict`. The delivery is still
paid (the buyer daemon pays on delivery, `crates/maxplayer-core/src/buyer/mod.rs:2220 @9423d18b`;
settled input 3).

### 4.4 Sandboxed test step

The runner exports the candidate tree with `git archive` into a new runner-owned temp directory, then
runs:

```text
docker run --rm --network none [--runtime runsc if registered] --cap-drop ALL \
  --security-opt no-new-privileges --read-only --tmpfs /tmp --memory 2g --cpus 2 \
  --pids-limit 256 -v <export>:/app:ro -w /app <SWARM_CODE_IMAGE> \
  node --test --test-reporter=tap acceptance/ test/
```

- No network, credentials, home directory or writable mount except `/tmp`.
- A 300 s wall-clock limit kills the container. TAP output is read with a 64 KiB cap; the temp
  directory is deleted afterwards.
- The runner parses TAP into a set of passing test names. It never executes anything from the tree.
- It journals the container id for every run, so "no seller code on the host" is auditable.

**Merge gate (the ratchet).** Merge only if every test named in the job's `tests` passes and no test
that passes at head fails on the candidate. Otherwise the verdict is `rejected`, reason `red` or
`regression`, with up to 2 KiB of failing output. A failed run is retried once before rejection.

### 4.5 Merge, one at a time

One merge is in progress at most (`day.code.intent`):

1. Journal the intent `{job, from: head, to: candidateOid}` and fsync it. The journal is already one
   atomic fsynced record (`scripts/challenges/swarm.mjs:602 @9423d18b`).
2. `code-push` `candidateOid` to `refs/heads/<branch>` **without force**. The relay's pre-receive
   policy plus `no-force-push` accept it only as a fast-forward of `from`, which serves as the lease.
3. Journal `head = to`, append to `merges`, record the new passing set, clear the intent, and set the
   job's verdict to `merged`.
4. If every acceptance test passes, push the tag `swarm/<run>/done` and stop the run as solved with
   result `<repo>@<oid>`. In code mode the planner's `solved` is ignored.

A refused push because the remote moved stops the runner with "product head moved outside the runner".
The operator resolves it. The runner never force-pushes.

### 4.6 Restart and crash safety

At startup in code mode, before any post, the runner compares `code-head` with the journal:

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

Budget accounting (`scripts/challenges/swarm.mjs:826 @9423d18b`), admission and the work window
(`scripts/challenges/swarm.mjs:1420 @9423d18b`), seller busy tracking
(`scripts/challenges/swarm.mjs:1403 @9423d18b`), roster, payment and collect retry classes. No new event
kinds and no relay change: kind 30617 and the contribution tags are already admitted. The only wire
addition is the seller's heartbeat marker (open question 6), an extra tag on an existing kind.

## 5. Planner changes for code

**Plan.** Workstreams keep `id`, `description`, `needs`, `estMinutes` and `dependsOn`
(`scripts/challenges/swarm.mjs:1002 @9423d18b`). `dependsOn` is validated as today
(`scripts/challenges/swarm.mjs:743 @9423d18b`). Code mode adds two required fields:

- `files`: globs the workstream owns.
- `tests`: test names or files that define its done.

The plan is rejected if two workstreams own overlapping globs or a glob covers a protected path.
`verification` (`scripts/challenges/swarm.mjs:730 @9423d18b`) becomes "the acceptance suite", fixed by
the runner.

**Planner input (added):** the head oid; passing and failing acceptance test names at head; the merge
list; each rejected job with reason, failing names and a 2 KiB excerpt. `includeAnswers` is not used.

**Assignments.** Only a workstream whose `dependsOn` are all merged. The runner also refuses an
assignment whose `files` overlap any in-flight job.

**Task text the seller receives** (built by the runner; the planner writes only the middle):

```text
[swarm:<buyer>:<id>]
You are working in a git checkout of <repo> at commit <base>. Read SPEC.md first.
Task: <planner text>
You own only: <files>. Do not modify any other path (acceptance/, package.json and SPEC.md are protected).
Done when these tests pass: <tests>. Run `node --test` before you finish. Use no dependencies.
Leave your changes in the working tree; the seller delivers them as one commit.
```

The inline `DELIVERY_NOTE` (`scripts/challenges/swarm.mjs:124 @9423d18b`) is not appended in code mode.
The 8 KiB task cap (`scripts/challenges/swarm.mjs:187 @9423d18b`) still applies.

## 6. Seller and member requirements, /swarm and the join skill

- **Seats:** **the maxplayer release that contains the seller change (stage S). Members must upgrade
  to it before joining a code run.** Plus `contribution_enabled` on (the default), the `node`
  capability, relay membership, and no `job_timeout_secs` below 1800. Docker seats are preferred. No
  GitHub account or git credential (§1.2).
- **Enforcement:** the seller change adds a heartbeat marker (open question 6). The runner assigns
  code jobs only to seats that carry it, so an old seat is never sent a job it would fail. The
  runner snapshot's heartbeat tag list (`web/app/src/challenge/shared/model.mjs:154 @9423d18b`) gains
  the marker; the board shows nothing new.
- **/swarm:** no visible change in v1. Progress updates carry "head <oid>, N/M acceptance tests
  passing, K merges". The goal text names the repo URL.
- **Join skill (`web/app/.well-known/skills/swarm-join/skill.md`):** one short "Code runs" paragraph:
  minimum version, the requirements above, and "upgrade before the run". Ships with the runner PR.

## 7. Pilot plan

- **App:** `tallycsv`, a zero-dependency Node 22 CLI that reads a CSV file (RFC 4180 quoting), filters
  with `--where col=value`, sorts, groups and sums, and prints a table, JSON or CSV. Modules:
  `src/parse.js`, `src/query.js`, `src/format.js`, `bin/tallycsv.js`. About 40 operator-written
  acceptance tests; expect about 6–10 jobs.
- **Repo:** `https://relay.maxplayer.ai/git/<runner buyer hex>/swarm-tallycsv.git`, created with
  `code-init` from the operator's skeleton.
- **Seats:** 2–3 team seats with docker and `node`, **upgraded to the release with stage S** and
  showing its heartbeat marker, named by the operator on the day. The October test fleet included
  `chef-flay` (claude, docker/runc) and `worker-argon` (gemini, docker/runsc); from team notes,
  unverified at this anchor.
- **Runner:** one operator host with docker (and `runsc` if available), a 3 h window, the normal run
  budget, paid jobs at seat rates (open question 4).
- **Before the pilot day:** one rehearsal job on one seat against the live relay proves the whole
  path that no test covers: buyer post with a relay-git base, host-side base fetch, container clone
  from the mount, delivery, collect, runner merge and push.
- **Success criteria:**
  1. All acceptance tests pass on head within one 3 h window.
  2. Zero seller code runs outside a container: every test run has a journaled container id.
  3. One deliberate runner kill during a merge leads to no double merge and no stale post on restart.
- **Measured:** merges per hour (target ≥ 2), share of deliveries merged (target ≥ 50%), sats per merged
  change, median job time against `durationSeconds`, pre-claim base-fetch time, and rejection reasons.

## 8. Staged implementation and effort

Two PRs, in order. **Stage S is a separate seller PR plus a release, merged and released before the
runner PR.** The runner PR is behind `SWARM_CODE_MODE` (default off) and is not merged while a swarm
day is live.

**Stage S — seller PR: host-side fetch of a public relay-git base** (`maxplayer-core`, seller node).
Scope as in §1.1.1: `public_base_preflight` through the existing off-loop gate, `public_baseline`
lookup on the container and host paths, the heartbeat marker. Failure tests it must cover:

- **The key is never in the container:** the container launch for a public relay-git job carries
  exactly the read-only cache mount plus today's mounts, no key file and no auth env; the phase-1
  inputs carry the cache path as the base, not the relay URL (assert on the launch argv and the
  written inputs file, as the existing argv tests do).
- **Non-member refused:** the fixture server answers 403 "restricted: not a relay member"; staging
  fails, no claim is published, no cache ref is written, and a retry after the fixture allows it
  succeeds.
- **Wrong host refused:** a base on another host, or a `/git/` URL on a host other than `git_base`,
  never gets the seller's header (request log shows no `Authorization`) and never takes the new
  path; a 64-hex private job route on the relay host is refused.
- **Pinned object:** a base whose branch moved past `base_oid` still stages `base_oid`; a missing
  `base_oid` fails staging; a changed baseline on re-ingest is refused.
- **Existing private path unchanged:** the existing private suites
  (`private_contribution_relay_only`, `local_contribution_base`, `private_repo_transport`,
  `private_content::tests`) pass unmodified.
- **GitHub/https public path unchanged:** a public contribution on an https base outside `git_base`
  stages nothing, plans `ContributionClone` (`crates/maxplayer-core/src/seller_node/run.rs:2689 @9423d18b`)
  and clones in the container with no credential, as today.
- Open-pool public offers on a relay-git base stage nothing (only offers targeted to this seller).

Then cut a release and get member seats onto it (the operator's announcement names the version).

**Runner PR** (`scripts/challenges/*`, sidecar, planner, docs):

| Stage | What | Failure tests it must cover | Effort |
|---|---|---|---|
| S | Seller PR above, review, release | the Stage S list | 3 days — 1 code, 1.5 tests and fixture, 0.5 release and member upgrade notice |
| 0 | Pilot repo content: `SPEC.md`, skeleton, acceptance tests (operator work) | acceptance suite fails on the empty skeleton and passes on a reference solution kept off-repo | 1 day — writing 40 good tests is the work |
| 1 | Sidecar `code-init`/`code-head`/`code-push` with `git-delivery`; code-mode config, startup checks, posting against the journaled head, seat marker filter | flag off posts exactly today's args; missing setting, foreign-host repo, wrong owner or no docker refuses to start; head mismatch or open intent defers the post; pins equal the journal; the key never appears in sidecar output; push tokens are ref-scoped | 3 days — new signed git client plus the post path |
| 2 | Collect, store fetch, shape/ownership/merge checks | two parents, wrong parent, protected path, path outside `files`, conflict each give the right reason; the buyer store is never written | 2 days |
| 3 | Sandboxed test step and ratchet | no docker means no run; no network inside; timeout kills it; output cap holds; a regression is rejected; one flaky retry; a TAP parse error rejects | 2 days |
| 4 | Merge, push, journal, restart reconciliation | each row of the §4.6 table with kills between intent and push and between push and journal; a non-fast-forward push is refused by the relay; never a force push; a merged job is never re-merged | 2 days |
| 5 | Planner input/output, code prompt, stub planner | overlapping `files` rejected; unmerged dependency rejected; planner `solved` ignored; solves only when all acceptance tests pass | 1.5 days |
| 6 | E2E: `run-e2e.sh` gets a working relay-git (loopback MinIO instead of the dead S3 endpoint, `scripts/challenges/e2e.py:164 @9423d18b`), a product repo created by `code-init`, and the stub agent's file mode (`scripts/challenges/stub-acp-agent.mjs:50 @9423d18b`) on a host-path seat | one merged job, one rejected job, restart mid-merge, final tag | 3 days — relay-git in the harness is new, and the stub must write into a cloned base |
| 7 | Join-skill paragraph, runner README section | docs only | 0.5 day |
| 8 | Rehearsal on one upgraded docker seat against the live relay, then the pilot day and a write-up | the rehearsal path in §7 and the success criteria | 2 days |

**Total: about 20 working days** (was about 15). The extra 5 days are: stage S itself (3), a signed
relay-git client in the sidecar instead of a GitHub credential file (+1 on stage 1), and making
relay-git work inside the E2E harness (+1 on stage 6). The rough guess of +1–2 days covered only the
seller code; it left out the seller tests and release, the runner's git client, and the E2E relay
storage. The member-upgrade wait is calendar time on top, not work.

## 9. Deliberately left out (each addable later without a wire change)

- **GitHub or any external host for the product repo.** Settled input 5 chose the relay. A public
  https base still works on today's path (stage S keeps it unchanged), so an operator could point
  `target_repo_url` at one later; v1's runner refuses a foreign host.
- **Anonymous read of the finished product.** Only relay members can clone it. A public mirror or
  export after the done tag can be added later.
- **Open-pool code jobs on a relay-git base.** Stage S stages bases only for offers targeted to the
  seat, so bidders do not all fetch the repo.
- **Parallel merges and a merge queue.** Jobs run in parallel; merges are one at a time.
- **Private targeted code jobs.** These need a board-readable run job list, because private offers
  carry no `i` tag (§1.0).
- **Session jobs** for fix-up turns. A rejected job becomes a new job.
- **Opening code runs to members.** Decided after the pilot.
- **Auto-deploy, hosting and releases** beyond the done tag.
- **Dependencies** (`npm install`) in the test sandbox.
- **Verification jobs on seats** instead of local docker.
- **Planner-written acceptance tests.**
- **Review jobs** (a second seller reviewing a diff).
- **Non-Node stacks** (`python`, `rust` capabilities).
- **A code panel on /swarm.**
- **Cleaning the seller's `private-inputs` cache.** Nothing removes it today for private jobs either.

## 10. Self-review: flaws fixed and risks of settled decisions

**Flaws found and fixed in this design:**

- *A relay-git base fails at the credential-free container clone.* Stage S fetches it on the seller
  host with the seller key and mounts it read-only (§1.1.1).
- *Old seats would claim and then fail.* The heartbeat marker keeps code jobs off seats without stage S
  (§6).
- *The owner could force-push by default.* The announcement sets `no-force-push`/`no-delete` (§3).
- *Free public contributions are only tip-matched.* The runner's own shape check (§4.3).
- *Two jobs editing the same file.* Ownership globs and the in-flight overlap refusal (§5).
- *Silent stale base.* The `code-head` guard before each post and the restart table (§4.2, §4.6).
- *Planner declaring done early.* Only the runner's test result ends the run (§4.5).

**Risks that come with settled decisions or defaults:**

- **Single relay.** The relay is now the only dependency for the base, deliveries, posting and the
  product repo. A relay or S3 outage stops the run; nothing is lost, because the runner journal and the
  relay CAS keep the head consistent.
- **Buyer key signs git pushes.** The runner's money key now also signs ref-scoped push tokens, inside
  the sidecar that already reads it. A leaked token can only fast-forward the build branch until it
  expires.
- **Member-readable code.** Any relay member can read the product repo, and any buyer can point a
  targeted job at any unbound member repo, which the seller then fetches with its key. This is no
  wider than what a member can already clone; staging is bounded by the existing object and byte caps.
- **Member upgrade lag.** Code runs exclude seats that have not upgraded, which may shrink the roster.
- **Paid rejected work.** Bounded by the unchanged run budget (settled input 3).
- **Sandbox escape.** `runc` shares the host kernel; use `runsc` where registered.
- **Seat timeouts.** A short `job_timeout_secs` cuts jobs silently (§1.4); join note only.
- **Flaky tests.** One retry only; the operator fixes the test.
- **Seller disk.** Each code job leaves a small base cache in `private-inputs/` (§9).

## 11. Open questions (each with a proposed default)

1. **Who writes the acceptance tests.** *Default: the operator, before the run, as protected paths.*
   A planner-written suite could let the run grade itself.
2. **Sandbox for the test step.** *Default: local docker on the runner host, `--network none`, `runsc`
   when registered.* The alternative is verification jobs on our own seats (§9).
3. **Pilot app.** *Default: `tallycsv` (§7).* Any zero-dependency Node app with about 40 tests works.
4. **Paid or free pilot.** *Default: paid at seat rates.* A paid collect runs the full buyer
   contribution check and gives the sats-per-merge number. Free relies on the runner check alone.
5. **Merging when head moved.** *Default: three-way merge plus a test re-run (§4.3).* The alternative
   is to reject and re-post, which is simpler but costs a paid job per conflict.
6. **How the runner knows a seat has stage S.** *Default: stage S adds a heartbeat tag (e.g.
   `["contribution_base","relay-git"]`) and the runner assigns code jobs only to seats carrying it.*
   The alternative is a version check against the join note only, which lets an old docker seat claim
   and fail. The heartbeat's `v` tag is the protocol version, not the release
   (`crates/maxplayer-core/src/heartbeat.rs:654 @9423d18b`), so it cannot be used.

## Appendix — verification at the anchor

- **Run** (repo devshell, cargo 1.98.1, `MAXPLAYER_CONTENT_PRIVATE_KEY` and proxy variables unset):
  - `maxplayer-core --features acp,gateway,git-delivery,wallet`, focused lib tests: 12/12 passed
    (public base validation and post refusal, input preflight and baseline, `plan_delivery_workdir`,
    `init_contribution_workdir_refuses_ext_base`, phase-1 orchestrator, contribution pin store).
  - Integration: `private_contribution_relay_only` 1/1, `local_contribution_base` 1/1,
    `private_repo_transport` 9/9.
  - `buzz-relay` git gates with `--include-ignored` against a scratch Postgres database (migrations
    applied by hand first; the tests do not migrate): `sec005_postgres_tests` + `policy::postgres_tests`
    37/37, including the unbound-repo read gate, removed-member denial, ban cascade and ref-scope tests.
- **Read only, not executed:** the seller's private-only preflight gate and the proposed public twin;
  the container and host base paths; the buyer post with a non-job relay-git base; relay membership on
  the launch relay (`nix/relay-host.nix`); the owner's push to an unbound repo and `no-force-push`;
  `BUZZ_GIT_PUBLIC_READ` being off in production.
- **Not run:** `run-e2e.sh` (its relay-git has no working S3); anything with docker (absent on this
  VPS); a live relay or seller.
