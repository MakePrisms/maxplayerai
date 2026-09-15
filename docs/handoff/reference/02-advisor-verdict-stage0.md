<!-- PROVENANCE: copied verbatim into this repository for handoff on 2026-09-10.
     Original lived on the authoring machine outside this repo and is NOT reachable to you.
     Absolute /Users/... paths appearing below are historical references from the time of
     writing; the in-repo copies under docs/handoff/reference/ are now authoritative.
     Content is unmodified apart from this header. Scanned for credentials: none found. -->

# Seller-tool onboarding stage 0 — FIRST FULL artifact review

Outcome: **REVISE**. Paper-contract acceptance is withheld. This is not a regrade of the approved plan and is not an implementation/security verdict. The nine documents substantially preserve the plan, but their integration proposal, independent test oracle and several example/eligibility rules need correction before Maxie's prototype brief uses them as an accepted contract.

Ordering lane: `agent:maxie:discord:channel:1546802028091670630`. Parent owns ALL sends, Discord and fallback. No external delivery performed by advisor.

## Identity and review boundary

- Repository: MakePrisms/maxplayerai.
- Reviewed head H: `1b3ec201b9007b67a33fa63e4156b512387bfc0e`.
- Reviewed base B: `d7b94db2dbb7aeeefdcbb087edd0c90df56a8bdb`; independently measured merge-base equals B.
- Delta: nine added files, 1,273 added lines, no runtime/test/build/dependency changes. All nine added documents were read completely, as bounded numbered excerpts of the additions. Relevant B source regions were independently inspected; no full base-file rereads.
- Plan copy: 20,400 bytes, SHA256 `1c35ba32686af63dee874943a676461191dd8ea30149a6504c66fe47e1353e01`, independently measured. Approved PLAN-ONLY verdict: 11,503 bytes, SHA256 `44377fb083c4aca2a029ade51235c7aed12807c965571b0cd2c2653a8245374a`, independently measured. Implementation brief hash also matches README provenance: `d7053b791496d70a0cf5adcb982a488366076acfe0b71aff56c0e275d686a160`.
- Evidence root: `/Users/forge/forge/v2/advisor/scratch/seller-stage0-1b3ec201/`. Isolated bare object store: `repo.git`. `full.diff`, added-document copies, base-source spills, acquisition/publication receipts and manifest accompany this verdict.
- Acquisition/publication: fork network fetch of exact H failed `not our ref`; both H and B were then fetched independently from the named worker repository into advisor's bare evidence store. This did not change the worker checkout. GitHub upstream commit-to-PR lookup returned HTTP 422/no such commit both at start and end. Fork seller-named refs and an upstream all-state PR search did not identify this delivery. Therefore **publication and a matching PR are not established**, not an invented docs-object PR gate. The review remains valid for the locally acquired immutable H/B objects; it does not certify a published PR/base/check lifecycle. No worker clean-HEAD claim is needed for this object-based grade.
- No code, tests, builds, docs tools, imports, parsers from the delivery, containers or live services were run. Git object/file inspection and evidence bookkeeping only. No checkout/worktree mutation, credentials, spend, merge, host changes, EC2 work or human-thread posts.

Citation convention: `00`–`08` mean files under `docs/specs/seller-tool-onboarding/` at H, named in the inventory below. All `store.rs`, `run.rs`, `seller_exec.rs`, `credential_proxy.rs`, `sandbox_netns.rs`, `home.rs` and `driver/acp.rs` citations refer to `crates/maxplayer-core/src/` at B; store/run are under `seller_node/`. Plan citations refer to the hash-bound plan above. PASSING below means sufficient **paper specification**, never a measured runtime pass.

## Stage-0 requirement grades

| Requirement | Grade | Evidence / disposition |
|---|---|---|
| Correct plan provenance and paper-only fence | PASSING | 00-README.md:3–24,41–47; exact hashes and docs-only diff independently verified. |
| Existing onboarding/config/MCP/credential-proxy survey | FAILING | 01-integration-survey.md:86–143,164–174 misses the actual credential proxy and misstates real-secret injection; F3. Config/workspace/capability anchors substantially check out. |
| Proposed repository location and real job connection point | FAILING | 01:72–84,145–162; separate crate is reasonable, but unsafe generic award hook, absent authority mapping, attachment and restart boundary; F1–F3. |
| Manifest schema sketch, narrowing layers and seven field types | PASSING | 02-manifest-schema.md:8–30,71–119. No invented existing loader. Party-sharing ambiguity must be removed under F6. This is a sketch, not a schema implementation. |
| Complete reviewed command-policy/source-to-sink contract | FAILING | 03-command-policy-mapping.md:7–96 is strong; example effect maxima and walk A handle/resource enforcement remain underspecified; F5. |
| Token/grant/custody/lifecycle contract connected to real source | FAILING | 04-token-grant-contract.md preserves signature/record/authority/expiry/budgets, but its concrete open/close and reconciliation proposal is incomplete; F1–F2. |
| File-processing CLI paper walk through mapping/custody/grant/checker | FAILING | 05-walk-a-file-processing-cli.md explicitly labels archetype assumptions; acceptable subject for paper stage. The walk does not close its handle/grant or effect-bound reasoning; F5. |
| Tenant-aware HTTP contrast through the same contracts | PASSING | 06-walk-b-tenant-aware-http.md:26–105,109–125 identifies nested shape, batch, selector, redirect and pagination hazards and defers support. Correct overstatements in gap descriptions under F6; no HTTP implementation required now. |
| Intended test entrypoints, dependency vocabulary and evidence layout | FAILING | 07-test-entrypoints-and-evidence.md:27–87,102–134 clearly marks proposed commands and retains negative controls, but vendor requests cannot prove zero child invocations; F4. Add integration crash/open cases under F1–F2. |
| Named gaps and explicit unsupported/deferred cases | DEVIATED | 08-gaps-and-unsupported.md:73–92 silently turns walk A's single-file output into a universal initial-release eligibility restriction; F6. G-2/G-6/G-7 also need source corrections, not implementation. |
| Authorized custom CLI/Linux Docker demo kept separate from REAL-tool gate | PASSING | 00:51–71; 07:136–166; 08:109–117. No reopening tool/platform selection. Contrasting real-tool gate retains full independent/frozen-kit requirements. |
| No runtime changes / no implicit advance | PASSING | Nine additions only; 00 scope fence and 07 proposed status. Runtime implementation is NOT IMPLEMENTED, as required at this stage, and is not itself a blocker. |

## Blocking corrections

### F1 — Owned award, authority binding, and replay behavior are not specified

**FAILING.** 01:78 and 04:84–85 propose opening immediately after `record_award`; 08:102 repeats it. Maxie's warning is independently confirmed and understated if interpreted as merely checking an award row.

B `store.rs:1584–1629` explicitly says award rows include somebody ELSE's win. `NoClaim` records an award without creating a job; `Duplicate` returns before checking a claim; `New` is an insertion result, not cryptographic proof of selection. The real caller validates buyer against the recorded offer and the accepted claim against the published local claim: `run.rs:6412–6426`. Only the New arm dispatches fresh execution (`6453–6468`). Suppression intentionally invokes the same store method (`6285–6317`), and ACCEPT can bind an award while explicitly NOT executing (`6150–6183`). A blanket store hook therefore joins paths with different authority and lifecycle meaning.

The documents also require party/service/grant equality against an authoritative record without locating their new durable representation. Existing jobs have job/offer/agent/state/delivery fields, not a service/grant record (`store.rs:910–929`). Saying the manifest surface is new is not enough to specify who supplies and authorizes this binding.

**Minimal repair:** name the trusted adapter at the authenticated owned-award/eligible execution boundary, not every store call. Require positive local accepted-claim/buyer proof and eligible durable job state, plus seller-approved service/party/resources/ceilings. No grant on `NoClaim`, error, unreadable authority, suppression or unvalidated award. Define New/Duplicate/ACCEPT/resume treatment explicitly. A duplicate must not mint a fresh authority scope, reset counters, extend deadlines or reopen a tombstone; interrupted opening may only recover the same committed authorization idempotently after revalidation. Specify the durable job-to-holder/service/grant binding and its seller-controlled source, without deriving it from buyer prose. Add paper cases for others' award, losing local claim, duplicate, ACCEPT-only, crash between award and grant persistence, and closed-job replay.

### F2 — Timeout description is false; close/restart atomicity is only asserted

**FAILING.** 01:62–65,81–84 and 04:119–122 claim timeout does not pass through a store state. There is no distinct Timeout enum, but runtime errors from the deadline-bound agent go through `fail_job_with_feedback` (`run.rs:7100–7124`), which calls `fail_job` (`8410–8420`), and `store.rs:1780–1787` persists Failed. The actual danger is that the fail write is best-effort/log-and-continue (`run.rs:8376–8391`), not that no failure transition exists.

Restart re-drives Awarded/Executing (`run.rs:4597–4634`); graceful shutdown deliberately leaves those rows for replay (`4721–4727`). Resume may run the agent again and treats missing deadline information as live (`1541–1580`). These marketplace semantics must not implicitly reopen holder grants or replay an uncertain vendor write. `is_finished()` is a predicate (`store.rs:738–743`), not an event delivery/atomic-close facility. 04:109–117 has the correct goal but no commit point, durable close reason, reconciliation ownership or failure behavior. Process termination and filesystem removal cannot literally share a SQLite transaction.

**Minimal repair:** specify a monotonic durable holder admission state with explicit close reason (success/failure/cancel/timeout/expiry as applicable), an atomic deny-new-calls transition ordered with reservations, and idempotent cleanup/revocation thereafter. Prefer this holder record over gratuitously adding marketplace job states. Name its adapter call sites for success, all failure/timeout paths, cancellation/shutdown, natural expiry and boot reconciliation. Explain independent holder supervision/deadline enforcement when the marketplace process disappears. If authority or persistence is uncertain, no token/call/renewal may be served. Define handling of outstanding reservations and uncertain vendor effects without auto-replay. Add pre/post-close-commit crash cases, failure-to-persist, lost/unreadable records, restart with active descendants, natural expiry and repeated cleanup; require denied admissions and preserved counters/tombstones, with the planned bounded cleanup oracle. Actual crash execution is a later-stage gate; the coherent proof obligation and protocol are current contract work.

### F3 — Survey misses existing mediation and the actual MCP attachment gap

**FAILING.** 01:139–143 and 04:28–31 describe the existing session as host-held and injected into the container. B `seller_exec.rs:2575–2619` instead starts a per-job host credential proxy and passes placeholders/redirects, expressly refusing real-credential fallback. Codex registration passes the real access token/account only to the proxy, and the container auth request gets placeholders (`3060–3069,3136–3167`). `credential_proxy.rs:227–259` provides per-job credential structures; its actual Drop implementation aborts owned listener/connection tasks (`1019–1037`). `sandbox_netns.rs:63–70,356–363` has network namespace holder/ownership records. Thus G-6's “no ... token ... holder concept exists in any form” is false. These are NOT the approved persistent tool holder or durable service/resource grant, but their existence matters to a required credential-proxy integration survey.

The ACP type really is name+argv (`driver/acp.rs:49–59`), but the seller execution path currently supplies `mcp_servers: Vec::new()` (`seller_exec.rs:2408–2413`). A separate crate alone will never attach the holder to a job. “Separate crate makes no runtime changes to existing behaviour checkable” (01:158–159) must not imply stage 2 needs no integration edits. Existing env forwarding includes built-ins plus operator additions (`seller_exec.rs:907–936`), not the new empty-by-default reviewed child environment; extra mounts also exist (`851–854`). Reuse is not security equivalence.

**Minimal repair:** correct the secret/placeholder flow and scope negative claims to the missing reusable tool-grant contract. Survey `credential_proxy`, network holder/reaper, job container cleanup and the live seller ACP call site. Keep `maxplayer-tool-kit` as a reasonable proposed isolated policy/holder component, but name the small core-side authorization/lifecycle/transport adapters and dependency direction. Describe job-specific MCP attachment/token delivery without exposing persistent credentials, and state what remains absent. Do not claim an arbitrary HTTP tool is supported because model credential mediation exists. Source inspection, not prototype execution, closes this paper gap.

### F4 — Zero vendor requests is not zero child execution

**FAILING.** 02:121–122 and 07:44–46 explicitly substitute a vendor-owned request counter for the required “zero child calls”; check 1 at 07:55 changes the oracle to zero vendor calls. A malformed manifest could launch a child that reads a local file, writes output, errors or exits before networking. The vendor counter remains zero and the proposed oracle passes. This is success-shaped emptiness in the checker contract, not a demand to run tests now.

**Minimal repair:** require an independent process-launch/fixture-child invocation observation in addition to the vendor counter and forbidden-effect markers. For each reject-before-invocation check, require validation error AND zero child starts AND zero vendor effects. Add a defective runtime that launches a child then rejects without contacting the vendor; it must fail the pre-invocation oracle. Keep the independent vendor counter for actual remote effects. Do not blanket-skip an independently observable cleanup check merely because a closed-token mutant fails auth; name genuine dependency relationships (07:84 currently makes cleanup a dependent skip).

### F5 — Walk A does not connect handles/resources or justify maximum effects

**FAILING.** 05:107–110 says `res:R` bounds requests although it has no argv sink; its per-call list at 130–132 omits resource membership and never specifies how an uploaded handle/destination slot is bound to J/P/H or res:R. Generic forged-handle and filesystem-isolation cases do not establish that a valid K-owned handle cannot be presented through J's otherwise valid token. The missing relation cannot be supplied by an arbitrary unused `res:R` string.

The walk declares 8,192 maximum bytes (05:57,134–135), but its mapping says quality 1..100 “bounds output size” (94) without any input-size-to-output bound, child output enforcement, network-call/retry bound or failure oracle. A quality setting alone supplies no such maximum. Likewise 03:123 allows 1..50 pages but 135 fixes the effect at 20; 02:58–60 allows only 3 items while its example binds page_limit 20. If these are deliberately inadmissible examples, say so; they are currently presented as ordinary worked mappings.

**Minimal repair:** specify holder-issued handle/slot ownership and immutable job/party binding, the exact admission membership check and a valid-cross-job-handle negative case. Bound uploaded bytes and each supported operation's calls/items/bytes using declared profile maxima and enforceable tool/runtime controls; uncertain or unbounded effects reject before execution. Make the positive example's page/item ceilings consistent and state whether bytes mean input, output, network or separate counters. Do not retrofit a nonexistent resource flag. Since the CLI is an archetype, explicitly list these semantics as required assumptions and use conditional classification until verified, rather than claiming only A5 remains conditional (05:153).

### F6 — Unsupported eligibility is quietly narrowed; gap register overstates omissions

**DEVIATED.** 08:86–87 makes single-file output mandatory for **every** initial-release profile. Plan §3 requires private checked output slots, not single-file-only tools. It is reasonable for walk A or the disposable demo to use one file (05:158–159); silently applying that to the frozen-kit unseen-tool adjudication narrows the approved acceptance scope. Remove the global condition or explicitly seek a plan-scope amendment; keep it as this example's chosen profile limit.

Also resolve 02:40,64 and 04:145–149: “separate holders for different parties and vendors” must not be weakened by an unexplained `shared-holder` option and P/Q opener list. Define sharing as same-party/vendor concurrent jobs, or represent distinct per-party holders explicitly. No cross-party sharing authority may be inferred from that enum.

Correct 08:C-1/C-4/C-7: the approved plan already permits schema-defined HTTP fields, requires nested/body/query/redirect/destination constraints (§3), and mandates profile-version review. 03:60–67 already covers uncontrolled callbacks/destinations and 03:20–21 already requires new version/review. The missing pieces are concrete deferred HTTP schemas/checkers and implementation enforcement, not permission to weaken those existing requirements. HTTP remains deferred. Separate unsupported, deferred and manual-setup labels even though 08's section C umbrella calls them all unsupported. Finally, 08:96–101 must not reintroduce first-real-tool selection as a blocker to the already-authorized synthetic CLI/Linux Docker demo; reserve real-tool selection/release-platform questions for their proper later gate.

## Plan §5 check-by-check paper disposition

The stage-0 check map covers all twelve headings and its positive, negative, concurrency and expiry cases are largely faithful. This table separates specification from execution: **all actual fixture/live executions are NOT IMPLEMENTED at H, appropriately deferred**.

| Plan check | Paper grade | Assessment |
|---|---|---|
| 5.1 schema/profile | FAILING | Cases retained; no-child oracle weakened to vendor counter (F4). |
| 5.2 discovery/result | PASSING | Independent expected bytes/digest and exact approved verb list, 07:56. |
| 5.3 operand grammar | FAILING | Grammar/positive literal retained, but zero child execution is not observed (F4). |
| 5.4 artifact consumption | PASSING | Forged handles, staging/after-validation races, original digest and outside-read marker, 07:58; add valid foreign-handle authorization under F5. |
| 5.5 authentication | PASSING | Wrong claims, holder, time, close outcomes, restart/renewal and positive control, 07:59. F1–F2 add integration-specific cases, not replacement tests. |
| 5.6 grant authority | FAILING | Existing negative list retained at 07:60, but owned-award opening, duplicate/recovery and handle binding are missing concrete oracles (F1/F5). |
| 5.7 leakage | PASSING | Auth sink positive control, separate non-auth canary, all captured channels, both success/failure, 07:61,68–69. |
| 5.8 isolation | PASSING | Concurrent and sequential markers, independent child mounts/identities and successful credential lookup, 07:62. |
| 5.9 cleanup | PASSING | Descendant, close/cancel/timeout and natural expiry, 5-second bound and unrelated K control, 07:63. F2 must connect the protocol to crash/restart; F4 removes an unjustified blanket skip. |
| 5.10 budgets | PASSING | Independent ceilings, boundary/concurrency and durable renewal/restart semantics, 07:64. Example arithmetic/enforcement remains F5. |
| 5.11 lifecycle | PASSING | Unhealthy, one notice, pause, new authorization after health and no write replay, 07:65. |
| 5.12 registration | PASSING | Rejection/pause and pinned profile/version drift, 07:66. |

## Preserved later gates and anti-gaming checks

- Stage 1: authorized custom CLI plus fake authenticated service, Linux Docker, synthetic credentials. Selection is settled for that mechanism demo. Plan bound remains one worker, two working days or 80 turns, at most two attempts. Maxie owns fixes and the follow-on brief. This verdict does not demand independent real-tool acceptance from the demo.
- Stage 2: reusable kit, full fixture suite, lifecycle, router skill and enrollment/recovery instructions before freeze; pinned server/schema/profiles/checker/examples/skill and acceptance reset on changes. Actual skill publication remains its separate mechanism. No skill/code/test execution performed in this review.
- Real acceptance remains two services on real tool one and a successful contrasting tool-two service selected by Petar/Josip after freeze; independent prior eligibility/oracle/effect allowance; haiku-class run limited to manifest/input edits, 40 turns/two hours and zero human interventions. Correct unsupported routing does not satisfy the second-tool success gate. Synthetic evidence contributes zero real-tool credit. 00:67–71 and 07:136–166 preserve this honestly.
- Proxy/HTTP, host-executor, browser and platform credential custody remain deferred; live writes require separate explicit disposable-resource/effect allowance. Black-box live results do not prove local isolation. No runtime guarantees or release approval are inferred.
- No tests edited to pass, deliverables renamed away, runtime files hidden in docs or fabricated executed checks found in the nine-file delta. Real narrowing found: universal single-file criterion (F6). Oracle weakening found: vendor counter substituted for process invocation (F4). Survey's broad NOT FOUND claims did not survive independent base-source inspection (F3). Archetypes are clearly labeled, which is appropriate for stage 0, not evidence of real-tool compatibility.

## Minimal next-round scope

Revise only the contract documents: (1) owned/authorized/idempotent grant-open adapter and durable binding; (2) holder close/reconciliation protocol grounded in actual failure/resume paths; (3) corrected proxy/MCP integration survey and separate-crate adapter boundary; (4) independent no-child oracle; (5) handle binding and consistent enforceable example budgets; (6) remove silent single-file narrowing and resolve party/deferred/stage labels. No implementation is requested by this verdict. Next review should be focused on the diff from H and F1–F6, not a full regrade, unless explicitly reordered with reason.
