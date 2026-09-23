# Execution-safety reviews

Implementation PR #1022 extends proposal #1021. This branch now contains the public
review worker, TypeSafe adapter, signed client gates, operator recovery, and local
integration tests. **It is not yet approved for production deployment.** No paid
classifier evaluation or default-threshold calibration has been performed.

## What runs where

- Buyer/seller clients request `REVIEW_REQUEST` (3409), verify `REVIEW` (3408), and
  apply their own unsafe-probability threshold. A skip affects only that client.
- The relay owner runs `maxplayer review serve reviewer.json`. The worker fetches
  signed public subjects from that one configured relay, reads existing local bare
  Git stores, calls TypeSafe, persists a signed terminal result, and publishes it.
- The worker uses the provider's existing HTTP API, not custom model inference.
  API contract: <https://docs.typesafe.ai/introduction/quickstart>.
- Clients never receive provider credentials. Review signatures do not replace
  Git verification, claim/award eligibility, payment signatures, budgets, or pay-once.

## Client settings

```toml
[review]
seller_offer = true
buyer_delivery = true
skip_buyer_pubkeys = []
skip_seller_pubkeys = []
reject_at_or_above_ppm = 500000
timeout_seconds = 30

[review.reviewers]
"wss://relay.example" = "<64-character-lowercase-hex-reviewer-public-key>"
```

`500000` means unsafe probability >= 0.50 blocks. This remains **uncalibrated**, not
an evidence-backed shipping threshold. No reviewer key is learned from a response.
Without a configured key, default-enabled reviews stop new claims/acceptances with
an explicit error. Distribute keys and deploy the reviewer before enabling clients.
Changing configuration still requires restarting the client/daemon.

Explicit skips use the role flags or public-key lists, never display names. Local
status records identify the subject and explain configuration/counterparty skips.
A pass also records reviewer identity, event ID, and effective policy in
`reviews/<subject-id>.json`. These local records are audit evidence, not substitutes
for verifying signed reviews on the next check.

## Timeouts and actual retry actions

A client wait defaults to 30 seconds. **A timeout ends that review wait, not the job.**
It does not claim, accept, reject, pay, bypass the check, or extend job deadlines.

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
  "repositories": {
    "https://relay.example/git/<owner>/<repo>.git": "/existing/job-store/<repo>.git"
  }
}
```

Secret files must be regular owner-private files (0600 or stricter). Provision a
dedicated signer and distribute its **public** key to clients. Never put credentials
in command arguments, public events, logs, or this repository. The database parent
must exist and be writable. Run the service under an operator-managed supervisor.
Only map **public job repositories**; private jobs are not supported yet.

```sh
maxplayer review serve /etc/maxplayer/reviewer.json
```

The repository mapping is deliberate: an untrusted result cannot make the reviewer
fetch an arbitrary URL, access the network internally, or read an arbitrary path.
The initial worker must be colocated with the existing bare job store (or have a
read-only mount of it). An unmapped/inaccessible repository produces an error. It
does not download and persist another source-content copy. Arbitrary third-party
Git hosting is not supported by this initial acquisition adapter.

The worker verifies request signatures, reviewer binding, namespace/version, exact
subject, freshness, source signatures, offer/result linkage, and requester access.
Open public offers can be requested by prospective sellers; targeted offers restrict
requests to the buyer/target. Delivery requests require the buyer or result author.
Requests are limited per signed requester (10/minute, bounded identity table).
Relay-level admission controls are still needed against identity churn.

## Provider retries, deduplication, and crashes

The worker has a 30-second processing budget including input acquisition. It makes
**at most three provider attempts total**: initial call plus two retries. Only
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

Canonical JSON contains the subject, full verified offer, full verified result when
present, and a sorted path manifest. Each manifest item contains a SHA-256 byte hash
and exact UTF-8 text. Git data comes from the advertised immutable commit, not branch
HEAD or a worktree. No hooks, scripts, build steps, filters, or delivered code run.
Inline deliveries are supported only when the offer declares inline support and the
RESULT passes the existing inline parser; their signed event ID binds the text.

Limits: 128 KiB source JSON and serialized provider request, 256 files, 4096 tree entries, 4096-byte paths, 16 KiB
provider/review response. Symlinks, submodules, non-UTF-8 paths/content, binary blobs,
inaccessible objects, and incomplete/oversized input fail closed. No truncation.
The input digest covers **the actual serialized provider request**, including the
classifier instructions and requested model, not only the file list. The returned
model identity is recorded in the signed `provider` tag. `jev-latest` is an alias,
not pinned weights; configure a pinned provider model for reproducible evaluation.

The required classifier is `execution-safety`, version `1` (this draft has not
shipped). Unknown optional classifiers do not influence it; duplicates, unsupported
versions, invalid probability distributions, or wrong subject/signature fail closed.
A safe classification is not a correctness guarantee, malware/dependency scan, or
execution sandbox. General harmful intent is a separate future classifier.

## Private jobs and remaining release gates

Private-job content transport is not implemented on current upstream. The client
rejects private requests before any public fallback. This branch does not invent a
second transport or publish private roots/metadata. Integrating private review with
the shared private-wire design remains a **blocking external dependency**.

Before production rollout:

1. Integrate and exercise the shared private-job transport before claiming private support.
2. Follow the [evaluation runbook](evaluations/execution-safety.md) and run labeled TypeSafe evaluation with explicitly approved spend and private credential
   provisioning; choose thresholds from measured false-positive/false-negative rates.
3. Provision the live reviewer signer/provider and relay kind admission, map the public
   Git store, verify live round-trips, and sequence client rollout. Local fixtures are not
   proof of production relay configuration or classifier quality.
4. Exercise operator recovery for indeterminate provider outcomes and conflicting reviews.

No merge, paid calls, production configuration changes, or deployment are performed
merely by building this branch.
