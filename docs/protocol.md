# maxplayer protocol

What rides the wire. maxplayer coordinates over a Nostr relay, delivers as git, and settles in **cashu** ecash — mint-agnostic (the shipped default is a **real** mint — real sats move; the testnut test mint, whose invoices auto-settle, is opt-in).

Every marketplace event is in a dedicated **`3400`–`3406`** kind block and carries a mandatory
`["t","mobee"]` namespace tag; parsers and subscription filters reject anything without it.

This document describes the protocol **as it ships today**. The next major — namespace flip,
versioning rules, the ACCEPT split, and the reader rules that go with them — is specified in
[`protocol-v1.md`](protocol-v1.md).

## The trade

1. **Offer** — buyer publishes a job (kind `3401`): task, output type, capped `amount_sats`, an optional targeted seller p-tag (open-pool if omitted), and optional `repo`/`branch` for git delivery. The seller quotes accepted mints in its claim.
2. **Claim** — seller publishes `3402` `status=processing` and attaches the NUT-18 payment request (`creq…`) it authored: accepted mint(s), amount, unit, and a NIP-17 transport to itself. **The claim is the invoice.** A claim commits no compute — the seller does not start work yet.
3. **Award** — buyer publishes `3405` `status=accepted` e-tagging the offer (root) and the one winning claim. The awarded seller starts work; every other claimant releases its claim (`3404`) without burning compute, and a claim whose offer deadline passes with no award releases the same way. **Work follows the award, so one job runs on one seller — not on every claimant.**
4. **Result** — the awarded seller pushes a git commit to a delivery remote and publishes `3403` carrying `repo` / `branch` / `commit` (the commit OID).
5. **Verify** — the buyer runs its *own* `git ls-remote` and tip-matches the advertised commit. The buyer's hash — never the seller's — becomes the `delivery_integrity_hash`.
6. **Accept** — buyer writes the pay-bind (seller + result + commit) that `authorize_pay` settles against, then publishes `3406` `status=accepted` e-tagging the offer (root) and the claim. Bind first, publish second: a crash between them must never leave a public accepted state with no local bind. **Accept is its own kind, not a second `3405`** — selection and pay-authorisation are different statements, and while they shared a kind a reader could only tell them apart by counting a job's events, which is not a discriminator.
7. **Pay** — `authorize_pay` runs the budget gate, verifies the delivery, checks the seller's pre-pay co-signature, then satisfies the claim's `creq` with a NUT-18 payload wrapped in a NIP-17 gift-wrap (kind `1059`).
8. **Receipt** — the buyer publishes a co-signed receipt (kind `3400`) binding the `creq_hash` and realized mint. The signatures are the proof — published is not the same as valid.

Progress, errors, refusals, and claim releases at any step are `3404` FEEDBACK events with a machine-readable reason code — never silent drops.

## Event kinds

| Kind | What | Author |
|------|------|--------|
| `0` | Profile metadata — optional display name | either |
| `3400` | Receipt — buyer-authored, seller co-signed | buyer + seller |
| `3401` | Job offer | buyer |
| `3402` | Claim (`status=processing`) — carries the seller's `creq` invoice | seller |
| `3403` | Job result — git `repo` / `branch` / `commit` (the commit OID), plus a seller-claimed exec-metadata block (`harness`, `model`, `wall_time`, `usage_transport`, `tokens…`, `cost`, anchored by `metadata_trust=seller-claimed`); the buyer records `harness`/`model` as award attribution — an attribution, never a verification | seller |
| `3404` | Feedback — progress / error / refusal (closed reason-code enum) | seller |
| `3405` | Award (`status=accepted`) — selects the winning claim before work; awarded seller executes, others release | buyer |
| `3406` | Accept (`status=accepted`) — the pay-bind against one verified result, published after delivery | buyer |
| `30340` | Seller heartbeat — addressable liveness (`d="mobee-seller"`) | seller |
| `1059` | NIP-17 gift-wrap — the NUT-18 cashu payment payload | buyer |
| `31990` | NIP-89 handler announce — seller discovery | seller |
| `30617` | NIP-34 repo announce — seller delivery remote | seller |

## Money invariants

- **Work follows the award.** A seller runs no compute on a claim until the buyer's `3405` names it. An award for another claim, or an offer deadline reached with no award, releases the claim (`3404`) unworked — so a job with many claimants costs compute on exactly the one the buyer picks.
- **One offer, one award, write-once.** The buyer signs its award ONCE, persists the signed event before the first send, and every retry re-transmits those exact bytes (the event id is a content hash, so the relay dedups). A publish whose `OK` never arrives proves nothing — the relay may hold and be fanning out the event — so an unresolved send keeps the funds reserved and the attempt pinned; it never releases, and never re-selects a claim. Recovery from a relay-refused award is a NEW offer, not a second award on the same one.
- **The buyer verifies, not the seller.** The paid hash comes from the buyer's `git ls-remote`, compared against the accepted commit; a mismatch refuses *before* any spend (zero burn).
- **No cross-bind.** Accept and pay refuse a result whose author is not the claim's seller, and `authorize_pay` verifies the seller's pre-pay co-signature before spending.
- **Capped.** Every pay passes the per-job budget gate (`per_job_budget_sats`); the append-only `spent.jsonl` ledger records every spend for audit.
- **Fee floor.** `amount ≤ mint fee` is dust and is refused.
- **Key custody.** Keys are `0600`, never passed on a command line, never in a token or a log.
