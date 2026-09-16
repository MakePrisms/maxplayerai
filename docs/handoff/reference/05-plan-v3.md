<!-- PROVENANCE: copied verbatim into this repository for handoff on 2026-09-10.
     Original lived on the authoring machine outside this repo and is NOT reachable to you.
     Absolute /Users/... paths appearing below are historical references from the time of
     writing; the in-repo copies under docs/handoff/reference/ are now authoritative.
     Content is unmodified apart from this header. Scanned for credentials: none found. -->

# Seller tool onboarding kit — plan v3

Author: maxie. Date: 2026-09-09. Requested by Petar in thread 1546802028091670630.
Status: revised design, awaiting focused advisor review. No implementation or staffing authorized by this document.
Supersedes PLAN-v2-20260908T1521Z.md. Review baseline: /Users/forge/forge/v2/advisor/verdicts/20260908-seller-tool-onboarding-plan-v2.md, SHA256 13bbe627ae88e8ed95b69ef16fba0252a7d4b98ab31425554678e70101a1625e.

## 1. Goal and deliverable

Give the seller's agent a short skill, versioned templates, examples and a checker. It should connect a supported tool without inventing server code or researching the platform. This is a hypothesis to test, not a promise that arbitrary tools are supported.

The repository contains a router skill, manifest schema, command-policy profiles, job-grant contract, fixture checker, live checker and templates. The seller edits a manifest selecting approved operations. A new command whose safety semantics are not covered requires a reviewed command-policy profile; a manifest alone cannot establish them. This is an explicit limit on generic onboarding, not hidden seller-agent implementation work.

Use plain terms: a seller operates tools; a job is one buyer request; a holder is a persistent isolated service containing a tool's login. An offering means the seller's advertised service, not a requirement that every marketplace listing have fixed pricing or a permanently fixed schema. The job receives an explicit allowed operation/resource set. Deriving that set from offer text is deferred.

## 2. Selection and release scope

Preserve the v2 routing decision, including the approved exclusion of broad direct credentials:
1. Public tool: install in the job image.
2. Direct vendor token: eligible only if a seller/vendor custodian issues it without exposing persistent refresh secrets, its lifetime fits the job budget, and its resources/actions fit that job's approved service and resources. Also require enforceable vendor revocation or vendor-enforced job-close binding. If immediate close enforcement cannot be established, select mediation instead. Do not describe an expiring stolen token as harmless.
3. Key-swap proxy: prefer it when the client can be routed through it, auth material travels in a supported replaceable field, and request semantics can be constrained. Path/method alone are insufficient for GraphQL, MCP or other body-dispatched APIs. Signing protocols are not simple token substitution.
4. MCP in a persistent isolated container: use when the real CLI or browser must hold login state. The trusted tool runs here, not in the buyer-controlled job container.
5. MCP host executor: use only when required platform or machine-bound login cannot operate in the container. Dedicated isolated machine/VM appropriate to the tool; no general shell on the seller's everyday host. Hardware-bound or licence restrictions may make even this unsupported.
6. Otherwise report unsupported. Manual work is deferred pending a separate product decision.

Route mixed interfaces per operation. Separate holders for different parties and vendors. Sharing a holder never grants one job another job's resources. Seller hosting is the initial scope; platform custody is deferred.

Stage 2 ships only the rung-4 CLI template. Rungs 3 and 5 are recognized but return `recognized shape, template deferred`; browser support does likewise. Never silently select a less secure route because its template exists. Public/direct routes have eligibility guidance but no automated adapter in this release; report manual setup required, not successful onboarding. Direct-route guidance requires evidence for every eligibility predicate, including close enforcement.

Router fixtures include public CLI, job-bound direct token, broad token, refresh-dependent token, API/CLI mixture, browser login, machine-bound login and unsupported platform. Expected decisions are recorded before testing. Deferred is distinct from unsupported and from success.

## 3. Manifest and command policy

The server accepts neither shell strings nor arbitrary executable/argv requests. A manifest selects a reviewed policy profile and fills declared values. A profile pins executable/image identity, subcommand, constant options, permitted environment names, stdin format, credential lookup, operand interpretation and permitted effects. Changes require review and a new profile version. Seller-agent claims about a command are not sufficient to approve a profile.

Allowed parameter types: bounded enum, bounded integer, boolean, grant-bound resource identifier, bounded literal text, opaque uploaded-artifact handle and holder-created destination slot. Each expands to one complete argument or one schema-defined field, never embedded substitution or recursive expansion.

Each profile supplies a source-to-sink mapping. For every field, specify its source type, validation/encoding, exact argument or stdin/HTTP field, vendor interpretation, and allowed effect. Literal text is allowed only where that command treats it as data, not a URL, path, expression, response file or config selector. Resource membership does not replace vendor-specific encoding. Enum values and constants receive the same authority review as variable fields. Reject unknown mappings.

Profile constants bind account, tenant, endpoint and credential selector to seller-approved policy. Reject constant options enabling shell execution, plugins, arbitrary configuration, debug-secret dumps or uncontrolled destinations. Environment starts empty except reviewed tool/runtime variables; no job-controlled PATH, HOME, proxy, loader or credential selectors. No shell is involved. A terminator helps parsing but does not establish operand safety.

For a fixture literal-text operand, permit ASCII letters, digits, spaces, underscore, dot and hyphen, length 1–64, with first character alphanumeric. Thus `-x`, `@file` and `x;touch y` reject; `hello-world` is valid literal data. Real profiles may use other explicitly reviewed grammars; tests derive from those grammars rather than assuming all tools accept a terminator.

Uploads become holder-owned immutable input objects: ingest under a size bound, open and validate without following links, copy into a private staging directory inaccessible to the job, then retain unchanged through CLI consumption. The CLI receives only that private path. The job cannot rename its parent or swap its content. Output slots are private, checked before export; reject symlinks, devices and out-of-slot outputs. No raw host paths enter the API.

Deferred HTTP profiles must additionally constrain nested body/query values, batch counts, resource selectors, redirects and destinations. Fixed host/method plus arbitrary nested strings is not an acceptable policy. No claim of HTTP support until that profile/checker work passes its own acceptance.

## 4. Authentication, custody and isolation

Choose trusted credential-reading children, not supervisor-only credential custody. The supervisor and genuine CLI inside the holder are trusted; buyer prompts, inputs and job containers are not. Containers alone do not prove a CLI cannot disclose its credential. Reviewed input semantics, egress, output handling and isolation are also required.

The seller enrolls interactively inside the persistent holder through a protected local terminal or vendor browser flow. Secrets never go into chat, command arguments, manifests or checker logs. The tool stores its own authentication state in that environment. A host login is not assumed portable. Initial enrollment may need the seller; jobs do not require repeated enrollment while the session stays valid.

The profile names credential/config lookup locations. At invocation, bind HOME to a private per-job directory with a controlled configuration base and private writable cache. Expose only the selected credential store to the trusted child. No buyer container sees that mount. Unrelated host files, other jobs, Docker sockets and host process namespaces remain unavailable. Read-only credentials are used only for tools that support them.

Tools requiring refresh/profile writes use a serialized credential-maintenance operation outside job control. Only its designated auth-store writes persist. Job-generated cache/config never merges back into the credential base. If a tool cannot separate those writes safely, mark that profile unsupported in the initial release. Browser profile mutation/isolation is deferred, not solved merely by a lock.

Per-job invocations have an enforced process and filesystem boundary, a private input/output area and reviewed egress. Implementations must prove those boundaries on the supported platform; running an MCP server in Docker does not alone satisfy this contract. Killing a process group is insufficient if descendants can escape it: use supervisor-owned container/cgroup or equivalent lifecycle control and test descendants.

The seller approves allowed job opener identities, parties, service IDs, verbs, resource sets and ceilings in holder policy. An authenticated opener cannot grant itself more authority. On job creation, validate the request against that policy and the authoritative job record; reject excess rather than silently granting it. The holder issues a token bound to holder, party, service, job ID, grant version and expiry. Every call verifies signature, audience, time, active job record, party/service equality, verb/resource membership and remaining budget. Claims alone do not override the record. Policy changes cannot broaden an existing job.

Close on success, failure, cancellation or timeout atomically denies new calls, revokes mediated tokens, stops owned processes and removes job data. Closed-job records persist through token expiry; restart fails closed until active records are reconciled. Renewals cannot revive closed jobs. Vendor operations already accepted can outlive local cancellation. Record this residual; counters do not bound every financial or destructive consequence.

Reserve calls/items/bytes atomically before execution using profile-declared maximum effects. Refuse unbounded operations. Counters survive restart, do not reset on token renewal, and reset only for a separately authorized new job. Refund only proved-unused reservations. Limits are admission bounds, not claims that prior vendor work is reversible.

## 5. Checker contract

These are planned tests, not existing commands or measured security guarantees. The checker reports PASS/FAIL/SKIPPED with evidence. Dependency failure causes dependent checks to be SKIPPED, never fake PASS. A manifest fault and a defective runtime require different fixtures.

Stage-2 fixture defaults: no internet, fake vendor only, synthetic credentials only, two parties P/Q, jobs J/K, holders H/G, resource R allowed and S forbidden. Maximum 20 simulated invocations and 64 KiB generated data per case; reset between cases. No real spend or external writes. Tests below apply to the stage-2 MCP-container CLI template unless noted. Deferred transport tests are SKIPPED with their deferred reason.

1. Schema/profile: valid manifest loads; embedded hole, unknown profile, unsafe constant and text-to-URL mapping each reject before invocation. Oracle: validation error and zero child calls. Effect budget zero calls for negative inputs.
2. Discovery/result: list equals approved verbs; fixture `render(R)` returns known bytes with expected digest. Oracle: independent expected file, not exit code or server self-report. One fake call.
3. Operand grammar: negative examples in §3 reject with zero calls. `hello-world` arrives as exactly one literal operand and produces expected bytes; no forbidden-effect marker. One positive call. Real-profile grammar probes use their recorded expected outcomes.
4. Artifact consumption: job provides forged handles, then an ingress test driver swaps symlinks/renames upload entries during staging and after validation. No outside-file read marker may fire. Invalid inputs reject; an already staged valid object may succeed only with its original digest. Private-consumer bytes must match the staged digest. At most two fake calls and 8 KiB.
5. Authentication: missing/garbage signature, wrong holder, party, service, job, natural expiry, post-expiry and each close outcome reject with zero calls. Use controlled fixture clock and authoritative records. Valid J/H/P call succeeds once. Include same token after restart and renewal against a closed record.
6. Grant authority: unauthorized opener, excessive grant request, forged claim grant version, verb/resource outside J and attempt to use K's valid token against J reject. Zero unauthorized calls; seller-authorized J/R succeeds once.
7. Leakage: synthetic auth secret may appear only in the declared fake-vendor authentication channel, never outputs, artifacts, errors, logs or other egress. A separate non-auth canary may appear nowhere outside its forbidden store. Inspect every captured channel; exercise both success and failure. Two fake calls. Failure to authenticate at the permitted sink also fails the positive oracle.
8. Isolation: concurrent J/K attempts cannot read each other's HOME/input/output markers; sequential K cannot see J's config/cache mutations. Instrumented child observes mounts and identities; forbidden-read marker stays zero. Four fake calls, 8 KiB. Credential lookup must still succeed at its permitted store.
9. Cleanup: start a fixture child with a descendant, close/cancel/timeout J, and verify no owned process or job filesystem remains within five seconds. Repeated calls refuse; unrelated K remains usable. Repeat natural token expiry with an active child. At most four fake invocations, no external effects.
10. Budgets: independently set call=3, item=3 and byte=8 limits. Test exact boundary, one over, and concurrent reservations where combined demand exceeds the limit. Only admitted effects occur; renewal/restart cannot reset counters. New authorized K has separate counters. Maximum ten fake requests and 16 bytes per subcase.
11. Lifecycle: fake vendor expires auth mid-operation. Holder returns credential-expired, marks unhealthy, queues one seller notice, pauses registration and refuses new work. Simulated protected re-enrollment plus successful health allows a newly authorized job; the closed failed job remains closed and no write auto-replays. Four fake calls, no external writes.
12. Registration: fail each mandatory check and verify rejection/pause; pinned version/profile drift invalidates prior acceptance. One fake registration per variant, zero vendor effects.

Negative controls include schema-invalid manifests AND runtime mutants: bypass signature validation, omit audience/party checks, leak auth to output, expose another job's mount, leave descendants alive, skip quota reservation and retain closed tokens. Each has a named required FAIL and dependent SKIPPED outcomes. Do not demand exactly one failing line.

Live checks use only seller-approved test resources: one health request and one read per run, with an independent expected result. A write-producing acceptance example requires a separate explicit disposable-resource/effect allowance. Stop at that allowance; lack of permission is a blocker, not permission to improvise. Remote black-box checks cannot prove internal isolation. Record pinned artifacts and local isolation evidence separately; registration must not label an arbitrary mutable endpoint secure from live checks alone.

## 6. Build and acceptance order

Stage 0: paper contract. Walk one file-processing CLI and one tenant-aware HTTP API through the mapping, custody, grant and checker contracts. Advisor reviews the named gaps. HTTP is a contrast test, not shipped support. Output: supported profile requirements and explicit unsupported cases.

Stage 1: disposable CLI prototype, one tool selected by Petar. Maximum one worker, two working days or 80 worker turns, whichever occurs first; at most two design attempts. Stop with evidence at the bound. Use test credentials and approved resources. Prove one real result and inventory profile features. This is not a shipped template and is not security approval.

Stage 2: build the reusable CLI kit, full fixture suite and lifecycle handling. Write the stage-2 router skill and enrollment/recovery instructions BEFORE acceptance. Pin server, schema, policy profiles, checker, examples and skill together. Profile/server/skill changes reset the acceptance run. Skill publication follows the available skill review/publication mechanism; this plan is not a skill publication.

Acceptance requires two services on tool one plus one on a contrasting tool two, with no edits to the pinned kit. Petar or Josip selects tool two after freeze; this is also the unseen-tool test. Before execution, advisor independently assesses eligibility against frozen capabilities and records required outputs, forbidden effects and safe effect allowance. A tool requiring a new profile is an unsupported capability, not silent success.

A haiku-class agent receives only the pinned kit, task inputs and an already enrolled test holder. It may edit only the seller manifest and job input data. Within 40 turns and two hours, with zero human interventions, it must pass applicable checks AND produce the real tool's expected result. A human-produced artifact or independent vendor read is the oracle, recorded before the run. No fixture-only success. Builder/agent cannot decide its own unsupported escape: advisor adjudicates against the predeclared policy. Correct unsupported classification is a routing success but does NOT satisfy the required successful second-tool onboarding; another eligible sample must be selected or the acceptance remains incomplete.

Across all three services, verify out-of-grant and closed-token rejection. Run all fixture negative controls, concurrency, cleanup, budget and expiry/re-enrollment cases. Independently verify real outputs. Test registration rejection and subsequent pause. Evidence bundle includes hashes, commands, redacted observations, oracles, interventions and skipped reasons. Maxie verifies the named gate once, advisor reviews, and humans decide release. No implementation follows merely because this plan is reviewed.

Later stages separately cover proxy, host executor and browser templates, each with real examples and its own custody and acceptance tests. No enrollment UI, manual workflow or platform-hosted credential custody ships under the initial stage.

## 7. Operations, decisions and risks

The seller owns enrollment, renewals and holder patching. Failed authentication pauses new jobs; notices contain no secrets. Supported-version policy and platform hosting need human decisions before release, not before revising this plan. Initial login happens in the holder, not by assumed copying from the host.

Petar selects the first test tool and confirms the supported initial platform before a prototype brief. Each worker brief goes through hearth, names maxie as ordering seat and carries its stop bound and gate. No worker is requested by this document.

Main risks: reviewed profiles may make onboarding too narrow; a weak agent may not succeed; some credential stores cannot separate auth updates from job state; isolation is platform-specific; vendor effects can continue after close. The prototype and contrasting frozen-kit test measure the first two. Unsupported is an honest limit, not a reason to expose a host shell.

## 8. Review repair map

A: retain broad-token exclusion and mediated cross-job/post-close denial; add stricter direct-close eligibility (§2, §4).
B: reviewed source-to-sink and constant policy; private staged consumption; adversarial mapping and race checks (§3, §5.1–4).
C: distinguish accepted/rejected literals, legitimate auth sink, runtime mutants and dependent skips; add explicit applicability, observations and budgets (§5).
D: skill before freeze; bounded prototype; frozen unseen real-result acceptance and independent unsupported judgment (§6).
E: trusted credential-reading child, enrollment/HOME/write mapping, opener authority, close enforcement, descendants and reservation semantics (§4–5).
F: explicit lifecycle test; prototype is not shipping; deferred-template results and browser deferral (§2, §5.11, §6).
