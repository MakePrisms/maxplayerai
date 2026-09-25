# Private offers: coordinated v2 rollout

Implementation branch only. **Do not enable until the implementation PR and its
client/relay end-to-end checks are approved.** No migration or deployment is performed
by this document or by building the binary.

## Configuration and buyer surface

Standard Maxplayer buyer/seller installations inherit the following defaults when
configuration fields are absent. Upgrading and restarting is sufficient after the
coordinated relay cutover; no manual config edit is needed. Explicit existing values,
including `false` switches and `default_visibility = "public"`, are preserved.

```toml
[privacy]
private_content_v2 = true
private_job_repos = true
private_jobs = true
default_visibility = "private"
service_pubkey = "31b18b42bcef9842c10e518834d32da2a0f8f6f8f3758124e25cc392ada1fe5c"
git_base = "https://relay.maxplayer.ai/git/"
```

These are public identifiers, not secret keys. The relay defaults to the same service
public key and enabled private-repository provisioning. Operators may override
`MAXPLAYER_PRIVATE_SERVICE_PUBKEY` and `MAXPLAYER_PRIVATE_JOB_REPOS`; self-hosted
buyer/seller installations must configure their intended service identity and Git host.
An explicit disable or invalid configuration fails closed, never downgrading a private
job to public. Disabling provisioning does not expose existing private repositories.

The service identity reuses the deployed reviewer's existing keypair. Its private
key stays in the operator-managed signer file, never in the repository or client
defaults. Keep a verified secure backup; do not generate a replacement for each
role. Reviewer trust, `privacy.service_pubkey`, and the relay's service ACL identity
must agree. See [identity migration](../execution-reviews.md#reusing-the-deployed-reviewer-identity)
for explicit settings and jobs encrypted to an earlier identity. Shipping the public
key alone does not deploy or verify a content consumer.

`post_job` accepts:

- `visibility`: `private` or `public`.
- `output_category` for private jobs: `text`, `code`, `image`, `audio`, `video`, `data`,
  `archive`, or `other`. The original `output` remains the execution request, not a
  lossy category substitution.
- `inputs`: local regular-file inputs, each `{ "source": "/absolute/local/file",
  "path": "relative/job/path" }`. Available for targeted private jobs; uploaded,
  pinned, and readable by the target before claim. No symlinks or traversal paths.

Targeted private jobs encrypt the original task, output and dispatch preferences for
buyer, target and service. Open private jobs explicitly disclose their initial task
and matching requirements; only subsequent content and delivery are private. They do
not support secret discovery or a post-award input phase. Fully public jobs retain
public task/Git content, with v2 receipt bindings and a public inline envelope.

Private contribution jobs preserve original owner/base pins and the existing
contribution authorship tuple. The buyer imports the exact pinned Git base into the
per-job repo before a targeted offer is published. This Git-object import is distinct
from an optional file-manifest `contribution.input`; the latter is also verified when
provided. Neither source credentials nor Git hooks/config are copied into execution.
Follow-ups are new offers with explicit input/history pins, not amendments to old tasks.

## Cutover sequence

1. Drain existing trades and reconcile pending payments on their original versions.
   Back up participant homes and the relay database/object-store state.
2. Apply relay schema/provisioning changes, deploy the reviewed client/relay binaries,
   and verify the matching service identity and Git route everywhere. Defaults enable
   private jobs: explicitly set switches false in staging while validating configuration
   and service credentials privately. Upgrade the relay before standard clients.
3. Prove targeted and open-pool private trades with buyer, selected seller, Maxplayer,
   and an outsider. Check inline and Git delivery, paid/free settlement, restart and
   recipient-copy retry. Outsiders must not read cached or freshly hydrated private Git.
4. At coordinated cutover, remove staging disable overrides or explicitly enable them.
   Restart buyer/seller processes (configuration is startup-loaded). Explicit public jobs
   use v2 bindings too; do not leave old producers running. Old persisted receipt journals retain their original signature domain.
5. Monitor provisioning failures and content-copy backlogs. Never repair a missing copy
   by posting its plaintext publicly. Recipient copies retry independently; a service
   application ACK is not a prerequisite for execution or settlement.

## Local state and recovery

- `private-content.sqlite`: mode 0600, exact envelopes, signed lifecycle context,
  independent recipient outboxes and EOSE-bounded receive progress.
- `public-v2.sqlite`: separate signed public context and immutable inline-signing
  intents; public messages never enter the private authenticated inbox namespace.
- Existing seller lifecycle/outbox and buyer award journals retain their roles.
  Protocol markers make a lost private/v2 context fail closed rather than downgrade.
- Existing buyer acceptance records persist the exact signed offer/claim/award/result,
  envelope/commitment, invoice hash, signatures, artifact identity and funding choice
  before ACCEPT. A same-result retry reuses the sealed record; it does not query a
  newer result or re-plan funding. Only receipt publication identifies the exact result
  publicly; ACCEPT remains job-level.
- Keep the content databases with the participant home during backup/restore. Do not
  delete them to clear a backlog. Recover unavailable input/object storage rather than
  executing an empty task. Review service access uses its own key and repository ACL;
  buyer/seller payment secrets are not part of that recipient set.

No retention-policy redesign, key rotation/recovery redesign, new review product,
legacy compatibility promise, READY message, or application-level service ACK is added.

## Adversarial-review corrections

- Foreign or malformed gift wraps cannot abort unrelated content scans. Count all
  matching wrappers toward page fullness before application validation, so dropping
  an inadmissible envelope cannot hide a truncated interval. Exact EOSE, notification
  gaps and database failures remain fail-closed. Conflicting or over-quota unbound
  envelopes are rejected individually; required content remains unavailable until
  a valid copy is admitted. A saturated single-timestamp interval still fails closed
  rather than skipping potentially valid messages.
- Recipient outboxes schedule a turn per recipient and durably rotate attempted
  copies, including failed/timed-out attempts and restarts. Per-attempt timeouts and
  a separate lifecycle-publication budget keep slow copies from consuming all carrier
  work. Carrier retries rotate too. The SQLite retry table is added automatically;
  preserve it with the content database.
- V2 claim, result and award queries each require their own end-of-history response.
  EOSE terminates a response, not necessarily the full history: the bundled relay
  caps requests at 2,000 rows and the database further clamps to 1,000 by default.
  Lifecycle reads therefore request 128-event pages (supported relays must honor
  that size) and page backward. A bounded per-subscription transport counter checks
  raw EVENT frames against SDK-delivered events at the exact EOSE. Expired, invalid,
  deleted or otherwise discarded rows cause retryable unknown state rather than
  making a full wire page appear short. Counters freeze at EOSE, exclude live
  events, and are removed on success, error or cancellation. Because the relay applies the namespace
  `#t` filter after its database limit, these requests omit `#t` on the wire and
  apply it locally only after completing the superset scan; otherwise foreign tags
  could make a truncated response appear empty. Before crossing a timestamp they read
  its complete second separately, preserving ties. Saturated seconds, changing
  boundary rows, notification gaps, timeouts or the 32-window work budget produce
  retryable unknown state, never confirmed absence; reservations stay held.
  Deploy the updated relay before enabling this lane: failed historical database
  queries must remove the subscription and send `CLOSED error:`, never EOSE.
  Old relays that return EOSE on failure are indistinguishable from honest empty
  history; client-side counting cannot repair that server contract.
  Result filters use an offer-authenticated target or locally validated selected
  seller when available. This narrows outsider traffic but does not replace the
  completeness checks. Single-timestamp saturation remains a fail-closed availability
  limit, not a promise of unrestricted flood resistance.
  Referenced awards and claims are fetched by exact ID; locally authenticated selection
  evidence survives relay pruning. Missing dependencies of a credible selected result
  are retryable unknown state, not proof of no delivery. Outsider result references do
  not gain authority to hold a reservation. Existing evidence/payment checks still run.
- SQLite opens explicitly prohibit symlink following in addition to the pre-open
  file permission check. The owner-only participant state directory remains required.

Regression coverage includes a local authenticated relay mixed-inbox scan, a retry
backlog larger than one batch across database reopen, refused lifecycle dependency
reads with recovery, and actual buyer reservation reconciliation while a selected
claim read is refused. These are local fixtures, not deployed-trade verification.
The capped-history regressions additionally cover paid public-v2 and private
open-pool reservations: 2,000 newer outsider results, selected-seller timestamp
saturation, pagination beyond the relay cap, and recovery after saturation clears.

History-failure coverage also includes a full page of correctly signed expired
selected-seller events discarded by the real SDK, subscription failure on the first
and later result pages, and reservation retention/recovery in both paid visibility
paths. A relay-handler test closes the actual SQL pool and asserts CLOSED without
EOSE and cleanup of both subscription indexes. These tests do not exercise a
deployed relay, live admission/rate limits or a paid trade.
