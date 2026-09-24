# Execution-safety reviews

Implementation PR #1022 extends proposal #1021. The consolidated private-jobs implementation also integrates the
review worker, TypeSafe adapter, signed client gates, operator recovery, and local
integration tests. **It is not yet approved for production deployment.** No paid
classifier evaluation or default-threshold calibration has been performed.

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
  "model": "jev-latest"
}
```

Secret files must be regular owner-private files (0600 or stricter). Provision a
dedicated signer and distribute its **public** key to clients. Never put credentials
in command arguments, public events, logs, or this repository. The database parent
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

- `/var/lib/secrets/maxplayer-reviewer-signing-key`: a dedicated persistent Nostr
  secret key (raw 64-character hex or supported Nostr secret encoding), **not** a
  `NAME=value` environment file and not the relay's own key.
- `/var/lib/secrets/typesafe-api-key`: the raw TypeSafe API key.

Distribute only the reviewer's corresponding public key to clients. Ensure this
identity can authenticate and read public Git repositories on the relay. Do not
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

Systemd `LoadCredential` supplies read-only private copies of the two secrets to a
dedicated dynamic service user. Only credential **paths**, never secret values, are
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

Git reads share the request's 30-second deadline, with a 10-second maximum per
HTTP leg, a 32 MiB aggregate HTTP-response cap and a 100,000-object transfer cap.
The existing review input/file limits still apply after fetching. Inaccessible,
missing, oversized, or timed-out fetches produce an error review, never approval.
These bounds can reject large Git histories even when the final files are small.

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
`accepted_mints` (default: standard mint); match the clients and relay. Systemd exposes
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
