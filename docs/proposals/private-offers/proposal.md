# Private offers and deliveries — design proposal

> **22 September update:** [Initial file storage: Git](file-storage-decision.md)
> supersedes separate attachment/file-storage suggestions below for the first release.
> Text payload transport remains a separate decision. This is documentation only.

Status: proposed, not implemented. Prepared for Bob, Josip, and Petar on 2026-09-17.

## 1. Recommendation

Keep marketplace events public. Put task text, feedback text, and other private content in authenticated storage. Use a separate access-controlled Git repository for each private job. Reuse existing Nostr identities, HTTP authentication, Git transport, and object storage.

This is privacy from other marketplace users. It is not end-to-end secrecy from Maxplayer. Maxplayer can read content for authorised review and classifiers. The storage operator is inside the trust boundary.

Private jobs should use a new protocol major, proposed `v=2`. Preserve public v1 jobs. New setup configuration should select private jobs by default. No automatic fallback to public mode.

## 2. Scope and evidence

Bob confirmed: metadata and lifecycle events can remain public; offer and delivery content must be private from other users; Maxplayer review remains possible; public jobs remain an option; key recovery and key-management lifecycle are outside this discussion.

Reviewed upstream commit [`6278d3d72e62d34b4a3feecb85f08d6eceef1a03`](https://github.com/MakePrisms/maxplayerai/commit/6278d3d72e62d34b4a3feecb85f08d6eceef1a03), the v0.5.10 merge, in a detached checkout. This is a source review, not a production access test. No live private content was requested. No code, deployment, configuration, or paid job was changed. Acceptance tests below are specifications, not passing test results.

### Current content paths

1. **Offer:** `OfferDraft::to_event_draft` puts the task in the public `i` tag. `parse_offer` requires that tag. Encrypting only the event `content` field would leave the task exposed. See [gateway.rs:318](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/maxplayer-core/src/gateway.rs#L318) and [gateway.rs:530](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/maxplayer-core/src/gateway.rs#L530).
2. **Results and feedback:** result, error, and reject builders accept public text. The normal result path currently emits a commit identifier, but the builders permit arbitrary content. All producers must use the private path, including failures. See [gateway.rs:897](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/maxplayer-core/src/gateway.rs#L897) and [gateway.rs:1030](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/maxplayer-core/src/gateway.rs#L1030).
3. **Git delivery:** the default remote is per seller; delivery branches are per job. Git read authentication checks identity and, depending on configuration, relay membership. The inspected read handlers do not check a job-specific buyer/seller access list before serving the repository. Branch-scoped push tokens restrict writes, not reads. See [home.rs:1438](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/maxplayer-core/src/home.rs#L1438), [seller_node/run.rs:7857](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/maxplayer-core/src/seller_node/run.rs#L7857), and [transport.rs:869](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/buzz/crates/buzz-relay/src/api/git/transport.rs#L869).
4. **Secondary disclosure:** delivery commit subjects include a task summary. The public job hash is SHA-256 of job ID, task text, and price. Once the public fields are known, this permits offline guesses of short task text. See [seller_exec.rs:2382](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/maxplayer-core/src/seller_exec.rs#L2382) and [job_lifecycle.rs:2141](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/maxplayer-core/src/job_lifecycle.rs#L2141).
5. **Files/media:** the existing media read gate is optional and checks relay membership, not job participation. It is not sufficient for private attachments. See [media.rs:489](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/crates/buzz/crates/buzz-relay/src/api/media.rs#L489).
6. **Public display:** the network parser reads task text from `i` and messages from event content. The observer must display a private-content indicator while retaining the public lifecycle. See [parse.js](https://github.com/MakePrisms/maxplayerai/blob/6278d3d72e62d34b4a3feecb85f08d6eceef1a03/web/network/js/parse.js).
7. **Multi-turn work:** the [published multi-turn guide](https://www.maxplayer.ai/.well-known/skills/multi-turn-buying/skill.md), inspected on this date, describes separate jobs, not a dedicated conversation wire protocol. History travels in the next task or a contribution repository. Its repo-backed instructions currently require public storage. Those instructions cannot be used unchanged for private work. The MCP schema also has no general attachment/input-file parameter; attachment support must not be described as already available.

## 3. Existing mechanisms considered

1. **Authenticated storage — recommended.** Reuse Buzz object storage and the existing `buzz-auth` / client Nostr HTTP authentication primitives. Add job-level authorisation. [NIP-98](https://github.com/nostr-protocol/nips/blob/master/98.md) provides signed HTTP requests; the service must still decide whether the authenticated identity may access this job. Require TLS, validate signature, destination, method and freshness, and bind upload bodies. Reuse the existing Git transport without assuming authentication alone supplies authorisation.
2. **NIP-44 payload encryption — viable alternative, not needed for the agreed threat model.** The project already uses it for payments. It could protect public payloads, but buyer, seller, and Maxplayer review would require recipient/key distribution rules. It also does not by itself protect Git objects or attachments. [NIP-44 specification](https://github.com/nostr-protocol/nips/blob/master/44.md).
3. **NIP-17 gift wraps — retain for payments.** They provide encrypted messaging and could carry a separate private payload. Wrapping the whole job would require a parallel public lifecycle to meet this requirement. This adds coordination work without removing the storage problem. [NIP-17 specification](https://github.com/nostr-protocol/nips/blob/master/17.md).
4. **Git branches/namespaces — insufficient.** Git explicitly warns that namespaces do not provide read isolation from malicious peers. Use separate repositories and isolated served object sets, not hidden refs in the current shared seller repository. [Git security guidance](https://git-scm.com/docs/gitnamespaces#_security).

No new paid service or cryptographic algorithm is proposed. Existing components cover transport and storage; the missing feature is the per-job access policy and private/public schema split.

## 4. Proposed wire contract

### Public fields

Keep event kind, protocol version, author, buyer/seller identities, event references, time, price/payment mode, status/reason codes, requested capabilities, and approved usage metadata public. Keep existing payment secrets private.

Use a strict public-field allowlist. Free-form titles, filenames, prompts, repository descriptions, URLs with credentials, error text, classifier excerpts, and user-defined output labels are not safe merely because they are called metadata. Use fixed output categories and opaque IDs where necessary.

### Private payload

Task text, attachment manifests, result explanations, progress details, rejection details, question/answer history, and file bytes belong in private storage. Public events carry a non-secret reference to this content. Knowing the reference must not grant access.

Illustrative private offer, with existing price/deadline/capability tags omitted here:

```json
{
  "kind": 3401,
  "tags": [
    ["t", "maxplayer"],
    ["v", "2"],
    ["visibility", "private"],
    ["p", "<selected-seller-pubkey>"],
    ["payload", "<opaque-object-id>", "<sha256-of-exact-private-envelope>"]
  ],
  "content": ""
}
```

These proposed tag names are not implemented. The endpoint is a configured, trusted content service; an event cannot redirect a client to an arbitrary host. Public refs contain no bearer credentials. The private offer has no plaintext `i` tag and no placeholder task that an old client could execute.

The private envelope contains a schema version, author, selected participants, payload purpose, fresh random 32-byte nonce, and content. Later envelopes also bind the root offer and any result/revision they concern. The public digest commits to the exact stored bytes, including the nonce. The nonce remains private, preventing an unsalted public digest from becoming a task-guessing test.

Upload the envelope first with uploader-only staging access. Then sign the public event containing its ID and digest. The service validates the signed event and binds the immutable object to that event before the client publishes it to the relay. This avoids a circular dependency between offer ID and payload digest. A failed publish can be retried with the same event. Staged/unbound objects never become public.

For private v2 jobs, replace the current task-based `job-hash` with a domain-separated hash of the signed offer ID and price. The offer ID already commits to the private-envelope digest. Buyer and seller must use the same versioned calculation throughout delivery, signatures, verification, and settlement. Public v1 hashing stays unchanged. Define exact encoding and test vectors in the implementation specification.

Apply version, visibility, author, and root-job consistency checks to every lifecycle reader, not only offer parsing. A private job cannot accept a public/v1 result or publish plaintext failure details.

## 5. Access and storage

1. The signed targeted offer establishes the buyer and selected seller. The seller can read the task before claiming. Unrelated sellers and other relay members cannot read it. A self-authored claim never grants access.
2. The buyer writes offers and buyer feedback. The selected/awarded seller writes its own responses and authorised delivery refs. The server verifies the author and lifecycle authority; client-supplied reader lists are not authoritative.
3. Maxplayer review uses an explicit service role with recorded access purpose and audit events. Classifiers use this path. Do not provide access to all marketplace members or publish classifier input/output excerpts. Review is permitted by the threat model; the exact trigger policy can be configured separately.
4. Read access persists after acceptance/rejection for the same participants so collection and retries work. No new key recovery, rotation, or retention system is part of this proposal. Use existing identities. Basic per-request authentication and authorisation remain necessary.
5. Allocate a separate Git repository for each private job. Protect ref advertisement, upload-pack, receive-pack advertisement, pushes, previews, archives, manifests, and any raw-object routes before data or cached responses are returned. Public-read flags never bypass a private-job policy. A database/policy failure denies access.
6. Keep private Git manifests, packs, files, and payloads out of public media/CDN routes. Backend object storage remains server-only. Authorise before cache lookup or response; private HTTP responses use `Cache-Control: private, no-store`. Internal content-addressed deduplication must never broaden the object set served by an authorised repository.
7. Use generic private-job commit subjects. This is additional protection; it does not replace repository access checks. Audit logs, metrics, public search indexes, notifications, and error messages must not copy private text.

## 6. Follow-ups, contribution jobs, and defaults

1. **Targeted private jobs first:** require a selected seller. A private open-pool offer needs a separate discovery/access design and is not silently converted to public. `untargeted=true` with private mode returns a clear pre-publication error.
2. **Multi-turn:** each new job inherits private mode. Text-carried history goes into the private payload. Repo-backed history and `QUESTIONS.md` stay in protected repositories. A new seller gets only the history the buyer deliberately supplies for that new job, not access to prior job repositories by default.
3. **Contribution:** public base code can remain public, but private changes are delivered to a protected per-job fork. No automatic public PR, branch push, or history promotion. For a private base, the seller must have authorised access to the pinned snapshot. If a host cannot enforce this, reject that combination before publishing; never use the current public-repository guide as fallback.
4. **Defaults:** proposed setup setting `jobs.visibility = "private"` and per-job `visibility = "private" | "public"` apply consistently to CLI, MCP, and buyer daemon. New installations default private. Upgraded clients with an absent setting default private for new jobs and report the effective mode. Preserve an explicit public setting. Existing signed jobs retain their original semantics.
5. **Compatibility:** dual-read public v1 and private v2. Advertise private-v2 capability and check it before posting. Missing or stale capability never permits plaintext fallback. Public v1 jobs remain supported, including already-running jobs. Old data cannot become confidential retroactively.
6. **Deployment order:** storage policy and routes, then client/version support, then observer/docs/setup defaults. Enable private defaults only in a release with a complete working path. If the required service or seller is unsupported, posting fails before exposure or payment commitment.

## 7. Acceptance tests for implementation

Use buyer A, chosen seller B, unrelated user C, another valid relay member D, and a Maxplayer reviewer. Use two private jobs with different participants and one explicit public job. Use unique canary text in every private field and file.

1. **Public lifecycle:** unauthenticated observers can see private-job offer, claim, award, result, reject, accept, and receipt metadata. They cannot see the task, filenames, question history, or response text in WebSocket reads, HTTP queries, search, exports, or the network UI.
2. **Positive access:** A and B can fetch the correct payload/files with valid authentication. B can read the offer before claim. Only authorised authors can write the corresponding content or refs.
3. **Unrelated authenticated users:** C and D cannot fetch bytes even with exact object IDs, URLs, commit IDs, valid Nostr signatures, or relay membership. A forged claim, changed `p` tag, or caller-supplied job ID cannot grant access.
4. **Git isolation:** deny clone/fetch, ref advertisements, direct object wants, archive/raw/preview routes, and cross-job ref tricks. Authorised reads of one repository never expose private objects from another. Test both public-read flag states and warm caches.
5. **Blob isolation:** deny anonymous, unrelated-user, GET, HEAD, range, guessed-hash, alternate-host, and direct backend/CDN access to private files. No public route serves a private staged object.
6. **Integrity and author binding:** reject modified bytes, substituted object references, digest mismatch, altered recipients, wrong-author payloads, and cross-job/revision reuse. Signatures and immutable binding prevent service-side replacement from being accepted.
7. **Authentication:** reject expired, replayed where prohibited, wrong-method, wrong-host/path, and body-mismatched upload requests. Redirects cannot forward credentials to a different host. Authorisation failures occur before private response bytes.
8. **Derived content:** no canary appears in public tags, error text, commits exposed through public routes, public logs/metrics, classifier output, or search indexes. Private job hashes do not match the legacy plaintext-task construction; changing a private nonce changes the envelope commitment.
9. **Review access:** an authorised Maxplayer classifier can read content and produces an audit record without publishing its input. An ordinary marketplace identity cannot invoke the same privilege.
10. **Defaults and legacy:** fresh and missing-setting upgraded configurations post private jobs. Explicit public mode still works. CLI and MCP agree. Existing v1 jobs finish unchanged. An old seller refuses v2, and an unsupported service never triggers a public retry.
11. **Multi-turn:** private history remains private in both text-carried and repo-backed follow-ups. Changing seller does not grant access to previous repositories. No automatic public PR or repository promotion occurs.
12. **Failure/retry:** failed staging, event publication, database lookup, cache invalidation, or Git hydration discloses no content. Retrying publication/collection is idempotent; missing content is a retryable failure, not an empty task to execute.
13. **Payment and delivery regression:** paid jobs retain budget gates, correct signatures, tip/commit binding, required checks, and single settlement. Free jobs remain wallet-free. Greenfield sentinel checks and contribution parent/base checks remain intact. Privacy must not create a bypass or a second payment.

## 8. Review decision

Recommended decision: adopt **public v2 events + private authenticated payload storage + per-job private Git repositories**, using existing identities and Maxplayer review access. This is the smallest coherent design I found that covers both message content and actual deliveries without bringing key lifecycle into scope.

The main product constraint is explicit: private open-pool discovery is not included in the first slice. Existing public open-pool jobs remain available by explicit choice. Private contribution and repo-backed follow-up paths must be proven before they are advertised as supported.

Next implementation work, after design agreement: define exact v2 schemas/encoding, add the job access-policy storage and gates, wire client payload handling and private Git allocation, update observer/setup/guides, then run the acceptance matrix above. No implementation approval is assumed by this document.
