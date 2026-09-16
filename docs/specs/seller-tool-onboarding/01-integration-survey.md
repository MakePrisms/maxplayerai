# 01 — Integration survey, proposed kit location, real job connection point

Survey of `maxplayerai` at `upstream/main` = `d7b94db`. Every claim carries a path; anything not
observed is marked **NOT FOUND** or **INFERENCE**. Plan v3 §6 stage 0 requires this inspection
before any contract is proposed, precisely so the contract does not "invent config as already
supported".

> **Revision note (F3).** The first version of this survey stated that credential custody today
> is "host-held and injected into the container". **That was wrong**, and the error mattered:
> the seller execution path already runs a per-job host credential proxy that keeps the real
> secret out of the container and passes a placeholder instead. §4 below is rewritten from the
> source. The related overstatement in G-6 is corrected in
> [08](08-gaps-and-unsupported.md).

Workspace members (`Cargo.toml:2-8`): `crates/maxplayer-core`, `crates/maxplayer-desktop`,
`crates/maxplayer-evals`, `crates/maxplayer`, `crates/maxplayer-relay-write-policy`.
`crates/buzz/` is on disk but is **not** a workspace member.

Citation convention: unqualified `store.rs` and `run.rs` are under
`crates/maxplayer-core/src/seller_node/`; other files are under `crates/maxplayer-core/src/`.

## 1. Seller onboarding and configuration — what exists

| Thing | Where |
| --- | --- |
| `SellerConfig` | `home.rs:193` |
| `SandboxConfig` | `home.rs:550` |
| root `MaxplayerConfig` | `home.rs:1471` |
| `load_config` / `save_config` | `home.rs:1923` / `home.rs:2224` |
| `require_seller_config` | `seller.rs:54` |
| interactive onboarding | `crates/maxplayer/src/sell.rs`, `ensure_seller_config:318`, entry `run:75` |
| harness registry | `seller_agents.rs`: `RegisteredAgent:51`, `AgentRegistry:130`, `resolve:263` |
| capability tokens | `capability.rs:36` — `CAPABILITIES = ["node","python","rust"]`, probed by `probe_capabilities:145` |
| buyer-repo declarative config (per-job, **not** seller-authored) | `checks.rs`: `DECLARATION_PATH = ".maxplayer/checks.toml"` `:11`, `parse_declaration:184`, 64 KiB limit `:14` |

`SellerConfig` fields: `agent_command`, `rate_sats`, `takes_no_payment`, `git_remote`,
`job_timeout_secs`, `agents`, offer-acceptance flags, `slots`.

**NOT FOUND: any seller-authored offering / service / listing schema, tool manifest, or
per-offering declarative registration.** Today a seller declares an agent command, a rate,
harness names, a sandbox mode, and capability *tokens that are probed rather than declared*.

Everything in [02](02-manifest-schema.md) is therefore **PROPOSED** new surface with no
existing loader, validator or reviewer.

## 2. Job lifecycle and the authority model

> **Findings stand; the use they were put to does not.** Everything this section establishes
> about the repository is verified and still true — an award row is not a win, and the job
> record carries no service or grant. What the scope correction withdrew is the *inference*
> that the tool lifecycle should therefore hang off the award boundary. It should not: the tool
> is enrolled per seller daemon, so §2.2's "the job record has no grant" is no longer a gap to
> close but simply the correct shape. See [00](00-README.md) for the governing correction.

### 2.1 An award row is not a win

This is the single most important correction to the first version of this survey, and it
governs the connection point in §5.

`Store::record_award(&self, award_id: &str, job_id: &str, buyer_pubkey: &str, now_unix: i64)
-> Result<Awarded, StoreError>` (`store.rs:1590`) returns
`enum Awarded { New, Duplicate, NoClaim }` (`store.rs:781-788`). Its own doc comment
(`store.rs:1584-1589`) is explicit:

> `#814 WIDENED WHAT A ROW MEANS ... The suppression path now records an authentic buyer award
> for an offer we recorded but never claimed — someone ELSE's win — so the row means "an award
> for this job exists", nothing more. The discriminator for "we won" is a CLAIM row ..., never
> the presence of an award.`

Confirmed in the arms themselves:

- **`Duplicate` returns before the claim is read** — `if inserted == 0 { tx.commit()?; return
  Ok(Awarded::Duplicate); }` precedes `let claim = claim_state(&tx, job_id)?;`.
- **`NoClaim` records the award and creates no job** — "Award for a claim we do not hold —
  record the award, create no job."
- **`New` is an insertion result**, not cryptographic proof of selection.

Three distinct callers reach this one method:

| Caller | Location | Meaning |
| --- | --- | --- |
| award handler | `run.rs:6428-6459` | **only the `New` arm dispatches** `spawn_bounded_execution` |
| suppression | `run.rs:6291-6294` | `suppress_taken_elsewhere` records *someone else's* win |
| ACCEPT | `run.rs:6172-6182` | binds an award and logs `"... bound from ACCEPT with no prior award ({outcome:?}) — NOT executing"` |

The genuine authority check lives in the caller, not the store. The buyer is taken from our own
recorded offer — "Only an offer we recorded can be awarded to us; its buyer is the sole
authorized awarder" (`run.rs:6382-6386`) — and `match_award` (`run.rs:772-788`) requires both
`award_author == offer_buyer` and equality between the award's claim id and **our published
local claim id** before returning `AwardMatch::Execute`.

**Consequence: a hook on `record_award` is unsafe.** It would join three paths with different
authority and lifecycle meaning, and would mint authority for someone else's win.

### 2.2 The job record has no service or grant

`jobs` table (`store.rs:912-929`): `job_id`, `offer_id`, `agent_name`, `state`,
`created_at_unix`, `updated_at_unix`, `pushed_commit`, `settled_elsewhere_at_unix`.

`JobState` (`store.rs:708`) is `Awarded/Executing/Delivered/Paid/Failed`; `is_finished()`
(`store.rs:738-743`) is a **plain predicate**, not an event or callback facility. Job id is a
bare `String` on the seller path (the `JobId` newtypes at `event.rs:51` and `payment.rs:33` are
unused here).

**There is no service column and no grant column.** Party/service/grant equality therefore has
no existing durable representation to check against; the contract must supply one.

### 2.3 Timeout, failure and restart — corrected

> **Revision note (F2).** The first version claimed timeout "does not pass through a store
> state". **That was false.** maxie's ruling and the source agree: timeout *does* reach
> `Failed`. The real hazard is different, and worse.

The chain exists: deadline-bound agent errors go through `fail_job_with_feedback`
(`run.rs:7100-7124`) → `fail_job` (`run.rs:8377-8393`) → the store persists `Failed`
(`store.rs:1780-1787`).

The hazard is that **the fail write is best-effort**. `fail_job`'s own doc reads
"(best-effort; a fail-mark that itself errors is logged, never propagated — the loop keeps
serving)", and its arms confirm it: `Ok(0)` logs "no job row moved to failed ... nothing was
healed", and `Err(error)` logs "fail_job write error (continuing)". Marketplace availability is
correctly prioritised over the fail write — but a holder that trusts that record inherits an
uncertainty it cannot see.

Restart semantics compound this. Restart re-drives `Awarded`/`Executing` rows
(`run.rs:4597-4634`); graceful shutdown deliberately leaves them for replay
(`run.rs:4721-4727`); and resume treats a **missing deadline as live**
(`run.rs:~1548-1578`): "A `None` deadline (absent/unreadable) is treated as LIVE: never fail a
genuine award on a missing fact — over-skipping is the worse (a lost award)."

That is the right call for a marketplace, and exactly the wrong default for credential
authority. **The holder must fail closed where the marketplace fails open.** See
[04](04-token-grant-contract.md) §"Durable holder admission record".

## 3. MCP integration — and the attachment gap

- `crates/maxplayer/src/mcp.rs` — maxplayer's own MCP **server**, exposing job tools.
- `driver/acp.rs:49-59` — `pub struct McpServer { pub name: String, pub command: Vec<String> }`.

Name + argv only: no image digest, no environment allowlist, no credential-store selector, no
effects declaration, no per-job resource binding.

**And the seller execution path attaches none.** `seller_exec.rs:2408-2412` builds
`SessionConfig { cwd: launch.cwd, mcp_servers: Vec::new(), env: identity.git_env() }`.

**This is the attachment gap, and it is decisive for the kit's shape (F3):** no seller job
receives any MCP server today. A separate crate can define a holder, but **a separate crate
alone can never attach one to a job.** Stage 2 requires core-side edits at this call site. Any
statement that a separate crate "makes no runtime changes checkable by diff" describes stage-0
paper only, and must not be read as implying stage 2 needs no integration edits.

## 4. Credentials, isolation and egress — corrected survey

### 4.1 A per-job credential proxy already exists

`seller_exec.rs:2575-2620`, "Credential containment (#647)":

> the real model credential must NOT enter the container: a stranger's job can read
> `-e ANTHROPIC_API_KEY` and exfiltrate a reusable secret. Start a per-job host proxy that holds
> the real credential, forward a format-plausible placeholder + a base-URL override pointing at
> the proxy in its place ... **If containment is required but cannot be established, the job
> FAILS — there is no fallback to putting the real credential in the container.**

Supporting surface in `credential_proxy.rs`: `JobCredential:230`, `RunningProxy:956`,
`impl Drop for RunningProxy:1019` (aborts owned listener/connection tasks),
`PROXY_HOST_ALIAS = "host.docker.internal":201`. The module doc describes value-based
substitution — the proxy identifies the job by finding the placeholder in a request header and
substitutes the real credential on the way out; the return leg scrubs the real credential back
to the placeholder; a refused destination fails the request and "the caller NEVER falls back".

Network-namespace holder/ownership records exist at `sandbox_netns.rs:63-70,356-363`, with a
firewall pinhole driven by `[sandbox] proxy_port_range`.

**So the repository already contains a working mediation mechanism with fail-closed
containment, per-job credential structures, ownership records and lifecycle teardown.** Three
of plan v3 §4's principles are already implemented here for the model credential, and the kit
should treat them as prior art rather than invent parallel machinery.

### 4.2 What is nonetheless absent

Scoped precisely, because the first version overstated this:

- **No persistent tool holder.** The proxy mediates a *model* credential for the duration of a
  run. It is not an enrolled, persistent, third-party-tool login environment.
- **No durable service/resource grant.** No token bound to holder/party/service/job/grant
  version/expiry; no reservation counters; no closed-job tombstone.
- **No per-job MCP tool scoping** (§3).
- Enrollment is not in scope of the proxy at all.

`codex_subscription.rs` contributes a pinned single upstream (`:10`), a token-lifetime margin
measured against the job timeout (`:12`), and a no-`Debug` secret type (`:14-19`) — the last of
which is a pattern the kit should copy directly.

### 4.3 Sandbox and environment — do not mistake reuse for equivalence

`SandboxConfig.mode` (`home.rs:550`) selects `launcher` or `docker`; docker mode "runs the
command inside a container that mounts ONLY the per-job workdir" (`home.rs:552`). Extra mounts
exist (`seller_exec.rs:851-854`).

Environment forwarding is **not** empty-by-default: `FORWARDED_AGENT_ENV`
(`seller_exec.rs:301`) is a built-in allowlist — `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `OPENAI_API_KEY`, … — **plus** anything
`[sandbox] forward_env` adds (`seller_exec.rs:908`).

Plan v3 §3 requires a reviewed child environment that starts **empty** except reviewed
tool/runtime variables. That is a *different* policy from the existing one. The existing
allowlist is a good model and is testable against an injected lookup (`forwarded_agent_env_from`),
but **reuse is not security equivalence** and the kit must not inherit these defaults.

## 5. Proposed kit location and job connection point

Per maxie's ruling, a separate policy/holder crate with **explicit core adapters** is
acceptable. Stage-0 paper lives at `docs/specs/seller-tool-onboarding/`; stage 2 proposes a new
workspace member `crates/maxplayer-tool-kit`, with `profiles/` and `fixtures/`.

**Dependency direction:** `maxplayer-tool-kit` depends on nothing in `maxplayer-core`'s seller
path. `maxplayer-core` depends on the kit through narrow adapter traits it owns. The kit is
policy and mechanism; core supplies authority facts and lifecycle events.

### The three core-side adapters

| Adapter | Core call site | Responsibility |
| --- | --- | --- |
| **authorization** | the owned-award / eligible-execution boundary — the `Awarded::New` arm at `run.rs:6428-6459`, **after** `match_award` returned `AwardMatch::Execute` | supply positive proof of our own win and the seller-approved service binding; request grant open |
| **lifecycle** | every terminal path: success, all failure/timeout paths through `fail_job`, cancellation/shutdown (`run.rs:4721-4727`), boot reconciliation (`run.rs:4597-4634`) | request holder close with an explicit reason |
| **transport** | `SessionConfig`'s `mcp_servers` at `seller_exec.rs:2408-2412` | attach the job's holder MCP endpoint and deliver its job token, without exposing persistent credentials |

**Not** a hook on `record_award`: §2.1 shows that method is reached by suppression and by
ACCEPT-without-execution, and its `Duplicate` arm never reads a claim.

Full open/close obligations, the durable binding, the duplicate/replay rules and the
fail-closed protocol are specified in [04](04-token-grant-contract.md).

## GAPS

- **G-1** No seller-authored offering/manifest surface exists.
- **G-2** Timeout **does** reach `Failed`, but the fail write is best-effort and restart
  re-drives non-terminal rows treating a missing deadline as live.
- **G-3** `McpServer` is name + argv, and the seller path attaches **none**; core-side edits are
  required at `seller_exec.rs:2408-2412`.
- **G-4** No event bus for job open/close — named call sites only.
- **G-5** Job id is a bare `String`; no type-level job binding.
- **G-6** No *persistent tool holder* and no *durable service/resource grant* exist. (A per-job
  credential proxy, per-job credential structures and namespace ownership records **do** exist —
  §4.1.)
- **G-7** Existing custody covers a model credential for one run; no in-holder enrollment of a
  third-party tool login.

Severity, contract gaps and unsupported cases: [08](08-gaps-and-unsupported.md).
