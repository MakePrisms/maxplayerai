# Buyer mint choice: credits first, one rule for public and private jobs

**Status:** implementation draft authorized by bob. Josip sign-off pending; do not merge. **Anchor commit:
`9ba25cda0206b194e6c81d983108eb7ef857f505`** (= `origin/main` = `v0.6.1-rc6`). Citations are
`file:line @9ba25cd`.

**Supersedes** the earlier draft of this file (the `["param","mints",…]` offer tag). bob, #buyer-mint-choice,
2026-10-05:
- The real problem is mints that can't receive a hop (`nostr://` credit mints).
- Public and private jobs follow the same mint rule (option 1: today's public rule everywhere).
- When the buyer holds enough at more than one mint the seller accepts, **credits are spent first**. No
  per-job `pay_from` option.

No offer tag, no protocol change, no relay change, no buyer-store migration, no new bind field.

## 1. The rule

For a priced job, given the seller's creq mint list:

1. **Pick the source mint** (`select_source_mint`, `crossmint.rs:171`), from mints in the buyer's wallet
   (`accepted_mints[0]` + `extra_mints`) that pass `allow_real_mints` and hold **the full price**:
   1. the first `nostr://` mint in the seller's list the buyer holds enough at (credits);
   2. else the first `https://` mint in the seller's list the buyer holds enough at;
   3. else the buyer's default mint.
2. **Plan** (`plan_payment`, `crossmint.rs:94`):
   - source in the seller's list → pay direct;
   - source is `nostr://` and not listed → refuse (credits never melt; unchanged, `:126`);
   - otherwise hop to the first mint in the **seller's order** that passes the fence **and is not
     `nostr://`**; none → refuse.
3. **Same rule for private jobs.** A private claim no longer has to list only mints the buyer approved.

Payments never split across mints.

## 2. Cases

C = seller's credit mint (`nostr://`), L = Lightning mint the seller lists, X = Lightning mint the buyer
uses that the seller doesn't list. "Enough" = the full price at one mint. Credits count only if the buyer
added C with `maxplayer wallet mints add` (otherwise the row is `configured=false` and ignored,
`crossmint.rs:200`). Any seller accepting C takes them, not just the issuer.

| Seller lists | Buyer holds | Result | Today |
|---|---|---|---|
| C + L (any order) | enough at C and at L | credits, direct | seller's first mint wins |
| C + L | enough at C only | credits, direct | same |
| C + L | enough at L only | sats at L, direct | same |
| C + L | sats at X (default) | hop X→L, buyer pays the fee | **C listed first: hop to C, fails after delivery** |
| C + L | some credits (< price) + sats at X | hop X→L, credits untouched | same bug if C first |
| C + L | not enough anywhere | refused at award (ceiling) | same |
| C only | enough at C | credits, direct | same |
| C only | sats only | **refused at award** | awarded, fails after delivery |
| L only | at L / at X | direct / hop X→L | same |
| L only | credits only | refused at award | same |
| several L + C | sats at X | hop to first `https://` in seller order | could hop to C |

Private jobs: identical. Today a private claim listing any mint not in the buyer's `accepted_mints` is
dropped silently (§3).

## 3. Current behavior being changed

- **Hop target can be `nostr://`.** `plan_payment` takes the first fence-admitted mint
  (`crossmint.rs:134-140`) with no scheme check. The sidecar disables NUT-04/05
  (`maxplayer-mint/src/dispatch.rs:28-35`), so the hop fails at the target mint quote
  (`crossmint_hop.rs:917`) after delivery.
- **Source preference follows seller order** (`crossmint.rs:171`): a buyer holding both credits and sats
  spends whichever the seller listed first.
- **Award filter ignores balances.** `claim_is_settleable` plans from the config default
  (`buyer/lifecycle.rs:658`; filters built at `:94`) while the ceiling (`buyer/mod.rs:1453`) and accept
  (`job_lifecycle.rs:1465`) use `select_source_mint`. With the `nostr://` skip alone, a buyer holding
  credits at an `extra_mints` mint would be refused by a credits-only seller. The filter must use the
  same source choice.
- **Private all-mints check.** `invoice::validate` requires every creq mint in the buyer's
  `accepted_mints` (`private_content/invoice.rs:111-117` via `HostPolicy::mint`,
  `maxplayer-private-protocol/src/wire.rs:102`); `extra_mints` don't count (`runtime.rs:26`, `:51`).
  A failing claim is dropped by `continue` (`job_lifecycle.rs:3738`). This is the private mechanism of
  #1069. #1092 kept it only because a hop could land on an unapproved mint
  (`private_content/tests.rs:1721`). Under option 1 that's accepted, as it already is for public jobs.
- **Private receipt mint check.** A kind-3400 receipt's `mint` is checked with the same
  `HostPolicy::mint` (`wire.rs:420`). Under option 1 the realized mint can be a seller-listed mint the
  buyer never added, so this check must change too. (Correction to an earlier chat answer that said it
  could stay.)

## 4. Changes (one PR)

- `crossmint.rs`
  - `select_source_mint`: two passes, `nostr://` first, then `https://`, each in seller order.
  - `plan_payment`: skip `nostr://` hop targets; refusal names the list.
- `buyer/lifecycle.rs`: `AwardFilters` carries the balance snapshot; `claim_is_settleable` plans from
  `select_source_mint(...)`. A failed balance read falls back to the default mint (today's filter).
- `buyer/mod.rs`: both `award_filters_for_offer` callers (`:950`, `:1625`) pass the balances already read
  for the ceiling. Park/refusal text says when the only route was a credit-mint hop.
- `maxplayer-private-protocol/src/wire.rs`: split `HostPolicy::mint` into a well-formedness check
  (HTTPS rules or canonical `nostr://npub`, ≤2048 bytes) and the approval check. Claim `creq` mints and
  the receipt `mint` use well-formedness only.
- `private_content/invoice.rs`: keep count (1..=32), no duplicates, well-formed; drop membership.
- `private_content/evidence.rs`: check the receipt / sealed realized mint is in the **signed claim's**
  creq list (the set comparison at `:171` covers the list; add the realized-mint membership if it's not
  already implied — confirm in the PR).
- `reviewer.rs`: no logic change, but it validates private invoices with its own mint list (`:59`), so the
  reviewer service must run the new code to see these jobs.
- `job_lifecycle.rs`: #1039 — a same-result re-accept keeps the stored `funding_mint`/`delivery_mint`.
  Included because credits-first makes a different re-pick more likely.

`HostPolicy.accepted_mints` is then unused by validation; leave the field for now.

## 5. Pays-once

- New selections: the source is chosen once at award (reservation pin) and sealed at accept
  (`job_lifecycle.rs:1863`); pay re-plans from the sealed mints (`authorize_pay.rs:1064`). Credits-first
  only changes what is chosen, not when.
- Ceiling, filter and accept call the same `select_source_mint`, so they can't diverge (extend the parity
  test at `buyer/mod.rs:3543`).
- In-flight binds sealed before upgrade with a hop to `nostr://`: after upgrade they re-plan to the next
  `https://` target, which is a new attempt id. Safe because the old attempt failed at the target mint
  quote, before any melt and before the hop journal was written (`crossmint_hop.rs:906-927`). The PR
  adds a test pinning that no journal or spend exists for that failure.
- #1039 closes the re-accept hole.

## 6. Tests

- `select_source_mint`: credits beat sats regardless of seller order; insufficient credits fall through to
  sats; unconfigured credit rows ignored; fence still applies.
- `plan_payment`: `nostr://` target skipped wherever it sits; credits-only seller + sats-only buyer
  refused; existing direct/hop cases unchanged.
- Award filter: credits held at an `extra_mints` mint against a credits-only seller is awardable; manual
  and auto paths identical (extend `lifecycle.rs:4577`).
- Private: invert `tests.rs:1721`; a `[approved, unknown]` (#1069-shaped) claim is visible and
  awardable; all-unknown well-formed list accepted; malformed / duplicate / >32 refused; receipt with a
  realized mint outside the signed creq refused.
- Pays-once: pre-upgrade bind with a `nostr://` hop target re-plans without double spend; same-result
  re-accept keeps mints and attempt id (#1039).
- Mutation checks (each must go red): remove the `nostr://` target skip; revert to seller-order source;
  filter back to default-only planning; drop the receipt-in-creq check.
- Money-path suite with a real sidecar credit mint: credits direct; seller `[C, L]` + buyer at X hops to L.

## 7. Rollout

- Client-only release; no relay deploy, no migration.
- The reviewer service must be redeployed in the same window, or it keeps ignoring private review
  requests whose claims list mints outside its list (as today).
- Mixed versions: an old buyer keeps today's behavior (including the bug); an old seller is unaffected
  (it only ever receives at its own listed mints, checked against its stored creq, `seller_node/run.rs:9777`).

## 8. Not in scope / known gaps

- **Hop source is always the default mint.** A buyer whose sats sit only at a non-default `extra_mints`
  mint the seller doesn't list is refused at award. Follow-up: hop from the default if it covers, else the
  first other `https://` wallet mint that does.
- **#1069 public repro** isn't explained by this trace; the private mechanism is fixed here.
- **Future Lightning-backed `nostr://` mint** would be skipped as a hop target by scheme. Revisit when one
  exists (e.g. read NUT-04 from mint info).
- **Trust widening for private jobs** is the deliberate cost of option 1: a hop may pay into any
  `https://` mint the seller lists, as public jobs already do.
