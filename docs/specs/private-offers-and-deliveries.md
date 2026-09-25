# Private offers and deliveries — implementation proposal

**Status:** reviewed design spec; [implementation in progress](private-offers-implementation.md), not enabled or deployed. **Anchor commit: `135e4ea0bd5330f7ab0272d501aa83a718edc777`** (= `origin/main` at writing, 23 September 2026).

This is a reviewable starting specification, not approval to deploy. Normative language below describes the proposed implementation. Settled product requirements are in §0; engineering defaults still open to adjustment are in §11. This document supersedes conflicting implementation suggestions in [the original discussion draft, PR #1023](https://github.com/MakePrisms/maxplayerai/pull/1023), not the decisions subsequently recorded there.

[Visibility flow diagram](private-offers-flow/README.md) — updated for the lifecycle-preserving scope below.

## 0. Settled inputs — not relitigated here

- Bob, original discussion continued in [private-offers-and-deliveries](https://discord.com/channels/1549984743666356239/1551352631887142984), 21–22 September: privacy from other marketplace users, not from Maxplayer; lifecycle and approved metadata remain public; explicit public jobs remain available; default configuration is private.
- Petar, [22 September content decision](https://discord.com/channels/1549984743666356239/1551929287270203404/1551966060775739444): recipient-encrypted private text using NIP-44 and the NIP-17/NIP-59 envelope pattern; a Maxplayer service recipient copy for every private job-content message. Payment secrets do **not** acquire additional recipients.
- Petar, [22 September file decision](https://discord.com/channels/1549984743666356239/1551929287270203404/1551965351590502527): Git for attachments and deliveries; private per-job repositories readable by buyer, seller and Maxplayer. No separate blob service in this release.
- Petar, parent-channel discussion, 23 September: coordinated breaking upgrade is acceptable; do not maintain parallel legacy implementations. Key/recovery and retention redesign, external-resource permission management, seller memory, and unused episode capture/emission are outside this effort.
- Petar, [23 September open-pool decision](https://discord.com/channels/1549984743666356239/1552236596768546828/1552237747345817641): publish the initial task for discovery; after seller selection, subsequent content and delivery use the private flow. The initial task stays public. Secret-task open-pool matching is deferred.
- Petar, [23 September scope adjustment, 13:06 UTC](https://discord.com/channels/1549984743666356239/1552254447013597214/1552305210293223466): preserve the existing lifecycle; no post-award buyer-input handoff or mandatory Maxplayer content-ACK/start gate. Open-pool tasks must be executable as offered; targeted private inputs are available before claim. Necessary privacy wire/authorization changes remain in scope. See [the recorded adjustment](private-offers-decisions/lifecycle-scope-adjustment.md). This narrows the initial open-pool implementation, not the content-privacy requirement.
- Petar, [this implementation-spec request](https://discord.com/channels/1549984743666356239/1552254447013597214/1552254450591207515), 23 September: propose a concrete starting specification.

The content/file decision records are on [the existing proposal branch](https://github.com/maxie-agent/maxplayerai/tree/1ac3fbb3f27b89fe6dd17936990af53afcf22fb8/docs/proposals/private-offers). Interpret earlier recommendations through those decisions.

### 0.1 Decision preservation and precedence

This PR includes **verbatim snapshots** of the [content-privacy decisions](private-offers-decisions/content-privacy-decisions.md) and [file-storage decision](private-offers-decisions/file-storage-decision.md), including their original discussion links and clarifications. [The coverage map](private-offers-decisions/README.md) maps their requirements and exclusions to this spec and its acceptance tests. These snapshots preserve the source at `1ac3fbb3f27b89fe6dd17936990af53afcf22fb8`; they are not rewritten summaries or a claim to archive every Discord message.

Settled decisions take precedence over proposed engineering details. Any later change must explicitly name the affected decision and record the human decision/link; it must not silently disappear during implementation or be reclassified as an optional default. Numeric limits and replacement-job policy remain proposals, not retroactive team decisions. The proposed mandatory service ACK was removed by the later scope adjustment; it is not an open initial-release requirement. Future source-note changes require an explicit reconciliation of the snapshot and coverage map.

## 1. User-visible contract

Use two independent inputs, not three competing meanings of “private”:

- `visibility: private | public`, default `private`.
- Existing optional `seller_pubkey`: set means targeted; absent means open pool.

Resulting behavior:

1. **Private + selected target:** task and content private from posting onward. The target must be able to read the complete task and validate/fetch all required private inputs before deciding to claim; targeting does not authorize execution.
2. **Private + open pool:** initial task is public. Label it **“Public task · private execution and delivery”** before posting. The complete task and all execution prerequisites must be available to candidates when they assess it. No confidential input upload or additional buyer handoff is supported after award in the initial release. Progress explanations, answers and deliveries are private after selection; execution starts from the already offered task.
3. **Public:** task, subsequent content and delivery are public, whether targeted or open pool.

CLI and MCP expose the same values and return `discovery_visibility`, `execution_visibility`, and the selected seller. MCP tool descriptions must explain that omitting `seller_pubkey` publishes the initial task even when `visibility=private`. No silent fallback from targeted-private to open-pool or public on error. A job's visibility cannot be changed in place; public re-publication requires a separate explicit job.

In the initial release, jobs requiring confidential buyer inputs use a targeted private offer. Open-pool tasks may reference already accessible public resources; their permissions remain externally managed. Do not introduce a new public attachment-upload mode or an award-time secret/credential handoff. Required inputs cannot be added or replaced after claim/award. Follow-up work requiring new instructions or inputs is a new offer, with those inputs available before its claim—not a new waiting state in the current job.

## 2. Reuse and code anchor

Use the existing Rust Nostr implementation and Git hosting stack. Do not build custom encryption or another file service. Reviewed primary standards: [NIP-17](https://github.com/nostr-protocol/nips/blob/master/17.md) and [NIP-59](https://github.com/nostr-protocol/nips/blob/master/59.md). They provide recipient envelopes, not marketplace authorization, transactional delivery, or a guarantee that Maxplayer received the same task.

Code citations below were re-derived at the anchor, not copied from the earlier review:

- `crates/maxplayer-core/src/gateway.rs:10 @135e4ea`: marketplace protocol major is currently `1`; `:361` serializes task into the public `i` tag; `:591` parses offers.
- `crates/maxplayer-core/src/kinds.rs:21 @135e4ea`: lifecycle kind registry, 3400–3407; reuse these for public coordination.
- `crates/maxplayer-core/src/payment_send.rs:260 @135e4ea`: existing NIP-44/seal/gift-wrap helper; `:181` and `:300` constrain outer timestamp randomization to under 180 seconds. Extract a transport primitive without sharing payment payload schemas or recipient policy.
- `Cargo.lock:2795 @135e4ea` resolves `nostr 0.44.4`; `:2874` resolves `nostr-sdk 0.44.1`. `crates/maxplayer-core/Cargo.toml:93 @135e4ea` enables `nip59` and `nip98`.
- Inspected installed `nostr-0.44.4/src/event/builder.rs:1480`: `gift_wrap_from_seal` generates an ephemeral key and randomized outer time. Do not assume its default timestamp is compatible with this relay; use the existing bounded-time transport construction.
- `crates/buzz/crates/buzz-relay/src/handlers/ingest.rs:241 @135e4ea`: persistent-kind allowlist includes gift wraps. `:1549` checks timestamp drift, `:1568` exempts gift-wrap outer identity from authenticated-key equality, and `:1578` begins the tighter NIP-17 size checks. Gift wraps use authenticated WebSockets, not HTTP event ingestion.
- `crates/buzz/crates/buzz-relay/src/api/git/transport.rs:107 @135e4ea`: `GitReadAuth`; `:116` uses the global public-read setting; `:869` starts upload-pack, before hydration at `:885`. Add job ACL authorization at this existing boundary; no cache-bypass vulnerability is asserted.
- `crates/buzz/crates/buzz-relay/src/api/git/policy.rs:204 @135e4ea`: push-policy hook; its existing ref-scoping checks only narrow permissions. Extend the underlying job role check, not just token scopes.
- `crates/buzz/crates/buzz-relay/src/api/git/manifest_event.rs:70 @135e4ea`: public repository-state emission. Private repos must not leak descriptive refs through this secondary path.
- `crates/maxplayer-core/src/job_lifecycle.rs:2259 @135e4ea`: current job hash includes task plaintext; `:1444` recomputes it during verification. Both producers and verifiers must move together.
- `web/network/js/parse.js:232 @135e4ea`: public task extraction. Public UI renders private placeholders, not ciphertext or guessed summaries.
- `docs/protocol-v1.md:383 @135e4ea`: lifecycle and verification/payment invariants; `:598` award, `:610` results. Preserve their ordering and security intent while introducing v2 content binding.

Issue/PR preflight found the existing privacy draft (#1023), not an already integrated private-job content lane. Existing payment envelopes are useful infrastructure, not proof this feature exists. No paid dependency or service is proposed.

## 3. Public wire contract — protocol v2

The [normative wire supplement](private-offers-wire-v2.md) defines the exact content-to-event mapping, closed per-kind private schemas, public-inline encoding and receipt preimage. Its precise definitions take precedence over shorthand summaries below. Main’s lifecycle authors/phases remain authoritative. No standalone follow-up/review message type is introduced.

Keep the existing lifecycle `offer → claim → award → execute → result → verify → accept → pay → receipt`. No new READY, input-handoff or service-ACK event/state is introduced. This does **not** mean unchanged wire compatibility: content references and private delivery binding still change.

Use marketplace `["v","2"]` on all new lifecycle events, including fully public jobs; this is independent of the ACP protocol version. Update every producer/parser and version fixture together. Unsupported/missing major on a newly admitted job fails closed after cutover.

Keep OFFER/CLAIM/AWARD/RESULT/FEEDBACK/ACCEPT/REJECT/RECEIPT kinds and their authors. No new top-level persistent kind is required. Add:

- OFFER: exactly one `["job", job_id]` (32 random bytes, lowercase hex), `["visibility","private"|"public"]`, and `["discovery","targeted"|"open"]`. Existing target identifies the seller for targeted jobs.
- All subsequent events: same job ID, root offer reference and existing claim/award/result references as appropriate. Execution events additionally bind exactly one award ID.
- Private content reference: exactly one `["content-id", message_id]` and `["content-commitment", commitment]` when an event has private explanatory content. IDs are 32 random bytes, not hashes of text.
- Private events have empty `content` and no task/title/free-form `i`, URL, filename, path, progress, error or summary tags. An open-pool OFFER alone carries the initial public task. An explicit public job uses the public content fields.
- Public private-job Git locators and branch identifiers, where required for delivery verification, use only opaque repo IDs and generated ref names. No user-supplied repository name or descriptive path enters a lifecycle event.

**Allowlist:** event kind/version/ID/signature/author; buyer/seller; job and lifecycle references; timestamps/deadlines; amount/currency/payment mode/mint; coarse status and enumerated reason; standardized capability/output codes; approved numeric usage; settlement signatures and delivery integrity identifiers. Every new field requires classification. Reject duplicate singleton tags, **every unlisted private-job tag/subkey**, invalid enum/lexical values and contradictory visibility/target combinations. The supplement specifies the only public fields and their exact shapes; arbitrary model/preset labels are private details, not approved public metadata. Never echo untrusted input in public error strings.

Private fields include requirements, task titles, acceptance criteria, follow-ups, answers, review findings, rejection explanations, descriptive URLs, attachment manifests, filenames and Git content/history. Classifier findings use the same private transport; this spec does not add a classifier product or a public verdict format.

## 4. Recipient-encrypted content records

### 4.1 Exact record and binding

Use kind-14 rumors sealed in kind 13 and separately wrapped in kind 1059 for each unique recipient. Only wrappers are published. Inner JSON `schema="maxplayer.content.v2"` is a separate dispatch domain from payment messages.

Illustrative plaintext body (placeholders are not valid hashes):

```json
{
  "schema": "maxplayer.content.v2",
  "job_id": "<64 lowercase hex>",
  "offer_id": "<event ID or null for initial task>",
  "award_id": "<event ID or null before award>",
  "message_id": "<64 lowercase hex>",
  "type": "task",
  "revision": 0,
  "supersedes": null,
  "author": "<pubkey>",
  "recipients": ["<buyer>", "<seller>", "<Maxplayer>"],
  "text": "Private task text",
  "requested_output": "text/plain",
  "dispatch": {},
  "attachments": []
}
```

`type` is one of `task`, `claim_details`, `progress`, `answer`, `feedback`, `rejection`, mapped to existing signed carriers/authors/phases by the [wire supplement §1](private-offers-wire-v2.md#1-mains-event-rules-remain-authoritative). Attachments have `repo_id`, `commit_oid`, `path`, `size_bytes`, `sha256`; all stay inside encryption. Review findings are attached subject-bound artifacts inside those existing content records or protected Git files, not a standalone transport event. The supplement defines pre-publication versus already-published artifact bindings without circular offer/result IDs. Follow-up work is a new offer/task, not an in-job `followup` type.

A content record is immutable. Edits to non-task explanations have a fresh message ID, incremented revision and `supersedes` ID. A change to the committed task or its required input manifest requires a new offer, not a content revision that silently changes an existing claim. Reordering does not apply an edit over an unknown predecessor.

Serialize the body once as UTF-8 JSON, retaining those exact bytes. The encrypted rumor content is a JSON envelope `{schema:"maxplayer.content-envelope.v2", nonce:<64-hex>, body_b64:<base64-of-body-bytes>}`. All recipient copies carry identical envelope bytes. Compute:

```text
commitment = SHA256(UTF8("maxplayer/content/v2\0") || nonce[32]
                    || u64be(body_bytes.length) || body_bytes)
```

Nonce is independently random and stays encrypted. No plain task hash, unsalted inline-answer hash, or public nonce is substituted. Reject invalid UTF-8, duplicate JSON keys, invalid lengths, or mismatched envelope/body schemas. Byte equality, not JSON key ordering after reserialization, defines a committed body. Publishing the OFFER's commitment does not form a cycle: initial task has `offer_id=null` and binds by random job ID; the resulting OFFER ID is pinned thereafter.

Receivers verify outer/seal signatures and decryption, seal author equals rumor author equals body author, exact recipient set, commitment, job/offer/award binding, authorized author and allowed message type. An outer ephemeral key is not an author. Deduplicate by `(job_id, message_id)`; same ID with different bytes is a protocol error. Never render a message from a losing candidate as selected-seller content.

### 4.2 Recipients and Maxplayer consistency

- Targeted task before award: buyer, targeted seller, configured Maxplayer service identity. This necessarily discloses the task to that target even if it declines.
- Private execution: buyer, awarded seller and that same service identity, including a self-copy for the sender where applicable.
- Discovery has public machine-readable claim coordination. When non-enumerated model/custom-preset details are needed, the signed claim binds encrypted `claim_details` for buyer, that claimant and Maxplayer. This preserves capability matching without publishing free-form labels. It introduces no free-form candidate-pitch feature. Candidates do not receive other candidates’ private details or the winner’s execution content.
- Review/explanation artifacts use the applicable existing carrier’s recipient scope in the supplement (including the candidate for its own pre-award feedback). New audiences need a later explicit policy, not ad hoc extra `p` tags.

Pin the service public key from trusted deployment configuration, never from an arbitrary offer. No payment token, wallet material or credential is copied to this audience.

Every outgoing private content record must generate copies from the same immutable envelope for all required recipients, including Maxplayer, and persist them together in the outbox before publishing. The common salted commitment lets each recipient independently validate the same content/version. Missing the configured service identity or omitting its copy is a local validation error, not an optional mode.

**No Maxplayer application-level ACK or moderation approval is required before display, claim or execution.** The service independently decrypts and validates its received copy; a temporarily offline consumer does not block the seller. Relay acceptance is not proof of service decryption, and the client must not report it as such. Durable retry state tracks unsent copies and relay failures without introducing a new job lifecycle state. Without an application-level ACK, participants cannot prove synchronously that the service received/decrypted its copy; this is the explicit tradeoff of the simplified scope, not a guarantee supplied by encryption.

Recipients validate their own copies and public commitment before using content. A targeted seller must have the actual task/required inputs before claiming; a buyer must have the actual delivery before verifying/paying. These are local content prerequisites, not a new post-award handshake. Events and wrappers may arrive out of order and are buffered boundedly. The same commitment detects a divergent service copy when that copy is processed; it does not prove a malicious sender supplied any usable service copy in advance.

### 4.3 Sending, retries and relay constraints

Persist the exact envelope, logical ID, recipient list and per-recipient send status in a durable outbox before transmission. Use authenticated WebSocket publishing and the existing under-180-second outer-time pattern. Rewrap after a freshness rejection/reconnect, retaining logical ID and body; do not keep retrying an expired outer event verbatim. A receive cursor must overlap the timestamp-randomization window and deduplicate logical IDs; it cannot simply query since the most recent receipt time.

Persist receive state and per-recipient relay publication status. Failed publication remains pending in the content outbox and is retried; it never permits public fallback. An offline Maxplayer consumer is not a start/settlement gate. Unavailable task/delivery content still prevents the action that actually needs those bytes. Durable duplicate processing is idempotent across restarts; a changed body under an existing ID is rejected. The relay's existing membership/rate limits remain; add per-authenticated-sender limits for pending content and unknown-job buffers, not per-random-wrapper-key limits.

Proposed size default: 16 KiB UTF-8 body **including attachment manifest**, plus a measured serialized-envelope cap below both NIP-44 layer limits and the relay's 128 KiB outer-content cap. Test the final encoded lengths, not only raw text. Oversize records fail locally; put large text/files in Git, never silently split semantic records.

## 5. Seller selection — existing lifecycle, no input handoff

Existing buyer auto-award/manual award logic selects one valid claim under existing capability, price and budget checks. A CLAIM is not selection; AWARD is the existing execution authorization. Keep the same lifecycle and restart behavior; do not add an `awaiting_private_inputs`, READY or Maxplayer-ACK stage.

### 5.1 Targeted private job

1. Buyer prepares the complete task and all required input snapshots before offering the job. The repository ID is the random job ID in the buyer/tenant namespace, so input commits and their manifest can be built locally before hosting exists.
2. Provision the job repository using the buyer-signed offer (it may be not yet published), upload the pinned inputs, then publish the offer and recipient-encrypted task copies. Input refs are bound in the committed task manifest; a published task cannot gain new required inputs under the same offer.
3. The target decrypts/validates the offer content and fetches/validates the pinned inputs **before claiming**. Persist the offer binding and retain a job-scoped local snapshot for execution/restart. Maxplayer gets its own encrypted content copy and repo read access; no ACK is awaited.
4. The buyer awards normally; the seller executes the already assessed task using those inputs. A restart recovers the same pinned snapshot or fails/retries the existing execution on infrastructure failure. It does not request new buyer inputs or turn the award into a new exchange.

### 5.2 Open-pool job with private execution and delivery

1. Buyer publishes the complete executable task publicly. Candidates assess it as offered, including already accessible public resources. Confidential inputs, private contribution bases or a later buyer credential handoff require targeted-private instead.
2. Sellers claim and buyer awards using the existing flow. Losing candidates get no private execution content or Git access.
3. The winning seller starts from that public task. There is **no subsequent buyer upload, private input manifest or readiness message to wait for**. Its progress explanations, answers and delivery content use private recipient copies and a private job repo.
4. The private output repository is provisioned idempotently as part of delivery setup using the signed offer and award. Repository authorization may retry or fail like other delivery I/O; it is not a new buyer handoff or a content-ACK start gate. Never fall back to the shared/public seller repository on failure.

### 5.3 Hosting operations, not new lifecycle transitions

Proposed minimal NIP-98 authenticated operation: `PUT /api/jobs/private/{job_id}` with `{signed_offer, signed_award?}` returns the opaque repo locator. This is a **new storage API**, not an already implemented endpoint or a job protocol event.

- Buyer may create a targeted input repo before publication/claim using its valid signed offer. The target has read-only input access; no pre-award delivery writes.
- A buyer or selected seller may ensure the delivery repo with a valid signed award. Validate offer buyer, claim, selected seller, target restriction, tenant and job ID. Persist the exact award binding before granting seller write access.
- Same binding is idempotent. A conflicting award/repo binding returns `409`; it never switches access to another seller by arrival timestamp. The existing buyer selection logic still owns the single-award guarantee.
- NIP-98 method/URL/body/replay checks apply. Every Git read/write rechecks the appropriate stored job/award permissions; creating a repo does not authorize work.

There is no separate `/activate` or `/readiness` workflow in this scope. The hosting operation does not block on Maxplayer decrypting messages. The first provisioning reserves `(tenant,buyer,job_id)` for exactly one validated signed OFFER ID/content commitment; reuse by another offer returns `409`, even before award or after closure. Client and server both enforce this—not just random ID generation. Single-host database uniqueness on buyer/job/repo bindings, restricted access before award, and valid-award write authorization remain necessary storage implementation details.

Failure after AWARD never selects a replacement or duplicates work automatically. Resume the same award/task/delivery state. Material changes to task, price or required inputs need a new offer, not a silent private follow-up.

For cancellation/replacement, proposed initial policy is a **new linked offer/job ID and new repo**, not in-place reassignment. Link only public-safe metadata; buyer explicitly selects content to carry forward and makes it available before the next claim. The previous seller loses write authority when its job closes but retains read access to the old job under existing retention policy. It gains no access to the new job. Previously downloaded/decrypted material cannot be revoked.

## 6. Git permissions, inputs and delivery

Private-job Git/API transport must use HTTPS and relay transport WSS outside local test fixtures. Never forward authorization across origins on redirects.

Separate repository object graph per private job; never implement privacy only by hiding branches in the shared seller repo. Existing trusted CAS/pack storage may be reused internally, but serving and traversal are scoped to the authorized repository. A known OID, locator or warm cache is not authority.

- Buyer: read; create targeted input refs `refs/heads/input/<random-id>` before offer publication. Once the offer is published, freeze its input refs; no new required inputs may be appended to that job. Open-pool private repos are output storage, not a delayed buyer-input channel.
- Targeted seller before award: read-only inputs. Selected seller with a validated award: read; append immutable delivery refs `refs/heads/delivery/<random-id>` for its own award.
- Maxplayer configured service: read every job repository, including committed history. Runtime content-review identity has no general write grant.
- Other sellers/members/anonymous users: no read/write access, even when global `git_public_read` is enabled.
- All participants: no force update, delete or replacement of refs already pinned by an offer/award/result. Close disables participant writes; existing retention policy governs reads/storage.

Enforce this before info-refs, upload-pack/hydration, raw-object/file/preview/archive reads and on receive-pack plus hook policy. Inventory non-smart-HTTP routes during PR2; no direct CAS/object-storage URL may bypass the gate. Private repository announcements expose at most opaque IDs and allowed coordination metadata; suppress public automatic 30618 ref listings or emit only an explicitly safe subset. Keep public repo behavior unchanged.

For targeted private offers, input snapshots are committed/uploaded before offer publication and their private manifest is part of the initial committed task. Required snapshots are fetched before claim, not after award. Buyer pins exact input commit(s); seller fetches only from the configured authorized host and validates manifests before handing files to the runner. Reject path traversal/absolute paths and materialization through symlinks; no automatic submodule/LFS/external URL fetch. External dependencies remain externally managed. Credentials do not go into Git.

Targeted contribution jobs import the buyer-authorized pinned base/history before claim. Open-pool contributions may use only bases already accessible for assessment/execution, not a later private import from the buyer; additional private commits never push upstream automatically. Same-seller follow-up work uses a new job and explicitly selected input snapshot/history; do not automatically mount unrelated earlier work. No seller-memory feature is added.

Trusted Maxplayer backups and internal pack/CAS caches remain within the existing storage/lifecycle policy. Recipient encryption of those trusted internal file copies is not required. This introduces no backup/retention/deletion redesign, and the deployment's backup configuration is not evidence that live bucket permissions were audited. The required feature change is job-level authorization on served data, including cache hits—not an independent cache fix or a generic logs/caches/backups workstream. Unused episode/telemetry capture/emission and seller memory remain excluded (§0).

**Proposed initial limits:** 10 MiB per file; 100 MiB cumulative unique uncompressed Git-object bytes per job including retained history; 1,000 files per snapshot. Count imported bases, trees/commits and inputs/deliveries against the repository cap. Reject oversized contribution bases clearly. Apply client preflight and authoritative server checks in quarantine before refs/CAS publication, with compressed-request, decompression, object-count and processing-time bounds. Concurrent pushes reserve quota transactionally; a rejected push cannot publish partial refs or consume permanent visible quota. Numeric transport/processing bounds are finalized from existing host limits in PR2 and covered by boundary tests.

## 7. Delivery and money invariants

Do not bypass verification or change who is paid. Preserve `offer → claim → award → result → verify → accept → pay → receipt`, the free-job no-payment path, amount/mint checks, delivery sentinel, contribution-base binding, recipient identity and existing pay-once fences.

The existing task-derived public `job-hash` is unsuitable for private tasks. Proposed v2 definition for all modes:

```text
job_hash = SHA256(UTF8("maxplayer/job/v2\0") || offer_event_id[32])
```

The signed OFFER already binds the amount, task or salted task commitment, visibility and random job ID. Both seller and buyer must validate that offer and private task commitment before deriving/accepting the hash. This retains a 32-byte commitment in existing receipt machinery without exposing a separate plaintext guessing oracle. Update every job-hash producer/verifier and sentinel fixture together; do not change just the seller output. A changed task/amount requires a new OFFER ID and hence a new job hash.

Git RESULT retains verification-required repo/ref/commit identifiers with opaque naming. The buyer fetches the exact result commit from the authorized job repo, verifies input/base relationship and sentinel, then persists the exact result-specific local bind before publishing main’s job-level ACCEPT and executing payment. The public ACCEPT itself does not gain a result reference; the receipt already names the exact result. Do not copy task text into public commit descriptions or result summaries; full commit content stays within the private repo.

Private inline answers live in the encrypted answer body, not public RESULT content. Define v2 inline delivery identity as that answer's salted `content-commitment`; buyer validates decrypted bytes against it before ACCEPT. Update the existing inline-preimage construction and verification on both sides to use this identity, using the exact v2 domain/array and the existing result-ID/mint exclusions defined in [wire supplement §3](private-offers-wire-v2.md#3-exact-delivery-and-settlement-binding). Public v2 inline answers use the same envelope/commitment formula with a public envelope, since their text is intentionally public. Keep the existing eligibility rules: no contribution inline delivery; a nonempty worktree delivers Git; marked answer required. Test that no remaining public field carries an unsalted hash of private inline text.

A private verification/rejection explanation is encrypted with the service copy. Public REJECT retains its enumerated reason and result binding. Missing delivery content, unknown repo or unverifiable delivery cannot turn into ACCEPT or payment. An offline Maxplayer content consumer alone does not prevent valid buyer verification or payment. Content-copy delivery is retried independently; no service ACK is required or treated as proof of work quality.

## 8. Three staged implementation PRs

The original development stages below used off-by-default flags. For the consolidated release, Petar requested upgrade-and-restart defaults on 24 September: private switches default on with the pinned Maxplayer service public key and Git host; explicit overrides are preserved (see the rollout guide). No deployment is implied. With a stage disabled, attempts to request the new private lane fail explicitly; flags never downgrade it to public. Old behavior remains only before coordinated cutover, not as an indefinite compatibility implementation.

**PR1 — protocol and encrypted content** (`private_content_v2=false`): v2 typed schemas/builders/parsers; exact byte/commitment vectors; extracted envelope helper; outbox/inbox and independent Maxplayer content consumption; public field allowlist; timestamp/size/replay tests. Publish cross-implementation byte/signature vectors from the wire supplement. No user-visible private posting until all stages are enabled. Target files: gateway/kinds, new content module, payment transport extraction, service consumer, schema fixtures. Failure gates: conflicting recipient copies, forged author, wrong job/award, stale wrapper, duplicate IDs, missing service copy, oversize message, no change to payment recipients.

**PR2 — private Git scope** (`private_job_repos=false`, requires PR1): job/ACL database migration, idempotent job-repo provisioning, read-route authorization, push hook roles, immutable refs, quotas and private announcement behavior. Target files: buzz Git transport/policy/hydration/manifest paths plus schema and API routing. Failure gates: outsider with valid relay membership; global public-read enabled; warm-cache access; arbitrary OID/cross-job traversal; stale token after closure; buyer writing delivery ref; seller rewriting inputs; conflicting award binding; crash during provisioning/award binding; concurrent quota pushes.

**PR3 — end-to-end product and cutover** (`private_jobs=false`, requires PR1+PR2): CLI/MCP/config/UI, targeted pre-claim reading, complete open-pool offers, unchanged award-to-execution transition, targeted pre-claim attachments/contributions and new-offer follow-ups, new job hash and private inline verification, collect/reject/restart paths. Failure gates: two competing claims, delayed/lost messages, restart at every transition, private inline paid exactly once, Git contribution verification, free jobs with no payment, no content on public surfaces. Public-v2 regression suite is required. Remove superseded v1 runtime paths during coordinated release, update protocol docs/quickstarts and explicitly migrate user defaults to private.

No need to ship an interim public release between these PRs. Internal flags exist to make review/testing separable, not to expose half-private jobs.

## 9. Acceptance test matrix and rollout

Use deterministic local relay/service/Git fixtures and at least four identities (buyer, seller, Maxplayer, outsider); add a second candidate for selection races. These are required future tests, **not tests run for this documentation PR**.

1. Targeted offer: buyer/target/service decrypt identical task; outsider cannot; target validates/fetches all required inputs before claim and does not execute before award. Missing task/inputs prevents claim; published inputs cannot be appended/replaced.
2. Open pool: initial task stays public and complete; winner executes on award with no buyer-input handoff or new waiting state. Post-award input upload is unsupported; jobs needing confidential prerequisites use targeted-private. Progress/answer/delivery remains private.
3. Fully public targeted/open modes still complete on protocol v2.
4. Required Maxplayer copy is durably generated with every private content record; offline service consumer does not block claim/execution/settlement. Retry relay failures; service validates matching commitment when received. Missing service key/copy fails local construction, and wrong received content is rejected without claiming synchronous proof of service receipt.
5. Correct encrypted bytes with forged seal author, wrong job/offer/award or wrong recipient set are rejected.
6. Public event snapshots contain none of a seeded task/title/path/URL/error/answer corpus; cover network UI, repo announcements and inline integrity tags, previous-turn context, previews, feedback, review/classifier excerpts and rejection details.
7. Reordered wrapper/event, offline recipient, lost relay OK and process crash recover idempotently without duplicate execution or payment. Restart never invents an ACK/input-handoff dependency; targeted inputs remain pinned to the original claim.
8. Two claim/award races yield exactly one active scope; conflicting signed awards do not grant both sellers execution.
9. Every Git read path denies outsider on cold and warm cache, global public-read enabled, direct OID and alternate ref requests.
10. Write-role/ref rules survive scoped/unscoped tokens; stale permission is rechecked server-side; closed jobs reject pushes.
11. Per-file/message/repo/count limits test limit−1, limit, limit+1; decompression and simultaneous pushes cannot bypass quota.
12. Attachment manifest tampering, path traversal, symlink escape, wrong input commit, unexpected external fetch and contribution-base mismatch fail closed.
13. Private Git and inline paid delivery retain signature verification, budget limits and pay-once recovery. Free delivery never constructs a paid receipt.
14. Replacement job sees only explicitly carried inputs; old seller cannot access the new job.
15. Unknown protocol/flags/service key or missing ACL never falls back to public/legacy. Git/relay outages use bounded existing operation retry/failure paths; an offline Maxplayer consumer alone never creates a start gate.
16. Review of result A cannot be attached to result B, another offer/task revision, another commit, or another inline commitment. Valid re-encryption of a stale review does not bypass subject-version validation.

17. Closed-schema mutations cover every public tag/subkey/value (including invoice nested fields), unknown keys, duplicate singleton/list entries, wrong carrier author/type/phase and claims with missing required private filter details.
18. Two result events for one claim cannot swap the persisted verified delivery or produce duplicate payments after restart. ACCEPT stays publicly job-scoped; RECEIPT names the bound result, and v2 Git/inline preimages match independent byte vectors.
19. Another signed offer cannot reuse `(tenant,buyer,job_id)` before award or after closure; repeat of the exact pinned offer is idempotent.

Rollout: drain active v1 jobs; back up/migrate the existing stores under existing operational policy; deploy relay/service/schema, then buyers/sellers/UI; verify all peers advertise v2; run local/staging four-identity tests; enable the complete lane together. Existing public history stays public. Do not attempt to privatize old public Git objects or postings. No production probe is part of this docs task.

Rollback: disable new admissions and pause affected executions; leave the new parsers, private ACLs and stores available for recovery. Never revert to software that treats private repos as globally readable. Finish or explicitly cancel in-flight jobs before any incompatible downgrade.

## 10. Self-review: fixes incorporated and remaining risks

- **Envelope defaults versus relay drift:** use the existing bounded outer timestamp strategy and test encoded sizes/reconnect overlap. No claim that merely calling the library default works.
- **Maxplayer copy without a handshake:** generate all recipient copies from one committed body, persist/retry them durably, validate independently on receipt. No synchronous proof of service receipt/decryption or malicious-sender copy compliance is claimed; no consumer-availability start gate.
- **Selection/ACL consistency:** existing single-award selection; idempotent repository provisioning and exact signed-award write binding. No reassignment by arrival order, new waiting stage or post-award input handoff.
- **Metadata leaks beyond OFFER:** sanitize all public writers, Git announcements, inline hashes and UI; authorization before hydration; no speculative cache redesign.
- **Task hash/inline payment regression:** explicit v2 binding definitions and both-side fixture updates; review money-path changes as security-critical in PR3.
- **Quota bypass via history/compression/races:** cumulative object accounting, quarantine and transactional reservation; imported contributions may exceed proposed limits.
- **Spam and outage:** existing authenticated membership plus bounded pending queues. Relay/Git unavailability can delay their operations; offline Maxplayer consumption does not stop execution. Replication is deferred. Transport acceptance is not decryption evidence.
- **Settled-model risks:** Maxplayer and participants can read and retain content; identities, timing, prices and approved identifiers remain public; public discovery text is irreversible; revocation cannot erase downloaded copies. This is not anonymity or privacy from Maxplayer.

Validation performed for this proposal: inspected anchor code and lockfile, pinned Nostr envelope implementation, original decision records and primary NIPs; checked spec paths/citations and whitespace. The accompanying static flow diagram was rendered and visually inspected. No runtime code, benchmark, crypto interoperability probe, live relay write or payment was performed. Implementation gates above must supply that evidence before enablement.

## 11. Open engineering defaults for review

These do not reopen the product scope. Proposed defaults let implementation proceed without another broad design round:

1. **Limits:** 16 KiB message body, 10 MiB/file, 100 MiB cumulative job object budget, 1,000 files/snapshot. Review with representative contribution repositories before enabling; publish negotiated server ceilings to clients.
2. **Replacement/follow-up:** a new linked job/repo, explicit input carry-forward; no automatic access to prior private history. Existing recipients keep old read access under current retention policy.
3. **Maxplayer content consumption:** independent decryption/validation with the same private content audience for explanations/reviews. Mandatory ACK/start gating is removed by §0’s later decision, not left as an implementation option.
4. **Storage API shape:** proposed NIP-98 idempotent `PUT` to ensure a job repo and bind write permissions to an existing award; same host/database as Git. No activate/readiness handshake.
5. **Public/private UX:** `visibility` plus existing seller targeting, with explicit public-discovery wording. Default private execution is not a promise to hide an open-pool task.

Recommended implementation starting point: PR1 wire-schema and commitment test vectors, followed by PR2's outsider-access tests. Enable only after PR3's complete flow passes.

## 12. Independent review resolution

The review of `207f38c` identified incomplete message mapping, public schema and delivery binding, plus job-ID reuse and a diagram audience ambiguity. [The wire supplement](private-offers-wire-v2.md) resolves the three schema findings and identity reuse without adding lifecycle roles/states. The diagram now explicitly distinguishes targeted read-only pre-claim access from selected-seller delivery access. Earlier source snapshots remain unchanged. [Independent re-review](private-offers-decisions/independent-review-resolution.md) by GPT-5.6-Sol approved the corrected specification with no remaining blockers found; this is documentation review, not runtime validation.
