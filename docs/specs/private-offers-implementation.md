# Private offers and deliveries — implementation status

Implements the reviewed [product specification](private-offers-and-deliveries.md) and
[wire contract](private-offers-wire-v2.md). **Work in progress, not ready for deployment.**
The integration branch wires targeted/open-pool private jobs and explicit public v2 jobs into the existing buyer/seller loop. All private enablement switches remain off by default; no deployment has occurred. See the [coordinated rollout guide](private-offers-rollout.md).

## Implemented building blocks

- A small shared `maxplayer-private-protocol` crate lives under the vendored relay workspace so
  both clients and the standalone relay package compile the same closed wire schema. It has no
  wallet or network runtime dependency and is Maxplayer-owned, not an upstream Buzz source file.
- Exact-byte salted content commitments; closed JSON/tag/invoice validation; NIP-44/seal/gift-wrap
  transport including sender self-copies; separate payment/content domains and recipient policy.
- SQLite outbox/inbox with atomic recipient persistence, immutable signed carriers, offer-ID
  reservations, conflicts/deduplication, bounded unknown-message staging, restart recovery and
  independently validated service copies. Queries require explicit EOSE before advancing cursors;
  SDK convenience fetches can return partial history on timeout and are not sufficient here.
- V2 receipt bytes and independent Python byte/Schnorr fixtures; immutable signed evidence and
  exact inline envelopes now flow through accept, verification, payment, receipt, collect and
  restart. Existing journals keep their original signature domain. Public v2 inline content uses
  the same salted commitment, without encryption.
- Per-job repository migration, immutable offer/award binding, primary-DB role checks before Git
  hydration (including fast/cache paths), immutable input/delivery refs, terminal write closure,
  no public ref announcements, and bounded cumulative uncompressed-object inspection before CAS.
- Provisioning uses strict API NIP-98 method/URL/body/replay verification. It does not inherit the
  intentional method/replay exceptions of streaming Git authentication. Client redirects are
  disabled; identity signing remains in the existing seller signer actor.
- Client input snapshots and manifest verification use libgit2 objects, not checkout/hooks or
  submodule recursion. Materialization admits only verified regular files into a new directory.
- HTTP router integration tests use disposable PostgreSQL/Redis and a local object-store fixture. They verify buyer/seller/service reads, outsider denial before object-store access after authorized warm reads, anonymous denial despite generic public-read mode, fail-closed flag disablement, and provisioning method/payload/replay checks.
- Public observatory placeholders distinguish targeted-private tasks from intentionally public
  open-pool tasks with private execution/delivery.

## Integrated lifecycle

- MCP/buyer visibility, explicit coarse output category, regular-file input manifests and persisted
  defaults. Disabled/misconfigured private posting fails before publication, never falls public.
- Original tasks/dispatch and bounded pinned inputs are resolved before claim. Selected private
  repository writes are provisioned through the existing award path. Open-pool initial tasks stay
  public; subsequent text is encrypted for buyer, selected seller and Maxplayer.
- Seller claim/result/feedback outbox projection, signer-actor encryption, independent service
  copy retries, authenticated EOSE backfill and restart-safe scan progress. Buyer copy retries
  run independently of active or completed jobs.
- Per-job host and container delivery routes, verified input baselines, contribution base imports,
  inherited checks and isolated Git objects without source credentials/hooks/config.
- Buyer result views require the exact signed claim/award chain and original envelope. Immutable
  evidence, signatures, invoice/mint set, selected result and funding choice survive restart.
  Same-result accept retries reuse the sealed funding choice.
- Public v2 retains public Git/text behavior but shares the exact award, artifact and receipt
  bindings. Its signed evidence has a separate local namespace from authenticated private content.

## Verification and rollout gates

The foundation has 1,803 core and 240 CLI regressions, 18 observatory tests, disposable PostgreSQL
migration/ACL tests, actual HTTP authorization/replay checks and a concurrent quota/CAS test.
The first lifecycle checkpoint passed 1,815 core tests (5 ignored); the subsequent focused run
passed 38 tests including public-inline binding and four-identity targeted/open-pool transport,
reordering and restart. Expanded paid/free tests and final regressions are being run on the
integration branch; this section must not be read as validation of later untested edits.

Before deployment: review the final implementation/CI, prove the assembled client/relay/service
installation with targeted/open-pool paid/free inline/Git jobs, and validate representative
contribution repositories against the bounded history limits. No live mint or production trade
has been used for local verification. Service access is through its designated participant key,
not a newly introduced reviewer role or an application ACK.

## Engineering details

The opaque 64-lowercase-hex Git repository namespace is reserved for private jobs. Missing ACL
records deny access rather than falling through to public hosting. Existing public object graphs
cannot be adopted by provisioning. The default `m<seller-prefix>` public repository names are
unaffected. The provisioning switch defaults off; disabling it never makes existing private
repositories public.

A private push measures the entire quarantined object graph against the 100 MiB cumulative
uncompressed limit (including history), 10 MiB blob limit and 1,000 files per commit. The initial
implementation also bounds inspection to 100,000 objects, 1,000 commits and 30 seconds, and refuses
symlinks and submodules. These limits require representative contribution fixtures before rollout.
The existing manifest-pointer CAS is the publication transaction: two pushes based on one parent
cannot both publish their independently measured graphs. A loser must hydrate the winning parent
and be measured again; failed pushes cannot make partial refs visible. The concurrent-push integration test proves this publication/quota boundary with two overlapping 54 MiB snapshots.

No READY phase, post-award buyer input handoff, application-level service ACK, paid external
service or production probe has been introduced.

### Historical-query failure signaling

Historical REQ database failures close the affected subscription with `CLOSED error:`
and remove its connection/fan-out entries and pubsub reference. No EOSE is emitted
for a failed history, including a failure after earlier filters returned events.
Clients must treat that failure as retryable uncertainty, not proof of absence.
The relay update is required before enabling the lifecycle history-completeness lane.
