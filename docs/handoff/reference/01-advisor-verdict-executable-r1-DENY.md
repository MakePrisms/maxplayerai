<!-- PROVENANCE: copied verbatim into this repository for handoff on 2026-09-10.
     Original lived on the authoring machine outside this repo and is NOT reachable to you.
     Absolute /Users/... paths appearing below are historical references from the time of
     writing; the in-repo copies under docs/handoff/reference/ are now authoritative.
     Content is unmodified apart from this header. Scanned for credentials: none found. -->

# DENY — seller-tool executable prototype, first full review

## Identity and decision

Repository: MakePrisms/maxplayerai.
H: `7d4286b907661e8b2e9cefbb04db6f716b0d8a5b`.
B: `b378de521648bebcb604987e71bed4849190a955`.
Merge base is B. Complete delta: 62 files, +3847/-1.
Ordering lane: `agent:maxie:discord:channel:1546802028091670630`.
Governing scope: `/Users/forge/forge/v2/maxie/runs/seller-tool-scope-correction-20260909.md`, 2631 bytes, SHA256 `683da09559bfc12631062c84ecdf4080778c4b31a3e7c16b3a49a7aa89dc64c3`, read whole.

**DENY executable acceptance at H.** Seller-level architecture is substantially implemented, but credential absence is falsely certified, privileged file use is raceable, the Docker stop check is confounded by a prior restart, and Docker evidence is not reproducibly source/image-bound. Correct these bounded prototype defects and contradictory documentation. Do not restore withdrawn award/grant gates or demand production integration to approve the explicitly labeled prototype.

No delivered code, script, import, parser, build, test or container was executed by advisor. Full H/B objects were fetched into an advisor-owned bare repository from the worker and independently from upstream GitHub. No source/shared checkout changes. Parent owns all sends, Discord/fallback and next fix loop. This is a source verdict, not an execution attestation.

## Complete governing executable rubric

All citations are at H. `K/` = `crates/maxplayer-tool-kit/`; unqualified Rust binary names below are under `K/src/bin/`. PASSING means bounded source implementation/test design, not advisor-run acceptance.

| Requirement | Disposition | Evidence and limit |
|---|---|---|
| Seller-level offering and shared operation list | PASSING | `K/src/config.rs:7-50`, `tool_holderd.rs:186-195,256-287`: one config serves all job endpoints; fixture compares full JSON. Docker comparison needs F3 correction. |
| No award/payment/job-grant/replay/marketplace-state gates | PASSING | Entire added runtime has none. Attach/detach addresses directories/sockets only (`tool_holderd.rs:397-500`). Withdrawn gates are superseded, not missing. |
| One enrollment across two sequential jobs | PASSING | `tool_holderd.rs:81-111` checks persisted session once; detach never logs out; `K/docker/demo.sh:157-237` runs A then B and checks vendor login=1/transforms=2. Maxie's fixture test is green; Docker run remains attributed. |
| Auth persistence and re-enrollment | PASSING | `vendor_cli.rs:117-146`, holder startup, and control-only `tool_holderd.rs:503-529`. Fixture includes successful post-restart operation. Synthetic only. |
| Credentials outside buyer containers | FAILING | Intended private home/env clearing exists, but F1 injects secret into job-shaped container; F2 breaks privileged file boundary. |
| No arbitrary shell/argv passthrough | PASSING | `K/src/validate.rs:95-151` builds argv in trusted spec order; `tool_holderd.rs:326-337` fixed program/subcommand, cleared env, no shell. Seller-authored config remains trusted. |
| Invalid operations/parameters rejected | PASSING | Unknown/missing/non-string inputs, text/choices and reserved directory selectors checked (`validate.rs:95-167`, `tool_holderd.rs:300-309`). Not credit for race-safe file access. |
| Confined file access/cross-job-file rejection | FAILING | Static absolute/traversal/outside-symlink cases work; F2 defeats consumption-time confinement. |
| Only seller-authorized clients reach endpoints | PASSING | Private directories/0600 sockets (`tool_holderd.rs:72-79,575-596`), separate job socket volumes and `--network none` (`demo.sh:131-168`) implement the intended Linux mount boundary. Adversarial foreign-client coverage is NOT IMPLEMENTED; same-uid host isolation/public authentication not claimed. |
| Seller control distinct from job authority | PASSING | Attach/detach/re-enroll/shutdown require control context (`tool_holderd.rs:231-249`); job identity comes from listener, not input. Operational status/health still exposed to job sockets; residual limit below. |
| Tool startup/stop tied to daemon lifecycle | FAILING | Holder is the prototype daemon stand-in. It enrolls/serves at process lifetime, but Docker causal stop/start proof fails F3. Actual seller-daemon ownership NOT IMPLEMENTED. |
| Visible unhealthy state on auth failure | PASSING | `vendor_cli.rs:149-167,212-218`; `tool_holderd.rs:344-349,532-559`; `holderctl.rs:44-50`. Explicit probe/auth-failed calls expose unhealthy state; status is cached, not a periodic-health promise. |
| Cleanup and supervision | NOT IMPLEMENTED | Docker contains prototype processes; explicit shutdown unlinks sockets (`tool_holderd.rs:237-249,563-572`). Native child cancellation/reaping, signal cleanup and actual Maxplayer supervision are not implemented. Do not infer all child work stopped from socket disappearance. |
| Custom CLI/fake authenticated service/Linux Docker/synthetic selection | PASSING | Five binaries, config, Dockerfile and harness exist; vendor authenticates transform (`vendor_service.rs:133-175`). No real-account acceptance. Docker execution is not advisor-verified. |
| Real container MCP connection OR explicitly labeled stand-in | PASSING | `demo.sh:163-178,221-226` uses container stdio bridge/socket; `tool_mcp_bridge.rs:11-14` explicitly disclaims real seller wiring. Scripted MCP client, not a production job agent. |
| Independent checker, functioning negative controls, exact evidence | FAILING | F1/F3 success-shaped checks; F4 missing raw exchanges/pins. Separate fake-vendor counters are useful mechanism observations, not independent real-tool acceptance. |
| Reviewed image reproducibility | FAILING | F4 mutable images/unlocked crate-only build; Mac log does not bind Linux image. |
| Concise scope correction and truthful docs | DEVIATED | Seven docs annotate withdrawn scope, but explicit retention of Parts II onward contradicts it (F5); custody/host-only claims also contradicted by F1/F2. |
| Independent real-tool acceptance | NOT IMPLEMENTED | Explicitly disclaimed and not required for this synthetic stage. |
| Production/full-kit/ready PR | NOT IMPLEMENTED | `crates/maxplayer-core/src/seller_exec.rs:2408-2413` still empty MCP list. Head-associated PR query returned `[]`; bounded publication observation only. Follow-on below. |

## Blockers and minimal corrections

### F1 — Absence checker introduces credential and converts errors to absence

**FAILING; blocking.** `K/docker/demo.sh:193-201` passes the synthetic enrollment secret via `-e NEEDLE="$SECRET"`. That places it in a job-shaped container's environment and the host Docker command arguments. Grepping selected files cannot prove it was absent from those surfaces. At 198 both no-match and grep failure emit `NOT_FOUND`; outer `|| true` suppresses failure too. Token scan 189-192 has the same error-to-absence problem. The holder-state check at 202 accepts any “No such file” from a combined listing; absence of `/run/secrets` can satisfy it while `/var/lib/holder` exists as an image directory. Maxie's reported defect is independently confirmed from source.

Fix: never send either enrollment secret or stored session token to container-side scan argv/env. Capture the identified actual job's visible filesystem/mounts, relevant process argv/environment, protocol/output/log surfaces into host-owned evidence and scan host-side with host-held patterns. Record safe inventory/counts/hashes, not credential values in reports. Missing captures, unreadable surfaces, unexpected empty inputs and scanner errors must not pass. Add a functioning negative control: insert a synthetic sentinel into a COPY of host-captured evidence, never into the buyer container, and prove the same checker fails; also prove scanner/capture errors fail. Cover enrollment and persisted session separately. Preserve and supersede old evidence.

The host Rust fixture also silently substitutes empty bytes/skips read failures (`K/tests/fixture_suite.rs:196,213-214`) and does not isolate its process identity. It proves ordinary placement/modes, not hostile-container unreadability. Repair its absence oracle rather than calling that test a substitute for Docker custody evidence.

### F2 — Checked path is reopened later with holder authority

**FAILING; blocking custody/file isolation.** `K/src/validate.rs:200-230` checks/canonicalizes paths and returns pathname strings. `tool_holderd.rs:320-337` later spawns the privileged CLI using them. `vendor_cli.rs:196` reads input and `:225` independently opens/truncates output. Buyer work volumes remain writable (`demo.sh:137-138,165-167`). A job can replace a checked file or parent after validation with a symlink; the later open resolves inside the holder namespace, which includes private state and other jobs. An input substitution can route outside contents through the allowed reversible transform; an output substitution can write outside the job. This is a static source finding, not an executed exploit. Canonical strings do not pin objects.

Fix: confine access at consumption using race-safe directory-relative/no-follow opens, stable handles and protected staging for the trusted CLI, or an equivalently effective child filesystem boundary. Output publication must also resist buyer pathname replacement. Add deterministic input/output/parent replacement controls at the check/use boundary; assert outside sentinels unchanged/unread and valid calls still succeed. Existing static symlink tests do not discharge this. No award or per-job entitlement changes are needed.

### F3 — Docker lifecycle negative tests an already-disconnected endpoint; list check is partial

**FAILING; blocking demonstration.** `demo.sh:242` restarts holder, whose startup creates an empty jobs map (`tool_holderd.rs:125`) and only a control listener. Job listeners require attach (`:423-459`). The script never reattaches B before stopping at `demo.sh:271` and probing B's old endpoint at 273-276. Thus expected failure can happen while holder is running. Committed post-restart status reports no attached jobs. Final start at 278-284 checks login count, not restored job success. Rust fixture 140-163 has a stronger before/after structure, but cannot repair this Docker evidence.

Fix: after restart, re-establish seller-side addressing, prove a successful call on the exact endpoint immediately before stop, prove its loss after stop, then start/reattach and prove success without a new login. Retain identities/responses. A still-running-successful endpoint must fail the stop checker. Observe busy children separately if claiming child-stop supervision.

Also `demo.sh:229-231` compares a regex prefix ending at the first `]` (inside schema's enum), not the complete tool list. Compare parsed full tool-list responses with expected nonempty operation/schema content; mutate a suffix field as negative control. The Rust fixture's full JSON equality (`fixture_suite.rs:70-78`) is good separate coverage.

### F4 — Unpinned image and incomplete retained raw evidence

**FAILING; blocking exact Docker acceptance; not proof of malicious substitution.** `K/docker/Dockerfile:11-17,24` uses mutable rust/debian tags and unlocked `cargo build --release`; crate-only context copies no workspace lockfile. `demo.sh:23,290-320` trusts a preexisting mutable image tag and records tag/platform/version/counters, not source/tree, loaded image digest, dependency lock or capture hashes. Maxie's Mac workspace run does not identify that Linux build.

Both 17-file committed evidence trees omit generated `job-a-mcp.jsonl`, `job-b-mcp.jsonl`, `job-a-crossjob.jsonl`, `job-b-after-stop.jsonl` used at `demo.sh:171,205,221,273`; `.gitignore:18` ignores `*.jsonl`. Summary PASS lines do not preserve those observations.

Fix: retain source-to-build receipt (including build-ancestor/source-equivalence proof if applicable), pin builder/runtime images by digest, retain appropriate lockfile and build locked, record actual loaded image ID/digest. Preserve sanitized raw responses/checker inputs, output identities, versions and hashes; explicitly include ignored captures or package them elsewhere. Maxie reruns the repaired Docker gate; advisor does not execute it.

### F5 — Local supersession notes preserve contradictory grant obligations

**DEVIATED; concise documentation correction required.** `docs/specs/seller-tool-onboarding/04-token-grant-contract.md:3,23-25` says Parts II onward remain live/implemented, but :149-153,188-198 require admission/deadline/marketplace reconciliation; :220-230 requires job grant verification; :290-295 per-job durable budgets; :299-302 permits only newly authorized jobs after re-enrollment. These contradict governing seller-daemon scope and same-job recovery. The global withdrawal helps but does not make “Parts II onward stand” accurate. Fix retention notes to preserve custody/file/lifecycle requirements ONLY, not obsolete grant/state gates. Do NOT implement withdrawn requirements to fix prose.

Also correct `demo.sh:306`/committed manifests' host-only/never-command-line claim (F1), `04:23-28` implemented containment claim (F2), and fixtures README's nonexistent `scripts/demo.sh` reference. Test credential generation wording must acknowledge the fixed synthetic fixture without reproducing its value. No evidence of a live credential is asserted.

## Prototype boundary and exact production follow-on

Selection remains custom CLI + fake service + Linux Docker + synthetic credentials. Stand-in is explicitly allowed. Missing production wiring is not an extra prototype blocker; it precludes full-kit/production-ready approval.

1. Own configured enrollment/holder handle from actual seller daemon boot to stop: `seller_node/run.rs:3982-4010,4710-4727`, existing signal seam `seller_node/shutdown.rs:136-179`. Keep session alive between jobs; expose failed start/auth/health and supervise children/cleanup. No marketplace-state/award changes.
2. Wire seller-created job addressing into real container launch (`seller_exec.rs:2386-2394,795-854`): mount only that job's endpoint/work area and install bridge, never credentials/private state/control/other jobs. Verify real non-root uid/socket access; demo runs root containers.
3. Populate `SessionConfig.mcp_servers` at `seller_exec.rs:2408-2413` with existing `driver/acp.rs:49-59` `McpServer { name, command }`; verify a supported real job agent initializes/lists/calls in its container. Preserve existing egress/environment/evidence rules; do not simply attach host paths to an agent config.
4. Demonstrate two actual sequential jobs sharing seller list/session, custody/invalid-input/file controls and daemon lifecycle. Independent real-tool acceptance remains a separate explicit stage, not implied by this fake CLI/vendor.
5. Supply actual reviewed ready PR link/head/base/checks before final readiness claim. Bind later publication to frozen verdict or focused replacement. No account, spend, merge, tag or push is authorized here.

## Residual limits, not new broad scope

- Local authority relies on seller uid and isolated mounts, not public protocol authentication. Foreign-client/management-denial adversarial coverage absent; do not claim it tested.
- `tool_holderd.rs:203-227` exposes all attached-job metadata through job-facing status, despite :379-380's layout-disclosure caution. Restrict seller-only status before claiming least disclosure; this alone is not direct file-content access.
- Existing connections retain only job-id strings (`:155-164,312-315,450-454`). Reusing a detached ID with a new root can redirect an old connection. Bind connection to original attachment or prohibit reuse while it remains active. This is addressing hygiene, not an award replay gate.
- Output ceiling is checked after child completion (`:356-369`), not bounded stdout/memory/total resource enforcement. No general authorized-operation abuse protection follows.
- Child work is not cancelled merely because bridge times out/native holder exits. Docker process containment differs from native supervision; production follow-on must own this.
- Zero fake-vendor transforms means zero successful transforms, not zero child starts/requests/local effects (`vendor_service.rs:158-174`). New negative tests use useful exact Reject variants and positive controls, but no independent exec observer or defective-runtime mutants. Do not elevate them into those stronger claims; no withdrawn award oracle is required.

## Evidence attribution, ownership, publication and coverage

Maxie log `/Users/forge/forge/v2/maxie/runs/tool-7d4286b-tests.log`, retained as `parent-tests.log`: 3881 bytes, SHA256 `9e36ac6942b220a4c6c0813a6cb367c12e12c400deb3f69fa1d4c62a653e946b`. Read whole: 6 fixture + 26 negative tests PASS; unit/doc suites zero. Maxie-run evidence, not advisor execution; log names detached checkout but embeds no build hash. Worker Docker 27/27 remains attributed mechanism evidence. Maxie withheld Docker gate; advisor independently confirms why.

`c4d02ba8169eb06acdf89d9110e6fe9d9e3fc0e8` is in H ancestry. Full delta is only eight Cargo.lock lines adding this crate and serde/serde_json, consistent with manifest; no runtime/Dockerfile change despite broader message. No unrelated implementation contamination found in this bounded commit. Worker-named author metadata cannot prove exclusive custody or resolve the other worker's citation. Preserve all work; no ownership-motivated amendment/deletion justified.

Independent upstream full-ID fetch succeeded for H/B. H tree `fe2f9193f967243205c1275c081fdce4b7a95b22`; B tree `c75c87e5fca50c4378db112ae225d7f18ebb4c7b`. Head-associated GitHub PR endpoint returned `[]` at 2026-09-09T22:10:03Z: not a repository-wide no-PR claim, branch/main/CI attestation or publication approval. Review is frozen to H/B, not mutable worker head.

Evidence directory `/Users/forge/forge/v2/advisor/scratch/seller-exec-7d4286b/`; final `manifest.tsv` covers evidence files/bare Git objects and excludes itself/identity receipts. Complete raw delta is retained; credential-sensitive material must be inspected with safe omission/redaction, never published wholesale.

All 62 files accounted for. Main reviewer inspected full runtime, Docker/demo/fixtures/Cargo, all seven changed doc diffs, ancestry and targeted unchanged integration source. Read-only subsidiary inspected all three test files, seven changed docs and both complete 17-file evidence trees; report `test-doc-review.md` (12687 bytes, SHA256 `34188a6b685e3c5a54a8e5e7902a034c67a4e884f3f07c7aa82111374292dcfd`). Parent verified load-bearing extra findings against source. Sensitive evidence fields were omitted before display; no claim of an exhaustive credential-value leak audit.

Tests are new at B: no existing gate tests edited/deleted. Findings concern flawed new oracles/overclaims, not inferred intent. Stage-0 paper verdict is superseded scope history, not executable approval. Later reviews are FOCUSED: diff since H plus F1–F5/affected rubric. No payment/merge/full-kit/real-tool approval.
