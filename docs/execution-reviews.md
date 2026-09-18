# Execution-safety reviews (mock-stage implementation)

This branch implements the signed review contract and client checks. It does not
install a production reviewer, call Jev, choose a calibrated shipping threshold,
or make the platform ready for deployment. Keep the PR in draft until integration
and deployment sequencing are complete.

## Decisions

1. Extend the existing Maxplayer kind block with `REVIEW` 3408 and `REVIEW_REQUEST`
   3409. Both carry namespace `maxplayer` and major `1`. This is a documented optional
   extension, not a silent change to legacy client behavior. The NIP event table
   and the nostr-protocol registry-of-kinds schema contain no entry for these numbers
   (checked 2026-09-18); this is not a global allocation guarantee.
2. Pin a reviewer public key per exact relay URL in local configuration. A review
   cannot appoint its own signer. A relay owner supplies the key out of band. Do not
   use trust-on-first-use or a seller's signature as reviewer authority.
3. The first classifier ID is `execution-safety`, version `1`. It assesses attempts
   to compromise the receiving buyer/seller environment. Harmful intent is separate.
4. Bound input to 128 KiB including serialized JSON, and at most 256 files. Sort UTF-8
   paths, include their exact text with the immutable subject binding, serialize using
   the shared input builder, and SHA-256 those bytes. Reject unsupported binary data,
   duplicate/traversal paths and oversize input; do not truncate.
5. Main currently supports encrypted payment messages, not a private-job content
   transport. A private review must return an explicit unsupported-transport error
   before any public request. Private job support is deferred to the private-wire
   implementation; no plaintext fallback is permitted. The current live offer/result
   callers process public protocol events only.

## Configuration and migration

Review defaults to enabled, including when a configuration file has no review table.
Without a reviewer key, new claims/acceptances stop with an explicit error. Deploy
an actual reviewer and distribute its key before enabling this branch in production.
Existing accepted payment obligations retain the existing recovery path.

```toml
[review]
seller_offer = true
buyer_delivery = true
skip_buyer_pubkeys = []
skip_seller_pubkeys = []
reject_at_or_above_ppm = 500000
timeout_seconds = 30

[review.reviewers]
"wss://relay.example" = "<64-character-lowercase-hex-reviewer-key>"
```

The threshold uses integer millionths to avoid configuration rounding. `500000`
means 0.50 and is an uncalibrated mock-stage value, not a final shipping decision.
Set `seller_offer=false` or `buyer_delivery=false` to skip that party's review.
Public-key lists support trusted counterparties. Restart after configuration changes.
No provider credential is stored here.

## Wire contract

The request is signed by the requesting buyer or seller. Tags contain `t=maxplayer`,
`v=1`, the root offer (`e` with `root`), exact subject (`e` with `reply`), reviewer `p`,
and `classifier=execution-safety,1`. JSON content contains `offer`, `event`, `kind`,
and `commit`. Offers use their own event ID for both references and a null commit.
Results use the exact result ID and advertised git object ID.

The reviewer validates the request and source signatures and authorization, obtains
only the bounded immutable input, calls its selected classifier, and signs a REVIEW.
The provider service and git snapshot acquisition are not part of this mock stage.
The reviewer must never execute submitted scripts to acquire review input.

REVIEW carries the same root and subject tags. JSON contains `schema=1`, the exact
`subject`, `input_sha256`, `status`, `results`, and `error_code`. Each result contains
`classifier`, `version`, `label`, and `probabilities`. The required classifier supplies
`safe` and `unsafe`, finite values in [0,1], summing to one within 0.000001. The label
must be a maximal probability. Unknown optional classifiers do not influence this
classifier. Duplicate IDs, unsupported required versions, and missing results fail.

An error has `status=error`, empty results and a nonempty error code. It is never a
safe assessment. No raw provider errors, submitted content, or secrets appear in errors.

Clients validate the signature, pinned author, exact root/subject tags, schema and
probabilities. Conflicting trusted results block. The unsafe probability is compared
with the local threshold, inclusively. Provider confidence is not used as probability.

## Client integration

1. Seller: after recording the offer but before claim creation, start a bounded
   review task outside the main event loop. The existing reconsideration tick revisits
   pending offers. There is no slot or invoice reservation while waiting for review.
   Terminal failures remain blocked for this daemon session; restart is the explicit
   retry mechanism in this stage. The in-memory task map is bounded at 256 subjects.
2. Seller execution: check again before starting or resuming the agent. This also
   covers work that predates the new claim check. It does not reopen delivered jobs.
3. Buyer: check the selected delivery before creating the acceptance bind or publishing
   ACCEPT. Both explicit acceptance and collect's implicit acceptance use this path.
   A retry of an already accepted job retains settlement recovery, not a new decision.
4. Save successful review ID, subject, reviewer and effective threshold in a local
   `reviews/<subject-id>.json` audit record before the action. This record is not itself
   trusted as a replacement for signed review verification.
5. The review wait is bounded by configuration and publishes at most one request per
   check. A missing result times out. Explicit buyer retry or seller restart can reuse
   a later result. A cached provider error requests another assessment but keeps the
   current action blocked. Later successful assessments supersede availability errors,
   not conflicting successful classifications. Conflicting classifications require
   reviewer/operator resolution, not repeated attempts to obtain a passing result.

The classifier does not replace sandboxing, git integrity checks, award ownership,
budgets, or payment signatures. Review-before-accept does not protect an application
that runs returned content itself before calling accept. Buyer agent/MCP exposure
must be checked before release so unreviewed content remains data, never instructions.

## Verification

The test transport supplies real signed fixture events without external requests.
Cases cover cached and requested results, timeout/retry, skip, trust lists, private
transport refusal, threshold equality, invalid distributions, optional classifiers,
forged signatures, changed content, and bounded deterministic inputs. No mock is
available as a production provider fallback.

## Remaining release work

1. Live reviewer service, immutable git snapshot collection, and Jev integration.
2. Private-wire integration and operator handling of conflicting classifications.
3. Full buyer content-exposure audit and protection before user-agent consumption.
4. Labeled classifier evaluation, approved paid-testing cap, and calibrated default.

No automatic merge or production deployment is authorized by this implementation.
