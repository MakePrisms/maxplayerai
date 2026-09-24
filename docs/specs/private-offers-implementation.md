# Private offers and deliveries — implementation status

Implements the reviewed [product specification](private-offers-and-deliveries.md) and
[wire contract](private-offers-wire-v2.md). **Work in progress, not ready for deployment.**
The existing buyer/seller trade loop still publishes protocol v1. No private posting is enabled.

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
- V2 receipt bytes and independent Python byte/Schnorr fixtures. These do **not** yet replace the
  existing payment-path producer/verifier; that cutover must happen together.
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

## Remaining integration (required before enablement)

- Coordinated v2 producers/parsers across every lifecycle kind, including explicit public jobs.
- Buyer/seller configuration, CLI/MCP visibility and input-file surfaces; no silent public fallback.
- Persistent live content workers and signed-event context resolution in the buyer/seller flows.
- Targeted task/input availability before claim; private claim capabilities; open-pool selection;
  private execution/results/feedback/rejection; repository setup on the existing delivery path.
- Contribution input/history handling, container delivery and new-offer follow-ups.
- Inline salted commitment and Git OID binding through accept, verify, pay-once, receipt, collect
  and restart. Preserve existing budget, invoice, mint, signature and result-specific bind gates.
- End-to-end four-identity, paid/free/public, outage/reordering/restart and HTTP ACL/cache tests.
- Service consumer deployment wiring, coordinated cutover instructions and documentation updates.

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
and be measured again; failed pushes cannot make partial refs visible. This must also be proved
in the concurrent-push integration test, not just inferred from unit quota tests.

No READY phase, post-award buyer input handoff, application-level service ACK, paid external
service or production probe has been introduced.

### Historical-query failure signaling

Historical REQ database failures close the affected subscription with `CLOSED error:`
and remove its connection/fan-out entries and pubsub reference. No EOSE is emitted
for a failed history, including a failure after earlier filters returned events.
Clients must treat that failure as retryable uncertainty, not proof of absence.
The relay update is required before enabling the lifecycle history-completeness lane.

Historical query results also fail closed if a stored event cannot be decoded.
Rows consumed by LIMIT must not silently disappear during database deserialization.
An isolated PostgreSQL regression covers a malformed newer row hiding a valid older
event, whole-result rejection, and recovery after the malformed row is removed.
