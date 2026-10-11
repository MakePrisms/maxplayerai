# Execution-safety reviews

Shipped in maxplayer 0.6.0 (#1022, from proposal #1021, integrated with the private-jobs flow
#1033): the review worker, TypeSafe adapter, signed client gates, operator recovery, and local
integration tests. Installing or upgrading a client does **not** deploy a reviewer; the relay
operator runs and upgrades the worker separately, and a released binary is not proof that a
given relay's reviewer is live. No paid classifier evaluation or default-threshold calibration
has been performed.

## What runs where

- Buyer/seller clients request `REVIEW_REQUEST` (3409), verify `REVIEW` (3408), and
  apply their own unsafe-probability threshold. A skip affects only that client.
- The relay owner runs `maxplayer reviewer serve reviewer.json`. The worker fetches
  signed public subjects from that configured relay (or validates signed encrypted
  private input bundles), fetches Git deliveries from its HTTPS Git endpoint, calls
  TypeSafe, persists a signed terminal result, and publishes it in the same privacy lane.
- The worker uses the provider's existing HTTP API, not custom model inference.
  API contract: <https://docs.typesafe.ai/introduction/quickstart>.
- Clients never receive provider credentials. Review signatures do not replace
  Git verification, claim/award eligibility, payment signatures, budgets, or pay-once.

## Code organization

- `maxplayer-core/src/review.rs`: shared review contract and buyer/seller checks.
- `maxplayer-core/src/review/state.rs`: local client status and retry state.
- `maxplayer-core/src/reviewer.rs`: operator worker, TypeSafe calls, input collection, and persistent caching.
- `maxplayer review status/retry`: client recovery; `maxplayer reviewer serve`: operator service.

## Client settings

Fresh buyer and seller homes enable offer/delivery reviews and include the public
reviewer key for `wss://relay.maxplayer.ai`; no manual review configuration is
needed for that relay. This is a public trust anchor, not a secret or a TypeSafe
API credential. Defaults are written on first CLI/home initialization, not by the
npm installer itself. Existing files that omit review settings inherit defaults.
Explicit reviewer maps (including an empty map), keys, skips, thresholds, and
disabled checks are preserved; upgrading does not rewrite them. Other relays
still require their own trusted reviewer entry and do not inherit this identity.

The built-in review settings use the same identity as the private-content service:

```toml
[review]
seller_offer = true
buyer_delivery = true
skip_buyer_pubkeys = []
skip_seller_pubkeys = []
reject_at_or_above_ppm = 500000
timeout_seconds = 300

[review.reviewers]
"wss://relay.maxplayer.ai" = "31b18b42bcef9842c10e518834d32da2a0f8f6f8f3758124e25cc392ada1fe5c"
```

`500000` means unsafe probability >= 0.50 blocks. This remains **uncalibrated**, not
an evidence-backed shipping threshold. No reviewer key is learned from a response.
Without a configured key, default-enabled reviews stop new claims/acceptances with
an explicit error. Distribute keys and deploy the reviewer before enabling clients.
Changing configuration still requires restarting the client/daemon.

The default reviewer and `privacy.service_pubkey` are one service identity.
The worker must hold that service's private key; matching public defaults do not
provision it or prove a live private review. Self-hosted deployments must set both
fields to their own service identity; custom relay trust is never inferred.

### Reusing the deployed reviewer identity

The shared default is the existing reviewer public key `31b18b42…`. Keep its
private key in place; do not replace it with the earlier `7e6b3b05…` content key.

1. Verify that the deployed worker's signer derives the public key shown above,
   and make a secure, recoverable backup of that signer and the worker configuration.
   The NixOS credential source is `/var/lib/secrets/maxplayer-reviewer-signing-key`;
   confirm the live unit's `LoadCredential` before relying on that path.
2. Use that same signer for any other private-content service process. Upgrade the
   relay so its default private-repository service ACL uses the same public key;
   update an explicit `MAXPLAYER_PRIVATE_SERVICE_PUBKEY` override if present.
3. Set both `privacy.service_pubkey` and the Maxplayer `[review.reviewers]` entry
   on existing buyers/sellers to the public key above. Upgrades preserve explicit
   settings, including the earlier content key. Fresh homes and omitted settings
   inherit the new default. Restart affected daemons/MCP processes.
4. Verify a controlled new private offer review, claim, delivery review and
   acceptance, including private Git retrieval, plus a public review.

Changing the default does not re-encrypt existing jobs or migrate existing repository
ACLs. Preserve the earlier content key and old job state; drain/reconcile old jobs
under their original identity where available. A new service key cannot decrypt
ciphertext addressed only to the earlier key. Never repair this by making jobs public.

This is a default/configuration migration, not evidence of a live deployment.

Explicit skips use the role flags or public-key lists, never display names. Local
status records identify the subject and explain configuration/counterparty skips.
A pass also records reviewer identity, event ID, and effective policy in
`reviews/<subject-id>.json`. These local records are audit evidence, not substitutes
for verifying signed reviews on the next check.

## Timeouts and actual retry actions

A client wait defaults to 300 seconds and `review.timeout_seconds` accepts up to 3600, matching the worker's largest window. Existing explicit shorter timeouts remain in effect. **A timeout ends that review wait, not the job.**
It does not claim, accept, reject, pay, bypass the check, or extend job deadlines.

**A plain timeout is no longer the catch-all failure shape.** Three separate changes remove the
silent paths that all used to read as "timeout" at the client (#1115):

- The client checks the relay's OK on its own review-request publication. A refused request
  (size, timestamp, policy) returns immediately as
  `review: relay refused the review request: <relay reason>` instead of a full-window wait.
- The worker answers every *authentic* refused request with a signed terminal error review:
  `stale_request`, `rate_limited`, `queue_full`, `invalid_subject`, `relay_configuration`,
  `review_store`. Private requests get the reply encrypted to the requester only. Only
  unauthorized requests (`unauthorized_request`) and unparseable events stay unanswered, and
  signed error replies are budgeted (at most 60 per minute) so floods degrade to log-only drops.
  One poisoned request or store row no longer ends the worker process.
- A timeout whose request WAS accepted by the relay now says so:
  `the relay accepted the review request but no reviewer response arrived; the reviewer service
  may be down or it dropped the request`. That points at the operator's
  `maxplayer reviewer serve` log (the worker logs its `maxplayer-core` version at start, so
  client/worker version skew is a one-line check).

Seller:

```sh
maxplayer review status <offer-id> --home /path/to/seller-home
maxplayer review retry <offer-id> --home /path/to/seller-home
```

The retry command writes an explicit local retry ticket. The running seller consumes
it on its existing reconsideration tick, clears only that offer's failed check, and
requests/checks the review again. It does not restart the daemon or reserve a slot
while the review is pending. The offer must still be eligible; expired/claimed offers
cannot be revived by a review retry. A policy refusal/conflict is not an availability
retry. The command does not reroll unsafe results.

Buyer: repeat the same `collect` MCP call/CLI command or explicit `accept` command
for the same job/result. Do not post a new job or ask the seller to redeliver unchanged
content. `maxplayer review status <result-id>` shows the local review state.
The error itself explains this recovery path. The automatic buyer settlement watcher
holds a failed review instead of repeatedly initiating paid checks. An explicit
collect retries; a new RESULT from the awarded seller starts a new review. Accepted
payment obligations are never held by this local review status.

`get_job` returns `review_status`. It reviews the newest delivery from the awarded
seller before exposing its content to the agent. Failed or unselected deliveries
retain identifying metadata but withhold inline answers, repository/branch strings,
agent/model strings, and contribution metadata. Collect independently checks the
selected result before acceptance. Already-accepted obligations keep payment recovery;
review changes do not retroactively cancel a payment obligation.

## Relay-owner worker

Example `reviewer.json` (paths are operator-managed; no credentials in this file):

```json
{
  "relay": "wss://relay.example",
  "signer_file": "/run/secrets/reviewer-signing-key",
  "provider_key_file": "/run/secrets/typesafe-api-key",
  "database": "/var/lib/maxplayer-review/reviews.sqlite",
  "model": "jev-latest",
  "window_seconds": 600
}
```

Secret files must be regular owner-private files (0600 or stricter). Provision the
existing content-service signer and distribute its **public** key to clients. Never
put credentials in command arguments, public events, logs, or this repository. The database parent
must exist and be writable. Run the service under an operator-managed supervisor.
Public and private per-job repositories are supported. Private review uses the existing
content-service identity and repository ACL; it does not grant a new reviewer identity
access to a private job. Configure the reviewer signer to that identity, and set the
client reviewer trust entry to the same `privacy.service_pubkey`.

```sh
maxplayer reviewer serve /etc/maxplayer/reviewer.json
```

### NixOS deployment alongside the relay

The flake exports `nixosModules.reviewer` (`services.maxplayer.reviewer`). The
existing `.#relay` host imports and enables it alongside Buzz and supplies the
flake's Maxplayer package, which includes the wallet/reviewer feature. No separate
manual binary install or foreground reviewer command is needed with this deployment.

Before applying that deployment, provision these files on the target using your
secure secret-provisioning method. Keep their contents out of Git, Nix expressions,
command arguments and logs. Both source files should be root-owned, mode `0600`:

- `/var/lib/secrets/maxplayer-reviewer-signing-key`: the **existing reviewer private
  key, reused as the shared content-service identity** (raw 64-character hex or supported
  Nostr secret encoding). This legacy filename is retained for deployment compatibility;
  `signerFile` may instead point directly at your securely provisioned service key.
  It is **not** a `NAME=value` environment file.
- `/var/lib/secrets/typesafe-api-key`: the raw TypeSafe API key.

Distribute only the reviewer's corresponding public key to clients. Ensure this
identity matches `privacy.service_pubkey` and can authenticate and read authorized
private as well as public Git repositories on the relay. Do not
rotate the signing key on every deploy; clients trust that specific identity.

Deploy the reviewed revision through the existing NixOS host flow:

```sh
nixos-rebuild switch --flake .#relay --target-host root@<host> --build-host root@<host>
```

On the target, verify:

```sh
systemctl status maxplayer-reviewer.service
journalctl -u maxplayer-reviewer.service -n 100 --no-pager
```

The unit starts on boot, after network-online and the local Buzz service, and
restarts on failure. It does not make Buzz depend on the reviewer. Missing secret
files cause reviewer startup to fail; no credentials or keys are auto-generated.
After supplying or rotating credentials, restart `maxplayer-reviewer.service`.
A running service alone is not an end-to-end proof: verify signed offer, inline,
and Git delivery reviews with controlled clients before rolling out normal clients.
Starting with valid credentials enables real TypeSafe calls for eligible requests.

Systemd `LoadCredential` supplies read-only copies of the two secrets to a
dedicated dynamic service user. These can have mode `0440` within systemd's
protected credential mount. The launcher stages owner-only `0600` copies in the
service-owned `0700` runtime directory, preserving the reviewer's strict secret-file
permission check. These runtime copies are removed when the service stops; the
original provisioned files remain unchanged. The launcher resolves the existing state directory
with `realpath -e` before composing the database filename: `DynamicUser` can make
`/var/lib/maxplayer-reviewer` a symlink into `/var/lib/private`, which SQLite's
`SQLITE_OPEN_NOFOLLOW` rejects. Only the directory is resolved; a symlink at the
database filename remains rejected. This uses the same database without moving,
resetting, or migrating state. A missing state directory fails startup.

If the temporary `90-database-path-fix.conf` runtime override was installed during
recovery, deploy the fixed revision first, then remove **only that override** so
the service uses the new Nix-managed launcher (not the pinned temporary binary):

```sh
# As root, after the fixed nixos-rebuild switch succeeds:
mkdir -p -m 0700 /root/maxplayer-reviewer-override-backup
mv /run/systemd/system/maxplayer-reviewer.service.d/90-database-path-fix.conf \
  /root/maxplayer-reviewer-override-backup/90-database-path-fix.conf
systemctl daemon-reload
systemctl restart maxplayer-reviewer.service
systemctl show maxplayer-reviewer.service -p ExecStart
systemctl --no-pager --full status maxplayer-reviewer.service
journalctl -u maxplayer-reviewer.service --since "2 minutes ago" --no-pager
```

Confirm `ExecStart` points into `/nix/store`, the reviewer stays active without new
`review_store` errors, and Buzz remains active. Do not use `systemctl revert`,
which can remove unrelated overrides. Retain the runtime launcher until recovery
is verified; the runtime workaround itself does not survive reboot. A running
service still needs a controlled private-job round trip to prove end-to-end health.

Only credential **paths**, never secret values, are
written to `/run/maxplayer-reviewer/reviewer.json`. The service generates that file
from non-secret Nix settings at startup. SQLite lives at
`/var/lib/maxplayer-reviewer/reviews.sqlite` (including its lock/WAL files); the
private state directory survives restarts and redeploys. Back it up consistently
with SQLite if restoring review-cache/billing history matters; existing relay
Postgres backups do not include it. Do not remove it as part of a normal restart.
The unit's private temporary directory is cleaned when the service stops.

To stage the host without starting reviews, set
`services.maxplayer.reviewer.enable = false` in your deployment configuration.
Standalone hosts importing the module must supply `package`, `relayUrl`,
`signerFile`, and `providerKeyFile`; the module defaults to disabled.

Git deliveries are fetched through the existing buyer smart-HTTP transport. A
`wss://relay.example` configuration permits only canonical
`https://relay.example/git/<owner>/<repo>` URLs on the same host and port.
Credentials in URLs, queries, fragments, alternate protocols, other hosts and
redirects are refused. The reviewer signs Git-read authentication with its own
signing key; give that identity read access to the public job repositories.

The fetch downloads only the advertised delivery branch (no tags) into a fresh,
owner-private temporary bare repository. The fetched tip must equal the commit
in the signed RESULT; a moved branch produces `input_integrity`, not a review of
different content. Existing object/hash/text validation then runs without checkout,
hooks, builds or execution. Scratch data is removed after success or failure.
A process crash can leave scratch directories under the OS temporary directory;
normal host temporary-file cleanup should reclaim them.

Git reads share the request's processing window (`window_seconds`), with a 10-second maximum per
HTTP leg. The aggregate HTTP-response cap uses the shared Git transfer budget: the 5 GiB
uncompressed repository quota plus framing/compression allowance (64 bytes per allowed object
and 64 KiB fixed overhead). Relay upload-pack responses and client private-input
fetches use that budget too; the
retained uncompressed quota is 5 GiB. The transfer object cap uses the shared
1,000,000-object quota.
The existing review input/file limits still apply after fetching. Inaccessible,
missing, oversized, or timed-out fetches produce an error review, never approval.
These bounds can reject large Git histories even when the final files are small.
The relay waits for a successful, nonempty Git subprocess result before serving a
fetch, then streams the completed temporary pack from disk rather than buffering
it in RAM. The Git reverse-proxy upload allowance is 1056 MiB, above the relay's
1 GiB admission ceiling; media upload allowances are unchanged.

An optional `repositories` object still supports operator-managed exact URL-to-local
bare-path overrides, including offline fixtures. Omit it for normal relay fetching;
no per-repository filesystem mappings, shared disk mount, or relay storage changes
are required. Unmapped destinations must pass the relay URL restriction above.

The worker verifies request signatures, reviewer binding, namespace/version, exact
subject, freshness, source signatures, offer/result linkage, and requester access.
Open public offers can be requested by prospective sellers; targeted offers restrict
requests to the buyer/target. Delivery requests require the buyer or result author.
Requests are limited per signed requester (10/minute, bounded identity table).
Relay-level admission controls are still needed against identity churn.

## Provider retries, deduplication, and crashes

The worker has a configurable per-request processing budget — `window_seconds` in the worker JSON, default 600 seconds, accepted range 60..=3600 — including input acquisition. The window is also the worst-case provider spend and head-of-line delay per request, since the worker is serial and fragments stop at the window. Each
provider request has a 30-second HTTP timeout and **at most three attempts**:
initial call plus two retries. Larger review inputs use bounded batches
with at most four concurrent provider requests; the whole batch set shares the
same window deadline. Only
transport failures, HTTP 408/429, and server errors are transient. Other HTTP errors,
invalid JSON/probabilities, and oversized responses stop immediately. Redirects and
hidden HTTP retries are disabled. Backoff honors numeric `Retry-After`; date-form or
invalid values conservatively end the window rather than retry sooner than requested.

The client clock and service clock are independent: queuing/network delay can make a
result arrive after the client times out. The service finishes its bounded attempt,
stores/publishes the result, and an explicit client retry can reuse it. It never
resumes an expired job merely because a review passed.

A bounded intake queue deduplicates active subjects before the single worker processes
them. Concurrent requests share both successful and failed attempts. Later equivalent
requests reuse the persisted signed event, keyed by exact provider-input digest, classifier version, and reviewer.
A process lock prevents two workers from spending against the same database.
Successful results (including unsafe ones) are immutable and reused. Errors are
reused for duplicate request IDs; a new explicit request can retry availability
failures. Request nonces make explicit retries distinct even within one second.
A client retry waits for a fresh response instead of immediately returning an old
cached provider error.

Persistence occurs before publication. A restart can therefore republish the same
terminal event without another provider call. **A crash during the provider call is
indeterminate**: the database retains an in-flight marker and returns
`provider_outcome_unknown` rather than silently billing again. Operator investigation
is required for that case. Exactly-once billing cannot be guaranteed across an
external API without provider idempotency. Conflicting trusted successful results
block; no automatic retry seeks a more permissive probability.

## Exact input and coverage

Git delivery reviews contain the task, subject/event/commit identifiers, pinned base
commit, and a structured diff with three context lines. The base comes from the
verified contribution offer (public tags or resolved private content), never the
seller's chosen parent or a moving branch. It must be available and an ancestor of
the delivered commit. Missing, corrupt or unrelated baselines fail closed. Artifact
jobs without a contribution baseline compare against the empty tree, including all
new files. Renames are represented as deletion plus addition. Mode-only changes
are included. Deleted text is included; unchanged files/history are not sent.
Git objects are read without checkout, hooks, filters, scripts or execution.
Offer and inline reviews retain their existing canonical event input; inline
support must be declared by the offer and validated by the existing parser.

Repository limits reference the same protocol constants as public/private relay
admission and client preflight: **100 MiB per blob, 5 GiB of unique uncompressed
retained Git objects (including history), and 1,000,000 objects**. There are **no
separate file-count or commit-count caps**, including during review tree traversal.
Relay pack upload and compressed repository storage both default to **1 GiB**;
explicit operator overrides remain supported and apply equally to public/private
storage. Reviewer fetches allow repacking/framing overhead above the uncompressed
quota, since a fresh pack need not have the same size as stored packs.

Diff provider requests are spooled into owner-private, unlinked temporary files.
Preparation retains individual blobs/patches and the current request, not copies
of the complete review. Spools are reclaimed when handles close, including after
a crash. Git fetch scratch directories retain the cleanup behavior described above.

These defaults are admission ceilings, **not a guarantee that a near-5-GiB review
finishes within the processing window or a reasonable provider bill**.
Provider requests remain bounded and an incomplete review fails closed. No paid
full-ceiling review has been benchmarked. Changed binary blobs, symlinks and
submodules are unsupported; unchanged files are outside the diff review scope.
Accepting a repository for storage does not assert that every content type has an
available classifier.

Public seller repositories still accumulate multiple job branches; the 1 GiB
compressed-storage ceiling applies to that physical shared repository, not
independently to each job branch. Private jobs have individual repositories. This
change unifies quota values and enforcement, **not repository layout or retention**.
The provisioning response advertises the new limits without a `max_files` field.
Deploy client, relay and reviewer updates together: older clients intentionally
refuse changed provisioning limits rather than assuming unsupported capacity.
Per-account storage budgeting and disk-watermark admission are separate follow-up
work; existing relay operation/cache concurrency controls are preserved.

Signed source events remain bounded at 128 KiB. Provider requests remain bounded
at 128 KiB serialized, with at most 30 KiB of state (conservative UTF-8 byte budget
for TypeSafe's [32k-token state + question limit](https://docs.typesafe.ai/models)).
Git diffs are packed into complete JSON requests with at most 24 KiB of state.
Every request repeats the full task, subject and pinned commit identifiers. Whole
file hunks are kept together when they fit. Oversized hunks split preferentially at
line boundaries (UTF-8 boundaries for oversized lines), repeating old/new paths,
file modes, hunk coordinates and byte offsets on every piece. No diff bytes are
truncated. Small diffs use one request. The full repeated task/context plus a
change fragment must fit in one request;
otherwise review returns an explicit input error rather than dropping task context.
This is a review-context limit, not a lower repository-size quota.
Offer/inline canonical inputs retain the existing UTF-8 fragment path (24 KiB,
1 KiB overlap); this change specifically replaces Git delivery snapshot batching.
Changed non-UTF-8/binary content, symlinks/submodules and unavailable inputs fail
closed. Provider/review responses remain 16 KiB.

An OK review requires a valid response for **every** fragment, all from the same
returned model identity. The aggregate uses the largest `unsafe` probability across
fragments; it is a conservative screening score, **not a calibrated probability for
the repository as a whole**. Independent requests lose global context and cannot
guarantee detection of attacks spanning requests or depending on unchanged code
outside the diff. These reviews remain a
safety signal, not proof that execution is safe. A failed, timed-out, or malformed
fragment produces an error review, never approval based on partial results. Large
reviews make more billable provider calls and can still hit timeouts/rate limits.

Git diff review digests use a new domain-separated, ordered, length-prefixed list
of **all exact provider request bytes**, including instructions, requested model,
task, pinned commit IDs, paths and hunk pieces. Old full-snapshot results cannot
be reused for diff review. Offer/inline digest behavior is unchanged. Thus batching
strategy and any input change invalidate the cache. Completed reviews remain immutable; a crash during a
batch set remains indeterminate and is not silently rebilled. Returned model identity
is recorded in the signed `provider` tag. Configure a pinned model for reproducibility.

The required classifier is `execution-safety`, version `1`. Unknown optional classifiers do not influence it; duplicates, unsupported
versions, invalid probability distributions, or wrong subject/signature fail closed.
A safe classification is not a correctness guarantee, malware/dependency scan, or
execution sandbox. General harmful intent is a separate future classifier.

## Private jobs and remaining release gates

Private jobs use the existing NIP-44/NIP-59 primitive with the separate
`maxplayer-private-review-v1` domain. REVIEW_REQUEST (3409) and REVIEW (3408) are
signed **inner** events, never published directly for private jobs. Only kind-1059
wrappers and recipient routing tags appear on the relay. Subject IDs, text, input
digests, classifier results/probabilities, provider metadata and errors stay encrypted.
Public jobs retain public review messages.

Clients supply the signed offer and commitment-bound task envelope; delivery review
also supplies the signed claim/award/result and answer envelope. The service verifies
these bindings and requester authorization before classifier work. Private Git files
are fetched by exact commit with the existing authenticated, bounded, no-checkout
transport. Host/mint policy comes from deployment configuration, never the request.
Seller keys remain in the signer actor.

Set `review.reviewers[relay]` to the same identity as `privacy.service_pubkey`; the
worker signer must be that service key, which already has content and Git access.
A different key fails closed without adding a recipient or using public transport.
Worker JSON accepts `private_git_base` (default: relay HTTPS `/git/` origin) and
`accepted_mints` (default: the mints that clients use by default, minibits and the
testnut test mint). A list replaces that default, so name each mint that a seller uses.
Systemd exposes
`privateGitBase` and `acceptedMints`. No credentials are generated or committed here.

Results and errors are encrypted separately for authorized buyer, seller/requester
and service recipients. Retry reuses the durable signed assessment and creates fresh
wrappers; old wrapper IDs are not required. Public requests referencing private offers
produce no public error response. Other gift-wrap domains and public results cannot
satisfy private reviews.

Serialized private request bundles are limited to 30 KiB and inner transport messages
to 60 KiB, with the shared ciphertext limit also enforced. Oversized inputs and
saturated recent-response windows fail closed, without truncation or public fallback.
This does not promise flood resistance or review of arbitrarily large repositories.

Before production rollout:

1. Deploy coordinated private-job/reviewer versions and matching identities; exercise
   targeted/open-pool and private Git reviews before production cutover.
2. Follow the [evaluation runbook](evaluations/execution-safety.md) and run labeled TypeSafe evaluation with explicitly approved spend and private credential
   provisioning; choose thresholds from measured false-positive/false-negative rates.
3. Provision the live reviewer signer/provider and deploy relay kind admission plus
   the supervised reviewer service. Verify authenticated Git retrieval and live
   round-trips, then sequence client rollout. Local fixtures are not
   proof of production relay configuration or classifier quality.
4. Exercise operator recovery for indeterminate provider outcomes and conflicting reviews.

No merge, paid calls, production configuration changes, or deployment are performed
merely by building this branch.
