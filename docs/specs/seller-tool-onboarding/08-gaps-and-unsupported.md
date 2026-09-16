# 08 — Named gaps, unsupported and deferred cases, next-stage needs

Plan v3 §6 stage 0's required output: "supported profile requirements and explicit unsupported
cases", plus the named gaps the advisor reviews. Plan v3 §5: **named gaps are a valid Blocked
report, never a silent fallback or a weakening of the approved contract.**

Revised against verdict findings F5 and F6.

## A · Repository gaps

Observed in [01](01-integration-survey.md) at `upstream/main` = `d7b94db`, and re-verified
first-hand against the source after the verdict.

| ID | Gap | Severity | Blocks |
| --- | --- | --- | --- |
| **G-1** | No seller-authored offering/service/listing schema or tool manifest exists. A seller declares only `agent_command`, `rate_sats`, harness names, probed capability tokens and sandbox mode. | **High** | All of [02](02-manifest-schema.md) — new surface, no loader, no validator, no reviewer. |
| **G-2** | Timeout **does** reach `Failed` (`fail_job_with_feedback` `run.rs:7100-7124` → `fail_job` `:8377-8393` → `store.rs:1780-1787`). The gap is that the write is **best-effort** — "a fail-mark that itself errors is logged, never propagated" — and restart re-drives non-terminal rows (`run.rs:4597-4634`) treating a **missing deadline as live** (`run.rs:~1548-1578`). | **High** | Holder close cannot trust the marketplace record. Resolved by the durable holder record and holder-enforced expiry ([04](04-token-grant-contract.md) Part II). |
| **G-3** | `McpServer` is `{name, command}` (`driver/acp.rs:49-59`) — no digest, env allowlist, credential selector, effects declaration or per-job scoping — **and the seller path attaches none**: `mcp_servers: Vec::new()` (`seller_exec.rs:2408-2412`). | **High** | A separate crate alone cannot attach a holder. Stage 2 requires a core-side edit at that call site. |
| **G-4** | No job open/close event bus; plain method calls. | Medium | Close atomicity lands on named adapter call sites. |
| **G-5** | Job id is a bare `String` on the seller path. | Medium | No type-level cross-job protection; the per-call record re-check is the only barrier. |
| **G-6** | No **persistent tool holder** and no **durable service/resource grant** exist: no token bound to holder/party/service/job/grant-version/expiry, no reservation counters, no tombstone. The `jobs` table has no service or grant column (`store.rs:912-929`). | **High** | All of [04](04-token-grant-contract.md). |
| **G-7** | Existing custody mediates a **model** credential for one run. There is no in-holder enrollment of a persistent third-party tool login, and no assumption that a host login is portable. | Medium | Plan v3 §7 enrollment. |

### Correction carried from the verdict

G-6 previously read "no token or holder concept exists **in any form**". **That was false.**
`seller_exec.rs:2575-2620` starts a **per-job host credential proxy** that keeps the real
credential out of the container, forwards a format-plausible placeholder plus a base-URL
override, and **fails closed**: "there is no fallback to putting the real credential in the
container." Supporting surface: `credential_proxy.rs` `JobCredential:230`, `RunningProxy:956`,
`impl Drop:1019`, `PROXY_HOST_ALIAS:201`; namespace ownership at `sandbox_netns.rs:63-70,356-363`.

G-6 is now scoped to what is genuinely missing: the reusable **tool-grant** contract.

### Reusable — do not rebuild

- The credential-proxy mediation pattern and its fail-closed posture (above).
- `SandboxConfig` docker mode mounting only the per-job workdir (`home.rs:550-552`); extra
  mounts exist (`seller_exec.rs:851-854`).
- `docker/maxplayer-sandbox`, `docker/maxplayer-netfilter`, `sandbox_net.rs`, `sandbox_netns.rs`.
- From `codex_subscription.rs`: pinned single upstream (`:10`), lifetime margin against the job
  timeout (`:12`), no-`Debug` secret type (`:14-19`).

**Reuse is not security equivalence.** `FORWARDED_AGENT_ENV` (`seller_exec.rs:301`) forwards a
built-in credential-bearing allowlist plus operator additions (`:908`); plan v3 §3 requires an
**empty-by-default** reviewed child environment. Adopt the mechanism, not the defaults.

## B · Contract gaps found by the walks

Scoped after the verdict: several earlier entries described the approved plan as *permitting*
something it already forbids. The plan is not weakened; the missing pieces are concrete
deferred schemas, checkers and enforcement.

| ID | Gap | Status |
| --- | --- | --- |
| **C-1** | Plan §3 already permits schema-defined HTTP fields and already requires nested body/query, batch, selector, redirect and destination constraints. **Missing: a concrete deferred HTTP schema and checker that enforce them** — not permission to relax them. | deferred work |
| **C-2** | Batch endpoints make the maximum effect a function of body content, so a profile constant cannot express it. Bounded batch counts required. | deferred work |
| **C-3** | Grant membership is defined over identifiers; predicate/filter selectors cannot be grant-checked. Selectors must reduce to enumerated identifiers. | deferred work |
| **C-4** | [03](03-command-policy-mapping.md) §"Constant policy" **already** forbids uncontrolled destinations and callbacks. **Missing: enforcement for HTTP redirects specifically** — redirect-following disabled and destination fields constant or absent. | deferred work |
| **C-5** | Call budgets sized for a CLI become exfiltration budgets under server-side pagination. | deferred work |
| **C-6** | Whether a tool's credential refresh writes are separable from job state is determinable only by inspecting each tool. | per-tool inspection |
| **C-7** | [03](03-command-policy-mapping.md) §"The profile is the unit of review" **already** requires a new reviewed profile version for any change, and the manifest pins a profile digest. **Missing: enforcement that a version bump re-reviews literal-text sinks** — the requirement exists; the mechanism does not. | implementation |
| **C-8** | Effect maxima must be *derived from bindings and enforceable*, not asserted beside them (F5). Any counter without a tool or supervisor control is unbounded and rejects. | resolved in 03/05 |

## C · Router result labels — three distinct outcomes

The previous version filed everything below under one "unsupported" umbrella. **These are three
different results and must never collapse into two** (plan v3 §2).

### C.1 · Unsupported — the kit refuses, and no route exists

1. Any tool whose credential store cannot separate auth writes from job state (C-6).
2. Any command whose safety semantics are not covered by a reviewed profile. A manifest alone
   cannot establish them.
3. Any effect the profile cannot bound with an enforceable control (C-8).
4. Direct vendor tokens without enforceable revocation or vendor-enforced job-close binding.
   Select mediation instead; an expiring stolen token is not harmless.

### C.2 · Deferred — recognized shape, template not in this release

5. Rung 3 (key-swap proxy) and rung 5 (host executor) templates.
6. HTTP transport profiles, pending C-1…C-5 and their own acceptance run.
7. Browser support, including profile mutation and isolation. Browser-based **authentication** is
   not supported for now (Petar, 2026-09-10): it fits the enroll-once holder model only when the
   vendor login persists and the holder can refresh it without a browser. A short-lived,
   non-refreshable login would force a per-job re-login, which the enroll-once model cannot hold.
8. Deriving the allowed operation/resource set from offer text.
9. Manual work as a fallback route, pending a product decision.
10. Platform-hosted credential custody; seller hosting is the initial scope.

### C.3 · Manual setup required — eligible, but no automated adapter

11. Public-tool and direct-token routes. Eligibility guidance exists; **no automated adapter
    ships in this release.** The honest report is *manual setup required* — never *successfully
    onboarded*, and never *unsupported*.

## D · Supported profile requirements

A profile may ship in the initial release only if all of these hold:

1. **rung 4**, with the real tool holding its login inside a persistent isolated holder;
2. **argv-shaped invocation** with a complete source-to-sink row — source, validation/encoding,
   sink, vendor interpretation, allowed effect — for every field **and every constant**;
3. **constants that close the dangerous sinks**: no shell/eval, no plugin load, no
   job-selectable configuration, no secret-dumping debug mode, no uncontrolled destination; plus
   a non-interactive control where the tool would otherwise prompt;
4. **effect maxima derived from bindings and enforceable** by the tool or the supervisor, with
   input, output and network counted separately (C-8);
5. **separable auth-store writes** (C-6), verified by inspection before build;
6. **private, holder-created output slots**, checked before export: no symlinks, no devices, no
   out-of-slot paths;
7. **holder-issued handles and slots immutably bound to holder/party/job**, with an admission
   membership check on every call ([05](05-walk-a-file-processing-cli.md));
8. **a reviewed egress allowlist**, default deny, ideally a single pinned upstream;
9. **literal-text sinks re-reviewed at every profile version bump** (C-7).

> **Narrowing removed (F6).** A previous requirement here made **single-file output mandatory
> for every initial-release profile**. Plan v3 §3 requires *private checked output slots*, not
> single-file tools, and applying walk A's example limit to the frozen-kit unseen-tool
> adjudication would silently narrow the approved acceptance scope. Requirement 6 now states the
> plan's actual rule. Single-file output remains the chosen limit of **walk A's own profile and
> the synthetic demo**, and binds neither the kit nor tool two.

A tool failing any of these is `unsupported` or `deferred` per §C — a routing success, not a
failure to work around.

## E · Next-stage needs

**None of these blocks the already-authorized synthetic demo.** The custom CLI plus local fake
authenticated service on Linux Docker with synthetic credentials is settled and needs no tool
selection, no live account and no platform decision. The table below concerns the **real-tool
release path only**, at its own later gate.

| Need | Owner | Gate |
| --- | --- | --- |
| Accept or replace the three core adapters and the dependency direction | maxie / advisor | this review |
| Accept or replace the owned-award admission boundary and durable holder record | maxie / advisor | this review |
| Accept `crates/maxplayer-tool-kit` as the policy/holder component | maxie / advisor | this review |
| First real tool selection | Petar | before the **real-tool** prototype, not before the demo |
| Supported release platform confirmation | Petar | before release, not before the demo |
| Disposable-resource/effect allowance | Petar or Josip | before any write-producing live example; absent it, live checks stop at one health request and one read |
| Tool two selection | Petar or Josip | after freeze, for the unseen-tool acceptance test |
| Router skill publication | stage 2 | through `skill_workshop`, never a direct `SKILL.md` write |

## F · Standing honesty condition

> The stage-1 custom CLI plus local fake authenticated service proves the **mechanism only**. A
> CLI written to fit the contract cannot falsify the contract. It is not independent real-tool
> acceptance and not general onboarding acceptance.
>
> Contrasting real-tool acceptance — two services on tool one plus one on a contrasting tool two,
> selected after freeze, with an independent oracle and zero human interventions — is **retained
> in full** and receives **zero credit** from any fixture-only or fake-vendor result. Correct
> `unsupported` routing does not satisfy the second-tool success gate.
