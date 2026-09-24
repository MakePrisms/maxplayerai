# Private offers: coordinated v2 rollout

Implementation branch only. **Do not enable until the implementation PR and its
client/relay end-to-end checks are approved.** No migration or deployment is performed
by this document or by building the binary.

## Configuration and buyer surface

All participating buyer and seller homes use the same trusted service public identity
and HTTPS Git deployment prefix. The three switches stage the rollout together:

```toml
[privacy]
private_content_v2 = true
private_job_repos = true
private_jobs = true
default_visibility = "private"
service_pubkey = "<Maxplayer public key: 64 lowercase hex characters>"
git_base = "https://<approved-host>/git/"
```

These are **public identifiers, not secret keys**. Switches default false in this
staging release; an unset visibility defaults private and fails clearly when the
installation is not enabled/configured. It never silently posts publicly. A persisted
`default_visibility = "public"` remains public; an explicit per-job visibility wins.
The relay's private-repository provisioning switch must also be enabled at cutover.
Turning it off subsequently blocks new provisioning; it does not expose existing repos.

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
   and configure the same service identity and Git route everywhere. Keep switches off
   while validating configuration and service credentials privately.
3. Prove targeted and open-pool private trades with buyer, selected seller, Maxplayer,
   and an outsider. Check inline and Git delivery, paid/free settlement, restart and
   recipient-copy retry. Outsiders must not read cached or freshly hydrated private Git.
4. Enable the switches together. Restart buyer/seller processes (configuration is
   startup-loaded). Explicit public jobs use v2 bindings too; do not leave old producers
   running. Old persisted receipt journals retain their original signature domain.
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
  caps historical results at 2,000. Lifecycle reads therefore request 128-event
  pages (supported relays must honor that size), count matching events before
  application rejection, and page backward. Because the relay applies the namespace
  `#t` filter after its database limit, these requests omit `#t` on the wire and
  apply it locally only after completing the superset scan; otherwise foreign tags
  could make a truncated response appear empty. Before crossing a timestamp they read
  its complete second separately, preserving ties. Saturated seconds, changing
  boundary rows, notification gaps, timeouts or the 32-window work budget produce
  retryable unknown state, never confirmed absence; reservations stay held.
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
