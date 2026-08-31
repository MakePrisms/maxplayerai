# Container-side delivery + branch-scoped push tokens — implementation plan

> **For agentic workers:** implement task by task. Steps use checkbox (`- [ ]`) syntax.
> Write a failing test before each production change. Do not commit or push without the
> maintainer's explicit request.

**Goal:** Stop the seller host from ever operating on a git repository the job agent could
write. Move clone, commit, and push into the sandbox. Scope the push credential to one ref so a
leaked token is worthless.

**Why:** A confirmed exploit — the agent writes `.git/config` with a `url.<attacker>.insteadOf`
rule; a push from the agent's repo then reads that config and sends the seller's signed push token to
the attacker. Two defenses close it: rewriting the workdir's `.git/config` to a minimal redirect-free
file before the push (so it is never redirected in the first place), and the scoped token (Track A)
as a second layer if a token ever leaks. Test:
`crates/maxplayer-core/tests/hostile_local_git_config.rs` — PASSES (drives `neutralize_push_config`,
with a re-plant positive control). `hostile_symlink_checkout.rs` shows the checkout side is safe; keep
it as a regression guard.

**Server side — partly done.** PR #929 (`feat/relay-branch-scoped-push-tokens`) adds the relay's
scope ENFORCEMENT (Appendix A). The one-container design below needs ONE more relay change — a longer
token life for scoped tokens — briefed at
`docs/superpowers/briefs/2026-08-31-relay-scoped-token-lifetime.md`.

**Two tracks:**
- **Track A — branch-scoped token mint.** Small, fully specified, ships independently. DONE (#930).
- **Track B — all git in ONE container.** The scoped token makes it safe to hand the push credential
  to the container, so clone/agent/commit/push all move inside it and the host runs no git. A
  long-lived (scoped) token lets ONE container do the whole job — no two-phase split — at the cost of
  a small relay change. See the Track B section for the design, the review gate, and the open
  question (ACP retries).

---

## Implementation status (2026-08-31)

**Track A — DONE, PR #930.** `feat/branch-scoped-push-tokens`.

**Track B — design is now ONE container + a long-lived scoped token** (all git in the container; the
host parses no agent git data). Branch `feat/container-side-delivery`, stacked on Track A. Uncommitted.

- ✅ **The exploit is CLOSED on `main`** via PR #937 (merged, reviewer C1 addressed). Before the host
  push, the daemon rewrites the workdir's `.git/config` to a minimal redirect-free file
  (`seller_git::neutralize_push_config`) so a planted `insteadOf` cannot redirect it.
  `tests/hostile_local_git_config.rs` proves it, and it was verified live against the relay. This
  orchestrator REUSES that same function for the container push (no duplicate).
- Orchestrator core built + tested: `run_phase1` (clone → agent → gate + commit; no push),
  `push_delivery` + retry, the container entrypoint pieces (`run_phase1_entry`, `spawn_agent_child`,
  inputs, OID file), and the `maxplayer __deliver` CLI. 14 unit tests; full suite green.
- Remaining (client): merge to ONE container run (push in the same process; add relay url + token to
  the inputs), mint the token with a NIP-40 `expiration` (Task B8), host wiring (Task B2), sandbox
  image (Task B7). Behind a config switch until the review + relay land.
- Remaining (server): the relay must enforce the ref scope AND allow a longer life for scoped tokens —
  brief at `docs/superpowers/briefs/2026-08-31-relay-scoped-token-lifetime.md`.
- Gated on: the security review (gate relocation), the ACP-retry question, and the relay work deployed
  + confirmed on `buzzrelay.orveth.dev`.

**Tech stack:** Rust, Tokio, `nostr-sdk` 0.44, `git2`/libgit2, Docker.

---

## Global constraints

- Keep the seller key on the host. The key never enters the container.
- Keep every current path working when a job carries no scope and no container delivery.
- Never print, log, or commit a token or a key.
- Single-source the delivery branch. The token scope and the push refspec must read the same
  value, so they cannot drift.
- Ship Track A before, or with, the relay. An old relay ignores the tag, so Track A is inert
  until the relay enforces it. Track A never breaks an old relay.

---

# Track A — branch-scoped token mint (client)

The host mints the NIP-98 push token. Add one signed tag that names the single ref the token may
write. The relay in PR #929 reads that tag and refuses every other ref.

## Task A1: teach the mint to carry a ref scope

**Files:**
- Edit: `crates/maxplayer-core/src/git_transport.rs`

**Steps:**
- [ ] Add a parameter `ref_scope: Option<&str>` to `nip98_authorization_header_with_keys`
      (`git_transport.rs:222`).
- [ ] When `ref_scope` is `Some`, add one tag `["ref", "<refname>"]` to the `EventBuilder`
      before `sign_with_keys`. Confirm the `nostr-sdk` 0.44 tag API (`EventBuilder::tag` /
      `Tag::custom`).
- [ ] When `ref_scope` is `None`, emit the event exactly as today. No tag.
- [ ] Keep the `nip98_authorization_header` wrapper (`git_transport.rs:209`) passing `None`, so
      its callers are unchanged.
- [ ] Emit exactly one `ref` tag. The relay reads the first one (Appendix A).

## Task A2: thread the scope through the signer actor

**Files:**
- Edit: `crates/maxplayer-core/src/seller_node/signer.rs`

**Steps:**
- [ ] Add `ref_scope: Option<String>` to `Command::HttpAuthHeader` (`signer.rs:44`).
- [ ] Add the same parameter to the public `http_auth_header` fn (`signer.rs:182`).
- [ ] Pass it into the mint call in the actor arm (`signer.rs:260`).
- [ ] The key never leaves the actor. This task adds a value in, nothing out.

## Task A3: pass the delivery ref at the push call site

**Files:**
- Edit: `crates/maxplayer-core/src/seller_node/run.rs`

**Steps:**
- [ ] The delivery branch is `run.rs:6024`: `format!("maxplayer/{}", &job_id[..8.min(job_id.len())])`.
      ⚠ This is NOT the fork branch `maxplayer/contribution/<job_id>` at `run.rs:1885`. The
      token must name the PUSH branch.
- [ ] Compute the ref once, next to `branch`: `let push_ref = format!("refs/heads/{branch}");`.
- [ ] Pass `Some(push_ref)` into `http_auth_header` at `run.rs:6072`.
- [ ] Feed the same `push_ref`/`branch` value to the push refspec, so the scope and the push
      cannot diverge.
- [ ] The scope MUST be fully qualified — `refs/heads/…`. The relay rejects a bare branch name
      (Appendix A).

## Task A4: leave every other minter unscoped

**Steps:**
- [ ] The boot write-auth probe and any fetch-leg header stay `None`. They read a ref
      advertisement; they do not push a ref, so a scope there is wrong.
- [ ] Only the delivery push carries a scope.

## Task A5: tests

**Files:**
- Edit: `crates/maxplayer-core/src/git_transport.rs` (unit tests)
- Edit: `crates/maxplayer-core/src/seller_node/signer.rs` (actor test)

**Steps:**
- [ ] Mint with a scope: the event carries one `ref` tag with the exact value, and the event
      still verifies as valid NIP-98 (`u`, `method`, signature).
- [ ] Mint without a scope: byte-identical to today. Backward-compat guard.
- [ ] Signer: `http_auth_header(Some(ref))` puts the scope into the returned header.
- [ ] Drift guard: assert the ref in the minted token equals the branch the push uses. This is
      the test that stops a future edit from splitting the two.

**Verify Track A:**
```
cargo build -p maxplayer --release --no-default-features --features wallet,acp
cargo test -p maxplayer-core --features wallet
cargo test -p maxplayer --features acp,wallet
```

---

# Track B — all git in ONE container (long-lived scoped token)

**Goal.** The seller HOST runs no git. Clone, agent, gate + sentinel + commit, and push ALL happen
inside ONE sandbox container. The host mints a branch-scoped token up front, injects it, and reads
back only the commit OID (inert text). The host never parses the agent's git data — which is the
whole point: it defends against UNKNOWN exploits in git parsing of hostile data, not only the known
`insteadOf` redirect. The host holds the seller key and wallet, so a host compromise is catastrophic;
that is why "the host runs no git on agent data" is the bar. The interim host fix (rewrite the config,
then push on the host) is a stopgap that still runs a host git push, so it does not meet that bar — it
buys safety today and is removed when the container path ships.

**Why one container, not two.** The push token must be valid at push time — the END of a
minutes-long agent run — and only the host can mint it (the key never enters the container). Instead
of splitting into two containers so the host can mint a fresh 60 s token between them, mint ONE token
that lives long enough to cover the whole run. This is safe because the token is BRANCH-SCOPED: a
leaked scoped token can only push the seller's OWN delivery branch to the seller's OWN relay, and the
buyer fetches by commit OID — so a longer life adds no real risk. Worst case is griefing one dead
branch.

**This requires a relay change** (see "Server work — brief for the relay agent"): the relay must
(1) enforce the ref scope and (2) accept an OLDER token ONLY WHEN it carries a valid ref scope — an
unscoped token keeps the 60 s window. The long-lived token is safe ONLY once the relay actually
ENFORCES the scope; until then it would be a long-lived push-anywhere token. So this ships only after
the relay work lands and is confirmed deployed.

## The shape: one container, driven from inside

**Host — before launch.**
1. Resolve the delivery branch (deterministic from the job id).
2. Mint a branch-scoped NIP-98 token with a NIP-40 `expiration` = job deadline + a small push margin
   (the relay caps it). The seller key stays on the host.
3. Start the per-job credential proxy (as today — it holds the model key; the container reaches it at
   `host.docker.internal`).
4. Write the inputs (job_hash, base, delivery branch, the composed prompt / task, deadline, agent
   argv, the proxy endpoint, workdir, out dir, relay url, and the token) to a file readable only by
   the orchestrator.
5. Launch ONE container with our orchestrator as the entrypoint.

**Container — the orchestrator (our binary, the entrypoint).**
1. Read the inputs, then DELETE the file immediately — job_hash and the token then live only in
   memory (B-2).
2. Clone the buyer's base (public; the sandbox allows the internet) or init empty for from-scratch.
3. DRIVE the ACP session itself: compose the prompt, run the agent turn(s) against the workdir, and
   RETRY on a transient failure within the deadline. The agent runs as a child; its model calls go to
   the host credential proxy over the network (the model key never enters the container). This is the
   ACP driver relocated from the host — the host no longer drives ACP.
4. Gate + sentinel + commit (the existing `snapshot_delivery_at`). An empty tree is refused, so a
   quota-dead agent gets no sentinel.
5. Reap the agent's process group, REWRITE the workdir's `.git/config` to a minimal redirect-free file
   (`neutralize_push_config`), then push the delivery branch to the relay with the token. The rewrite
   means a planted `insteadOf`/`pushInsteadOf`/`include` cannot redirect the push, so neither the token
   nor the delivery pack leaks to a redirect host. Branch-scoping is the second layer.
6. Write the commit OID (and a usage summary) to the out files. Exit.

**Host — after.** Read the OID string. Sign + publish the kind-3403 naming it. Payment. NO git, NO
ACP, NO agent monitoring.

## Decisions — resolved

**Driver in the container — the retry coordination dissolves.** The orchestrator, not the host, drives
ACP and owns the retry loop (`run_agent_with_retry`, relocated). One process decides success, retries,
gates, commits, and pushes — so there is no host↔container "commit vs retry" signalling to design. The
model credential still never enters the container: the agent reaches the host proxy over the network
regardless of where the driver runs (`credential_proxy.rs`: "The proxy is a host process").

**Metering at the proxy.** Usage / budget is counted at the host credential proxy, which forwards
every model call and is the authoritative, tamper-proof meter — NOT self-reported by the container (a
compromised container could under-report to dodge the budget). The container may report usage for
observability; money decisions use the proxy's count.

**One container, long-lived scoped token.** No two-phase split, no mid-run token handoff. Simplest
runtime, clean restart. The cost is that a longer-lived bearer token exists — bounded to nearly
nothing by the scope.

**B-2 — the token stays away from the agent; `job_hash` is public, not a secret** (reviewer C5). The
orchestrator holds the scoped push token and never puts it in the agent's env, args, or a path the
agent reads — and it is worthless if it leaks anyway (scoped). `job_hash = sha256(job_id | task |
amount)` is derived from the PUBLIC offer, so the agent CAN compute it; keeping it out of the agent's
environment is hygiene, not a security boundary. What actually protects the sentinel is the gate (see
the review gate), not `job_hash` secrecy. No root and no uid split (the container runs
`--user <seller> --cap-drop ALL`). Note the "child cannot ptrace its parent" property holds only under
host Yama `ptrace_scope ≥ 1` — a host setting — so it is not load-bearing here; the token's safety is
the scope.

**The host parses no agent git data.** It mints (crypto), reads an OID string, signs a nostr event.
Every operation that touches the agent's tree or objects — clone, commit, push — is in the container.
This is the defense-in-depth the effort is for.

**Token lifetime = job deadline + margin, capped by the relay.** The seller sets a NIP-40
`expiration`; the relay honors it for scoped tokens up to a hard cap, so a client bug cannot mint a
year-long token. Unscoped tokens are unaffected (still ±60 s).

## Alternative, if we do NOT change the relay freshness

One container still works without touching the relay's 60 s rule: after the agent exits, the host
drops a FRESH 60 s token into the shared workdir (a file, not a network call — the volume is
bind-mounted), and the orchestrator picks it up and pushes. Keeps tokens short-lived; costs a
wait/poll inside the orchestrator and a messier restart. Fall back to this only if the relay freshness
change is rejected.

## Review gate — moving the gate + sentinel into the container

The completion gate + sentinel move into the container orchestrator. It is a fraud-check relocation
and needs maintainer + security sign-off. Here is the safety analysis for that sign-off.

**A malicious agent cannot forge a passing sentinel.** The gate (`snapshot_delivery_at`) does three
things in order: (1) it DELETES any sentinel file already in the workdir, (2) it checks the tree is
real (non-empty / actually differs from the base), (3) only then does it write a FRESH sentinel and
commit. So an agent that pre-writes a fake sentinel has it wiped, and the delivery is judged on the
tree it actually left. An agent that wrote only a fake sentinel and did no work → sentinel removed →
empty tree → refused.

**The protection is the gate, not `job_hash` secrecy** (reviewer C5). `job_hash` is PUBLIC (derived
from the offer), so the agent CAN construct a valid sentinel string — that is not what stops it. What
stops it is the gate's delete-and-rewrite (above): whatever the agent writes is wiped, and the delivery
is judged on the real tree. So forgery is defeated by the gate ALWAYS running (our binary, not the
agent), independent of what the agent knows. Do not let a future optimisation (e.g. skipping the wipe)
lean on the false premise that the agent cannot learn `job_hash`.

**Forging or suppressing does not help an attacker.** Forging a sentinel makes delivery SUCCEED — good
for the seller, so no adversary wants it. Suppressing it means the agent deleted its own work → the
delivery just fails → the buyer paid for a job whose agent sabotaged it, not an attack on the seller.
The agent cannot touch the orchestrator's logic: it is a separate child, and the orchestrator runs
from the read-only image, not from the agent-writable workdir.

**What actually changed vs. the host version.** Only WHERE the gate runs (host → container). The gate
is our binary and the agent is a separate child that cannot modify its execution (it runs from the
read-only image, not the agent-writable workdir). `job_hash` living in the container is immaterial — it
is public either way. So the guarantee is unchanged: the gate, run by our code, decides.

**The buyer's trust is unaffected.** The sentinel was never a defense against a malicious SELLER (who
controls the node, host or container, and could bypass the gate either way). The buyer's protection is
independently re-fetching the seller-signed commit and checking the sentinel matches the buyer-signed
task — that check is identical wherever the seller minted it.

Mitigations in one line: the orchestrator is OUR binary, the agent is a separate child that cannot
ptrace it, and the sentinel needs no key (it is `sha256(job_id | task | amount)`).

## Resolved — the orchestrator drives ACP and owns retries

This was the last open question ("who signals commit vs retry across the host↔container boundary").
The driver-in-container decision settles it: the orchestrator drives the ACP session, so it owns the
retry loop (`run_agent_with_retry`, relocated) AND commits in the same process — there is no
cross-boundary signal to design. What makes this safe to move is that the credential proxy stays on
the HOST (the model key never enters the container; the agent reaches the proxy over the network). See
Task B9 (relocate the driver) and Task B10 (meter at the proxy).

## Status of the code (branch `feat/container-side-delivery`)

Built + tested (BUILDING BLOCKS; some predate the driver-in-container decision and use a `spawn_agent`
seam where production will call the relocated ACP driver):
- `run_phase1` (clone → agent seam → gate + commit), `push_delivery` + retry, the container entrypoint
  pieces (`run_phase1_entry` / `run_phase2_entry`, `spawn_agent_child`, inputs, OID file), and the
  `maxplayer __deliver` CLI shim. 14 unit tests; full suite green.
- Interim config-rewrite fix wired live on this branch — the SCAFFOLD that closes the exploit. Marked as such
  in `run.rs`; removed LAST (below).

To reach THIS design (each item is a task below):
- [ ] Relocate the ACP driver into the orchestrator (Task B9) — the big one; the `spawn_agent` seam
      becomes a call to `run_agent_with_retry` against the workdir under a pass-through policy.
- [ ] Meter at the proxy (Task B10).
- [ ] Merge to ONE container run: clone + agent + gate + commit + push in one process; a single
      `maxplayer __deliver run` with the relay url + token in the inputs.
- [ ] Client: mint the scoped token with a NIP-40 `expiration` (Task B8).
- [ ] Host wiring (Task B2): launch the one container, inject inputs + token, read the OID, publish the
      kind-3403. Behind a config switch until the review + relay land.
- [ ] Add the binary to the sandbox image (Task B7).
- [ ] Remove the interim host fix LAST, once the container push is live AND the relay is
      confirmed enforcing the scope.

**Not verifiable in-tree.** The driver relocation, host wiring, image, and long-lived-token round trip
cannot be end-to-end tested without a container + a live ACP harness + the relay change. That is why
the security review is a DESIGN review that GATES the build: the reviewer signs off on this design plus
the security-critical code (exploit fix, gate/sentinel relocation, B-2, token model), and only then is
the untestable wiring built and validated against a real container + relay.

## Task B1 — container orchestrator entrypoint — DONE (needs the phase-merge above)

`run_phase1`, `run_phase1_entry`, `spawn_agent_child`, inputs + OID file, `maxplayer __deliver` CLI.
Tested. Merge the push into a single entry per "Status of the code".

## Task B2 — host wiring — BLOCKED (review + relay)

Launch the one container (orchestrator entrypoint; driver-in-container per B9) with inputs + token →
read the OID → publish the kind-3403 → pay. The host runs no git and no ACP. Behind a config switch so
production stays on the interim fix until the review + relay land.

## Task B3 — inputs in, OID out — DONE (extend with token + relay url)

`Phase1Inputs` and the OID file exist. Extend the single-run inputs with the relay url and the token.

## Task B4 — push retry — DONE

`PushRetryPolicy` + `push_with_retry`; jittered exponential backoff, bounded. Now bounded by the
token's expiration rather than a 60 s window.

## Task B5 — restart/resume

A crash re-runs the one container (re-clone + re-agent + re-commit + re-push). The delivery commit is
deterministic (same tree + `job_hash` ⇒ same OID), so a re-push is idempotent. To skip re-running a
completed agent, journal the OID and short-circuit to a push-only re-run.

## Task B6 — tests

- [ ] `hostile_local_git_config.rs` — keep as the guard for the interim host fix while it is
      live; when the container push replaces it, reframe to assert a leaked scoped token can push
      nothing but the seller's own delivery branch (worthless).
- [x] Gate refuses an empty tree in the orchestrator.
- [x] Sentinel binds to `job_hash`.
- [x] Push retry clears a simulated conflict and gives up after the bound.
- [ ] Inputs (incl. the token) deleted before the agent runs; token absent from the agent env/argv.
- [ ] Restart re-runs the one container idempotently.

## Task B7 — sandbox image carries the binary

Add a `FROM rust:1-bookworm` builder stage to `docker/maxplayer-sandbox/Dockerfile` (mirror the
top-level `Dockerfile`: `cargo build --release -p maxplayer --features acp,wallet` → COPY the binary),
switch the image build context to the repo root, and update
`.github/workflows/publish-sandbox-image.yml`. Additive; no ENTRYPOINT change (the host passes the
full command at `docker run`).

## Task B8 — client: long-lived scoped token

- [ ] Add a NIP-40 `expiration` tag to the scoped mint (`nip98_authorization_header_with_keys`), value
      = job deadline + push margin. Only when a ref scope is present.
- [ ] Thread `job_deadline_unix` to the mint call site.
- [ ] Keep unscoped mints unchanged (no `expiration`; still ±60 s at the relay).
- [ ] Test: a scoped token carries the `expiration` tag with the expected value; an unscoped one does
      not.

## Task B9 — relocate the ACP driver into the orchestrator (the big one)

The agent run moves from the host into the container orchestrator. The driver code already lives in
`maxplayer-core` (compiled into the binary that ships in the image), so this is INVOCATION, not a
rewrite:
- [ ] The `spawn_agent` seam in `run_phase1` calls `seller_exec::run_agent_with_retry` (the existing
      retry loop) against the workdir under a PASS-THROUGH sandbox policy — the agent runs directly, no
      nested docker (the container IS the sandbox).
- [ ] Inputs carry what the host used to assemble: the composed prompt / task, the deadline, the agent
      argv / harness preset, and the credential-proxy endpoint (`host.docker.internal:<port>`). No
      secret — the proxy holds the model key.
- [ ] The orchestrator reports the run outcome + usage summary to the out files.
- [ ] Heartbeats: base them on "container alive" or a lightweight orchestrator ping, since the host no
      longer watches the ACP stream (small follow-up, not a blocker).
- [ ] ⚠ Not end-to-end testable in-tree — needs a container + a real ACP harness. Unit-test the input
      contract and the pass-through wiring; validate the rest against a running container.

## Task B10 — meter usage at the credential proxy

- [ ] Move usage / budget accounting to the host credential proxy, which forwards every model call and
      is the authoritative meter. Do NOT trust the container's self-reported usage for money decisions
      (a compromised container could under-report to dodge the budget).
- [ ] The container may still emit a usage summary for observability / the roster.

## Server work — brief for the relay agent

The relay half is a separate deliverable: `docs/superpowers/briefs/2026-08-31-relay-scoped-token-lifetime.md`.
Hand it to the agent that manages the buzz relay. Summary: (1) enforce the ref scope (confirm PR #929
covers it), (2) accept an older token ONLY when it carries a VALID ref scope, bounded by its NIP-40
`expiration` up to a hard cap; unscoped tokens keep ±60 s. The client change is inert until this ships
and is confirmed deployed on `buzzrelay.orveth.dev`.

**Verify Track B:**
```
cargo build -p maxplayer --release --no-default-features --features wallet,acp
cargo test -p maxplayer-core --features wallet
cargo test -p maxplayer --features acp,wallet
```

---

## Open items for coordination

- [ ] **Relay deployment.** PR #929's author could not confirm whether `buzzrelay.orveth.dev` is
      built from this vendored copy or a separate `gudnuf/buzz` repo. Until that is confirmed, a
      scoped token gets NO enforcement, silently. Confirm before anyone relies on it.
- [ ] **Sentinel-gate relocation review.** Get the maintainer + security review to accept moving the
      gate into the sandbox — the analysis is in "Review gate — moving the gate + sentinel into the
      container". Also in scope for that review: the long-lived scoped token, and the driver-in-container
      credential model (the model key stays in the host proxy).
- [ ] **Lock the tag name.** `ref` is the agreed key. Keep the client and PR #929 in agreement.

---

## Appendix A — the server contract (from PR #929, verified)

- The signed NIP-98 event may carry one tag `["ref", "<refname>"]`. The relay reads the FIRST
  tag named `ref`, element `[1]` (`transport.rs`).
- `<refname>` MUST be fully qualified and valid: non-empty, `≤ 256` chars, starts with `refs/`,
  no `..`, no byte `≤ 0x20` or `== 0x7f` (`policy.rs is_valid_ref_name`). A bare branch name is
  rejected with 403.
- Enforcement is EXACT match. Every ref in the push must equal the scope. No glob, no prefix
  (`policy.rs` step 3b).
- Enforcement runs only in the pre-receive hook, i.e. the push. The clone / `info/refs` GET is
  not scope-gated, so one scoped token serves both legs.
- NIP-98 dedup stays off, so one token serves the GET advertisement and the push POST.
- The scope is optional and additive. No tag = today's behaviour. An old relay ignores the tag.
- The scope rides inside the HMAC on the relay's own hook path, empty included, so relay version
  skew fails closed.
- Token age tolerance is ±60 s (`buzz_auth::nip98`, `TIMESTAMP_TOLERANCE_SECS`). This is the
  constraint behind decision B-1.

## Appendix B — key code anchors

- Mint: `crates/maxplayer-core/src/git_transport.rs:209`, `:222`.
- Signer actor: `crates/maxplayer-core/src/seller_node/signer.rs:44`, `:182`, `:260`.
- Push call site + delivery branch: `crates/maxplayer-core/src/seller_node/run.rs:6024`, `:6072`,
  `:6095`.
- Fork branch (do not confuse with the push branch): `run.rs:1885`.
- Snapshot + sentinel gate: `crates/maxplayer-core/src/seller_git.rs:293`, `:378`.
- Sentinel value: `crates/maxplayer-core/src/delivery_sentinel.rs:69`.
- Sandbox egress (internet open, host denied): `crates/maxplayer-core/src/sandbox_net.rs`.
- Job workdir mount: `crates/maxplayer-core/src/seller_exec.rs:728`.
- Exploit test (must pass after Track B): `crates/maxplayer-core/tests/hostile_local_git_config.rs`.
- Checkout-safe regression guard: `crates/maxplayer-core/tests/hostile_symlink_checkout.rs`.
