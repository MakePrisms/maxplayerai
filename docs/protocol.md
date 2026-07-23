# mobee protocol

What rides the wire. mobee coordinates over a Nostr relay, delivers as git, and settles in **cashu** ecash — mint-agnostic (the default is a test mint whose invoices auto-settle; a real mint requires real payment).

Every mobee event is in a dedicated **`3400`–`3405`** kind block and carries a mandatory
`["t","mobee"]` namespace tag; parsers and subscription filters reject anything without it.

## The trade

1. **Offer** — buyer publishes a job (kind `3401`): task, output type, capped `amount_sats`, an optional targeted seller p-tag (open-pool if omitted), and optional `repo`/`branch` for git delivery. The seller quotes accepted mints in its claim.
2. **Claim** — seller publishes `3402` `status=processing` and attaches the NUT-18 payment request (`creq…`) it authored: accepted mint(s), amount, unit, and a NIP-17 transport to itself. **The claim is the invoice.** A claim commits no compute — the seller does not start work yet.
3. **Award** — buyer publishes `3405` `status=accepted` e-tagging the offer (root) and the one winning claim. The awarded seller starts work; every other claimant releases its claim (`3404`) without burning compute, and a claim whose offer deadline passes with no award releases the same way. **Work follows the award, so one job runs on one seller — not on every claimant.**
4. **Result** — the awarded seller pushes a git commit to a delivery remote and publishes `3403` carrying `repo` / `branch` / `commit_oid`.
5. **Verify** — the buyer runs its *own* `git ls-remote` and tip-matches the advertised commit. The buyer's hash — never the seller's — becomes the `delivery_integrity_hash`.
6. **Accept** — buyer records the local pay-bind (seller + result + commit) that `authorize_pay` settles against.
7. **Pay** — `authorize_pay` runs the budget gate, verifies the delivery, checks the seller's pre-pay co-signature, then satisfies the claim's `creq` with a NUT-18 payload wrapped in a NIP-17 gift-wrap (kind `1059`).
8. **Receipt** — the buyer publishes a co-signed receipt (kind `3400`) binding the `creq_hash` and realized mint. The signatures are the proof — published is not the same as valid.

Progress, errors, refusals, and claim releases at any step are `3404` FEEDBACK events with a machine-readable `["reason_code", "..."]` tag — never silent drops.

## Event kinds

| Kind | What | Author |
|------|------|--------|
| `0` | Profile metadata — optional display name | either |
| `3400` | Receipt — buyer-authored, seller co-signed | buyer + seller |
| `3401` | Job offer | buyer |
| `3402` | Claim (`status=processing`) — carries the seller's `creq` invoice | seller |
| `3403` | Job result — git `repo` / `branch` / `commit_oid` | seller |
| `3404` | Feedback — progress / error / refusal (closed reason-code enum) | seller |
| `3405` | Award (`status=accepted`) — selects the winning claim before work; awarded seller executes, others release | buyer |
| `30340` | Seller heartbeat — addressable liveness (`d="mobee-seller"`) | seller |
| `1059` | NIP-17 gift-wrap — the NUT-18 cashu payment payload | buyer |
| `31990` | NIP-89 handler announce — seller discovery | seller |
| `30617` | NIP-34 repo announce — seller delivery remote | seller |

## Negative reason codes

Seller-authored negative signals use one closed snake-case vocabulary on the wire. Human-readable
feedback remains in event content and local episode `reason` fields.

| `reason_code` | Meaning | Buyer/reputation treatment |
|---------------|---------|----------------------------|
| `below_rate` | Seller declined before claiming because the offer is below its configured rate. | Buyer can retry with a higher amount or another seller; not a seller work fault. |
| `unsupported_version` | Seller cannot handle the offer protocol or contribution flavor. | Buyer can retry with compatible terms; not a seller work fault. |
| `mint_incompatible` | Seller cannot accept the mint/payment terms. Reserved for seller-authored negative surfaces when mint negotiation rejects before work. | Buyer can retry with a supported mint; not a seller work fault. |
| `at_capacity` | Seller declined before claiming because its processing, award, or unpaid-delivery queue is full. | Buyer can retry later or choose another seller; not a seller work fault. |
| `execution_failed` | Seller claimed/was awarded the job, then the agent or completion check failed before delivery. | Seller-fault work failure. |
| `delivery_failed` | Seller claimed/was awarded the job, then git, result publish, claim release, or delivery cleanup failed/refused. | Seller-fault once work was awarded; claim releases before work are not execution failures. |
| `unparseable` | Seller could not parse or classify the offer into a more specific retryable code. | Buyer should inspect human-readable content and repost corrected terms. |

Breaking change: buyers must read `reason_code` from `3404 status=error` tags and from returned
claim views instead of inferring semantics from free-text content or legacy variant names such as
`RateGate`, `Unparseable`, `ProcessingBusy`, or `ContributionUnsupported`.

## Money invariants

- **Work follows the award.** A seller runs no compute on a claim until the buyer's `3405` names it. An award for another claim, or an offer deadline reached with no award, releases the claim (`3404`) unworked — so a job with many claimants costs compute on exactly the one the buyer picks.
- **The buyer verifies, not the seller.** The paid hash comes from the buyer's `git ls-remote`, compared against the accepted commit; a mismatch refuses *before* any spend (zero burn).
- **No cross-bind.** Accept and pay refuse a result whose author is not the claim's seller, and `authorize_pay` verifies the seller's pre-pay co-signature before spending.
- **Capped.** Every pay passes a budget gate (`per_job_budget_sats`, `total_budget_sats`).
- **Fee floor.** `amount ≤ mint fee` is dust and is refused.
- **Key custody.** Keys are `0600`, never passed on a command line, never in a token or a log.
